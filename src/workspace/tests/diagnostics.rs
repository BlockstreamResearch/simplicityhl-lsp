use simplicityhl::error::{
    Diagnostic as CompilerDiagnostic, DiagnosticManager, Error as CompilerError, Span,
};
use simplicityhl::parse::{self, ParseFromStrWithErrors};
use simplicityhl::UnstableFeatures;
use tower_lsp_server::lsp_types::{Position, Range};

use super::Project;
use crate::analysis::AnalysisSnapshot;
use crate::workspace::diagnostics::DiagnosticBundle;

#[test]
fn imported_diagnostic_is_owned_by_its_real_source() {
    let project = Project::new();
    let library = project.file("library.simf", "pub fn broken() -> (u1, u256) { 0 }\n");
    let root = project.file(
        "main.simf",
        "use crate::library::broken;\nfn main() { broken(); }\n",
    );

    let bundle = DiagnosticBundle::from_snapshot(&project.analyze(&root.path, &root.source));

    let imported = bundle
        .get(&library.uri)
        .and_then(|diagnostics| {
            diagnostics
                .iter()
                .find(|diagnostic| diagnostic.message.contains("Expected expression"))
        })
        .unwrap_or_else(|| panic!("expected imported diagnostic, got {bundle:?}"));

    assert_ne!(imported.range, Range::default());
    assert!(!bundle.get(&root.uri).is_some_and(|diagnostics| {
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == imported.message)
    }));
}

#[test]
fn cross_file_secondary_labels_notes_and_help_are_preserved() {
    let project = Project::new();
    project.file("library.simf", "pub fn helper() {}\n");
    let root = project.file(
        "main.simf",
        "use crate::library::helper;\nfn main() { helper(); }\n",
    );

    let mut snapshot = project.analyze(&root.path, &root.source);
    let imported_file_id = snapshot
        .functions
        .get_func("helper")
        .expect("imported function")
        .span()
        .file_id;
    assert_ne!(imported_file_id, 0);

    snapshot.compiler_diagnostics = vec![CompilerDiagnostic::new(
        CompilerError::CannotParse {
            msg: "primary".to_string(),
        },
        Span::new(0, 0..3),
    )
    .with_secondary(Span::new(imported_file_id, 7..13), "secondary")
    .with_note("context")
    .with_help("fix it")];

    let bundle = DiagnosticBundle::from_snapshot(&snapshot);
    let published = &bundle
        .get(&snapshot.sources[0].uri)
        .expect("root diagnostics")[0];

    assert_eq!(
        published.range,
        Range::new(Position::new(0, 0), Position::new(0, 3))
    );
    assert!(published.message.contains("Note: context"));
    assert!(published.message.contains("Help: fix it"));

    let related = published
        .related_information
        .as_ref()
        .expect("secondary related information");
    assert_eq!(related.len(), 1);
    assert_eq!(related[0].message, "secondary");
    assert_eq!(
        related[0].location.uri,
        snapshot.sources[imported_file_id].uri
    );
    assert_eq!(
        related[0].location.range,
        Range::new(Position::new(0, 7), Position::new(0, 13))
    );
}

fn document_from_source(source: &str) -> AnalysisSnapshot {
    let mut diagnostics = DiagnosticManager::new();
    let program = parse::Program::parse_from_str_with_errors(
        0,
        source,
        &UnstableFeatures::none(),
        &mut diagnostics,
    )
    .unwrap_or_else(|| panic!("source should parse: {diagnostics:?}"));

    let path = std::env::temp_dir().join("semantic_tokens.simf");
    AnalysisSnapshot::from_program(&program, source, &path)
}

#[test]
fn lsp_diagnostics_preserve_secondary_labels_notes_and_help() {
    let source = "fn main() {}\n";
    let mut document = document_from_source(source);
    let diagnostic = CompilerDiagnostic::new(
        CompilerError::CannotParse {
            msg: "primary".to_string(),
        },
        Span::new(0, 3..7),
    )
    .with_secondary(Span::new(0, 0..2), "secondary")
    .with_note("context")
    .with_help("fix it");
    document.compiler_diagnostics = vec![diagnostic];
    let bundle = crate::workspace::diagnostics::DiagnosticBundle::from_snapshot(&document);
    let uri = &document.sources[0].uri;
    let published = &bundle.get(uri).expect("root diagnostic")[0];

    assert!(published.message.contains("Note: context"));
    assert!(published.message.contains("Help: fix it"));
    let related = published
        .related_information
        .as_ref()
        .expect("secondary label becomes related information");
    assert_eq!(related.len(), 1);
    assert_eq!(related[0].message, "secondary");
    assert_eq!(related[0].location.uri, document.sources[0].uri);
}
