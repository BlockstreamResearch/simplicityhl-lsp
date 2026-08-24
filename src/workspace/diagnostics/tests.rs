use simplicityhl::error::Span;
use tempfile::TempDir;
use tower_lsp_server::lsp_types::Position;
use tower_lsp_server::UriExt;

use super::*;
use crate::config::Settings;

#[test]
fn imported_diagnostic_is_owned_by_its_real_source() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    std::fs::write(root.join("Simplex.toml"), "").unwrap();
    std::fs::create_dir(root.join("simf")).unwrap();
    let library_path = root.join("simf/library.simf");
    std::fs::write(&library_path, "pub fn broken() -> (u1, u256) { 0 }\n").unwrap();
    let source = "use crate::library::broken;\nfn main() { broken(); }\n";
    let root_path = root.join("simf/main.simf");
    std::fs::write(&root_path, source).unwrap();
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .unwrap();
    let snapshot = AnalysisSnapshot::analyze(source, &root_path, &settings, &[root.to_path_buf()]);
    let bundle = DiagnosticBundle::from_snapshot(&snapshot);
    let library_uri = Uri::from_file_path(std::fs::canonicalize(library_path).unwrap()).unwrap();
    let root_uri = Uri::from_file_path(std::fs::canonicalize(root_path).unwrap()).unwrap();

    let imported = bundle
        .get(&library_uri)
        .and_then(|diagnostics| {
            diagnostics
                .iter()
                .find(|diagnostic| diagnostic.message.contains("Expected expression"))
        })
        .unwrap_or_else(|| panic!("expected imported diagnostic, got {bundle:?}"));
    assert_ne!(imported.range, Range::default());
    assert!(!bundle.get(&root_uri).is_some_and(|diagnostics| {
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == imported.message)
    }));
}

#[test]
fn cross_file_secondary_labels_notes_and_help_are_preserved() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    std::fs::write(root.join("Simplex.toml"), "").unwrap();
    std::fs::create_dir(root.join("simf")).unwrap();
    let library_path = root.join("simf/library.simf");
    std::fs::write(&library_path, "pub fn helper() {}\n").unwrap();
    let source = "use crate::library::helper;\nfn main() { helper(); }\n";
    let root_path = root.join("simf/main.simf");
    std::fs::write(&root_path, source).unwrap();
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .unwrap();
    let mut snapshot =
        AnalysisSnapshot::analyze(source, &root_path, &settings, &[root.to_path_buf()]);
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
    let published = &bundle.get(&snapshot.sources[0].uri).unwrap()[0];

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
