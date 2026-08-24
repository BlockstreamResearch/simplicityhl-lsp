use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc as StdArc, Mutex as StdMutex};

use ropey::Rope;
use simplicityhl::error::{
    Diagnostic as CompilerDiagnostic, DiagnosticManager, Error, Location as CompilerLocation, Span,
};
use simplicityhl::parse::{self, ParseFromStrWithErrors};
use simplicityhl::UnstableFeatures;
use tempfile::TempDir;
use tokio::sync::Notify;
use tower_lsp_server::lsp_types::{Range, Uri};
use tower_lsp_server::UriExt;

use super::*;
use crate::analysis::AnalysisSnapshot;
use crate::config::Settings;
use crate::text::{offset_to_position, span_to_positions};
use crate::workspace::{DiagnosticUpdate, WorkspaceState};

/// `parse_program` resolves imports from the project the file lives in, so tests
/// need a real path on disk rather than a placeholder.
fn in_temp_project(source: &str) -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("temp dir");
    std::fs::write(temp.path().join("Simplex.toml"), "").expect("write manifest");
    std::fs::create_dir(temp.path().join("simf")).expect("create source dir");
    let path = temp.path().join("simf/main.simf");
    std::fs::write(&path, source).expect("write source");
    (temp, path)
}

const IMPORTING_ROOT: &str = "use crate::shared::broken;\nfn main() { broken(); }\n";

struct DependencyProject {
    temp: TempDir,
    root_path: PathBuf,
    dependency_path: PathBuf,
    root_uri: Uri,
    dependency_uri: Uri,
    settings: Settings,
}

impl DependencyProject {
    fn new(dependency_source: &str) -> Self {
        let temp = TempDir::new().expect("temp dir");
        std::fs::write(temp.path().join("Simplex.toml"), "").expect("write manifest");
        std::fs::create_dir(temp.path().join("simf")).expect("create source dir");
        let dependency_path = temp.path().join("simf/shared.simf");
        let root_path = temp.path().join("simf/main.simf");
        std::fs::write(&dependency_path, dependency_source).expect("write dependency");
        std::fs::write(&root_path, IMPORTING_ROOT).expect("write root");
        let root_uri =
            Uri::from_file_path(std::fs::canonicalize(&root_path).expect("canonical root path"))
                .expect("root URI");
        let dependency_uri = Uri::from_file_path(
            std::fs::canonicalize(&dependency_path).expect("canonical dependency path"),
        )
        .expect("dependency URI");
        let settings = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": true }
        }))
        .expect("valid settings");
        Self {
            temp,
            root_path,
            dependency_path,
            root_uri,
            dependency_uri,
            settings,
        }
    }

    fn root_snapshot(&self) -> AnalysisSnapshot {
        AnalysisSnapshot::analyze(
            IMPORTING_ROOT,
            &self.root_path,
            &self.settings,
            &[self.temp.path().to_path_buf()],
        )
    }

    fn dependency_snapshot(&self, source: &str) -> AnalysisSnapshot {
        AnalysisSnapshot::analyze(
            source,
            &self.dependency_path,
            &self.settings,
            &[self.temp.path().to_path_buf()],
        )
    }

    fn write_dependency(&self, source: &str) {
        std::fs::write(&self.dependency_path, source).expect("write dependency");
    }
}

fn diagnostic_count(updates: &[DiagnosticUpdate], uri: &Uri) -> usize {
    updates
        .iter()
        .find(|update| &update.uri == uri)
        .expect("diagnostic update")
        .diagnostics
        .len()
}

fn record_count(
    events: &StdMutex<Vec<(&'static str, usize)>>,
    label: &'static str,
    updates: &[DiagnosticUpdate],
    uri: &Uri,
) {
    events
        .lock()
        .expect("event lock")
        .push((label, diagnostic_count(updates, uri)));
}

fn sample_program() -> &'static str {
    "fn add(a: u32, b: u32) -> u32 { let (_, res): (bool, u32) = jet::add_32(a, b); res }
     fn main() {}"
}
fn invalid_program_on_ast() -> &'static str {
    "fn add(a: u32, b: u32) -> u32 {}
     fn main() {}"
}

fn invalid_program_on_parsing() -> &'static str {
    "fn add(a: u32, b: u32) -> u32 "
}

fn parse_program(
    source: &str,
    path: &Path,
    settings: &Settings,
    workspace_roots: &[PathBuf],
) -> (Vec<CompilerDiagnostic>, Option<AnalysisSnapshot>) {
    let snapshot = AnalysisSnapshot::analyze(source, path, settings, workspace_roots);
    let mut syntax_diagnostics = DiagnosticManager::new();
    let parsed = parse::Program::parse_from_str_with_errors(
        0,
        source,
        &settings.unstable_features(),
        &mut syntax_diagnostics,
    )
    .is_some();
    let diagnostics = snapshot.compiler_diagnostics.clone();
    (diagnostics, parsed.then_some(snapshot))
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

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn diagnostic_transactions_do_not_interleave_state_and_publication() {
    let project = DependencyProject::new("pub fn broken() -> u32 { false }\n");
    let old_snapshot = project.root_snapshot();
    project.write_dependency("pub fn broken() -> u32 { 0 }\n");
    let new_snapshot = project.root_snapshot();
    let root_uri = project.root_uri.clone();
    let dependency_uri = project.dependency_uri.clone();

    let transaction = StdArc::new(DiagnosticTransaction::default());
    let workspace = StdArc::new(RwLock::new(WorkspaceState::default()));
    let old_input = workspace
        .write()
        .await
        .begin_open(&root_uri, IMPORTING_ROOT, Some(1));
    let first_started = StdArc::new(Notify::new());
    let release_first = StdArc::new(Notify::new());
    let second_started = StdArc::new(Notify::new());
    let second_transition_ran = StdArc::new(AtomicBool::new(false));
    let events = StdArc::new(StdMutex::new(Vec::new()));

    let first_wait = first_started.notified();
    let first = {
        let transaction = StdArc::clone(&transaction);
        let workspace = StdArc::clone(&workspace);
        let first_started = StdArc::clone(&first_started);
        let release_first = StdArc::clone(&release_first);
        let events = StdArc::clone(&events);
        let root_uri = root_uri.clone();
        let dependency_uri = dependency_uri.clone();
        tokio::spawn(async move {
            transaction
                .run(
                    &workspace,
                    |workspace| {
                        workspace.replace_if_current(
                            &root_uri,
                            old_snapshot,
                            old_input.version,
                            old_input.generation,
                        )
                    },
                    |updates| async move {
                        record_count(
                            &events,
                            "first publication started",
                            &updates,
                            &dependency_uri,
                        );
                        first_started.notify_one();
                        release_first.notified().await;
                        record_count(
                            &events,
                            "first publication finished",
                            &updates,
                            &dependency_uri,
                        );
                    },
                )
                .await;
        })
    };
    first_wait.await;
    let new_input = workspace
        .write()
        .await
        .begin_change(&root_uri, IMPORTING_ROOT, Some(2))
        .expect("new analysis ticket");

    let second_wait = second_started.notified();
    let second = {
        let transaction = StdArc::clone(&transaction);
        let workspace = StdArc::clone(&workspace);
        let second_started = StdArc::clone(&second_started);
        let second_transition_ran = StdArc::clone(&second_transition_ran);
        let events = StdArc::clone(&events);
        let root_uri = root_uri.clone();
        let dependency_uri = dependency_uri.clone();
        tokio::spawn(async move {
            second_started.notify_one();
            transaction
                .run(
                    &workspace,
                    |workspace| {
                        second_transition_ran.store(true, Ordering::SeqCst);
                        workspace.replace_if_current(
                            &root_uri,
                            new_snapshot,
                            new_input.version,
                            new_input.generation,
                        )
                    },
                    |updates| async move {
                        record_count(&events, "second publication", &updates, &dependency_uri);
                    },
                )
                .await;
        })
    };

    second_wait.await;
    tokio::task::yield_now().await;
    assert!(!second_transition_ran.load(Ordering::SeqCst));

    release_first.notify_one();
    first.await.expect("first transaction");
    second.await.expect("second transaction");
    assert_eq!(
        *events.lock().expect("event lock"),
        [
            ("first publication started", 1),
            ("first publication finished", 1),
            ("second publication", 0)
        ]
    );

    let workspace = workspace.read().await;
    let final_snapshot = workspace.get(&root_uri).expect("latest root snapshot");
    let final_diagnostics =
        crate::workspace::diagnostics::DiagnosticBundle::from_snapshot(final_snapshot);
    assert!(final_diagnostics.get(&dependency_uri).is_none());
}

#[tokio::test]
async fn closing_document_restores_dependency_diagnostics_through_transaction() {
    let broken_dependency = "pub fn broken() -> u32 { false }\n";
    let clean_dependency = "pub fn broken() -> u32 { 0 }\n";
    let project = DependencyProject::new(broken_dependency);
    let root_snapshot = project.root_snapshot();
    let clean_snapshot = project.dependency_snapshot(clean_dependency);
    let root_uri = project.root_uri.clone();
    let dependency_uri = project.dependency_uri.clone();

    let transaction = DiagnosticTransaction::default();
    let workspace = RwLock::new(WorkspaceState::default());
    let root_input = workspace
        .write()
        .await
        .begin_open(&root_uri, IMPORTING_ROOT, Some(1));
    workspace
        .write()
        .await
        .replace_if_current(
            &root_uri,
            root_snapshot,
            root_input.version,
            root_input.generation,
        )
        .expect("root diagnostic update");
    let open = workspace
        .write()
        .await
        .begin_open(&dependency_uri, clean_dependency, Some(7));
    let publications = StdArc::new(StdMutex::new(Vec::new()));

    transaction
        .run(
            &workspace,
            |workspace| {
                workspace.replace_if_current(
                    &dependency_uri,
                    clean_snapshot,
                    open.version,
                    open.generation,
                )
            },
            {
                let publications = StdArc::clone(&publications);
                let dependency_uri = dependency_uri.clone();
                move |updates| async move {
                    let update = updates
                        .iter()
                        .find(|update| update.uri == dependency_uri)
                        .expect("direct dependency update");
                    publications
                        .lock()
                        .expect("publication lock")
                        .push((update.diagnostics.len(), update.version));
                }
            },
        )
        .await;

    let close_generation = workspace
        .write()
        .await
        .begin_close(&dependency_uri)
        .expect("close ticket");
    transaction
        .run(
            &workspace,
            |workspace| workspace.remove_if_current(&dependency_uri, close_generation),
            {
                let publications = StdArc::clone(&publications);
                let dependency_uri = dependency_uri.clone();
                move |updates| async move {
                    let update = updates
                        .iter()
                        .find(|update| update.uri == dependency_uri)
                        .expect("restored dependency update");
                    publications
                        .lock()
                        .expect("publication lock")
                        .push((update.diagnostics.len(), update.version));
                }
            },
        )
        .await;

    assert_eq!(
        *publications.lock().expect("publication lock"),
        [(0, Some(7)), (1, None)]
    );
}

#[test]
fn test_parse_program_valid() {
    let (temp, path) = in_temp_project(sample_program());
    let (err, doc) = parse_program(
        sample_program(),
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );
    assert!(err.is_empty(), "Expected no parsing error, got {err:?}");
    let doc = doc.expect("Expected Some(Document)");
    assert_eq!(doc.functions.iter().count(), 2);
}

#[test]
fn library_file_without_main_keeps_definition_metadata() {
    let source = "fn helper() {}\nfn caller() { helper() }\n";
    let (temp, path) = in_temp_project(source);
    let (errors, doc) = parse_program(
        source,
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );

    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    let doc = doc.expect("library document");
    let call_start = source.rfind("helper").expect("helper call");
    let call = doc
        .find_related_call(Span::new(0, call_start + 1..call_start + 1))
        .expect("helper call should remain navigable");
    let function = doc
        .functions
        .get_func(call.name().to_string().as_str())
        .expect("helper definition");
    let source_file = doc
        .sources
        .get(function.span().file_id)
        .expect("current file should always have source metadata");

    assert_eq!(function.name().as_inner(), "helper");
    assert_eq!(function.span().file_id, 0);
    assert_eq!(
        source_file.uri,
        Uri::from_file_path(std::fs::canonicalize(&path).expect("canonical path"))
            .expect("file URI")
    );
}

#[test]
fn nested_main_does_not_conflict_with_a_synthetic_entry_point() {
    let source = "mod nested { fn main() {} }\n";
    let (temp, path) = in_temp_project(source);
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .expect("valid settings");

    let (errors, doc) = parse_program(source, &path, &settings, &[temp.path().to_path_buf()]);

    assert!(
        doc.is_some(),
        "the source should remain available to the LSP"
    );
    assert!(
        !errors.iter().any(|diagnostic| {
            matches!(
                diagnostic.error(),
                Error::FunctionRedefined { name } if name.as_inner() == "main"
            )
        }),
        "a nested main must not collide with an injected main: {errors:?}"
    );
    let expected = Error::MainOutOfEntryFile.to_string();
    assert!(
        errors.iter().any(|diagnostic| {
            matches!(diagnostic.error(), Error::CannotParse { msg } if msg == &expected)
        }),
        "the compiler should still report that main is outside the entry scope: {errors:?}"
    );
}

#[test]
fn use_items_and_aliases_resolve_to_the_imported_definition() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    std::fs::write(root.join("Simplex.toml"), "").expect("write manifest");
    std::fs::create_dir(root.join("simf")).expect("create source dir");
    let dependency_path = root.join("simf/math.simf");
    std::fs::write(&dependency_path, "pub fn add() {}\npub fn subtract() {}\n")
        .expect("write module");
    let source = "use crate::math::{add as plus, subtract};\nfn main() { plus(); subtract() }\n";
    let path = root.join("simf/main.simf");
    std::fs::write(&path, source).expect("write entry file");
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .expect("valid settings");

    let (errors, doc) = parse_program(source, &path, &settings, &[root.to_path_buf()]);
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    let doc = doc.expect("document");
    let imported_at = |needle: &str| {
        let start = source.find(needle).expect("import token");
        doc.find_imported_function(Span::new(0, start + 1..start + 1))
            .expect("imported function")
    };

    let original = imported_at("add as");
    let alias = imported_at("plus,");
    let grouped_item = imported_at("subtract}");
    assert_eq!(original.name().as_inner(), "add");
    assert_eq!(alias.name().as_inner(), "add");
    assert_eq!(grouped_item.name().as_inner(), "subtract");
    assert!(doc
        .find_imported_function(Span::new(
            0,
            source.find("math").unwrap() + 1..source.find("math").unwrap() + 1,
        ))
        .is_none());

    let source_file = doc
        .sources
        .get(original.span().file_id)
        .expect("imported source metadata");
    assert_eq!(
        source_file.uri,
        Uri::from_file_path(
            std::fs::canonicalize(&dependency_path).expect("canonical dependency path"),
        )
        .expect("file URI")
    );
}

#[test]
fn nested_and_transitive_reexports_resolve_to_original_definitions() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    let write = |path: PathBuf, source: &str| {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("create dir");
        std::fs::write(path, source).expect("write file");
    };

    write(
        root.join("Simplex.toml"),
        "[dependencies]\nmerkle = { path = 'deps/merkle' }\nfacade = { path = 'deps/facade' }\n",
    );
    write(root.join("deps/merkle/Simplex.toml"), "");
    let merkle_path = root.join("deps/merkle/simf/build_root.simf");
    write(
        merkle_path.clone(),
        "pub mod wrapper {\n    pub fn get_root() {}\n    pub fn hash() {}\n}\npub use crate::wrapper::{get_root, hash};\n",
    );
    write(
        root.join("deps/facade/Simplex.toml"),
        "[dependencies]\nleaf = { path = '../leaf' }\n",
    );
    write(
        root.join("deps/facade/simf/smth.simf"),
        "pub use leaf::ops::hash;\n",
    );
    write(root.join("deps/leaf/Simplex.toml"), "");
    let leaf_path = root.join("deps/leaf/simf/ops.simf");
    write(leaf_path.clone(), "pub fn hash() {}\n");

    let source = "use merkle::build_root::{get_root, hash as and_hash};\nuse facade::smth::hash as or_hash;\nfn main() { get_root(); and_hash(); or_hash(); }\n";
    let path = root.join("simf/main.simf");
    write(path.clone(), source);
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .expect("valid settings");

    let (errors, doc) = parse_program(source, &path, &settings, &[root.to_path_buf()]);
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    let doc = doc.expect("document");
    let imported_at = |offset: usize| {
        doc.find_imported_function(Span::new(0, offset + 1..offset + 1))
            .expect("imported function")
    };

    let nested = imported_at(source.find("get_root").expect("nested import"));
    let nested_alias = imported_at(source.find("and_hash").expect("nested alias"));
    let transitive = imported_at(source.rfind("hash as").expect("transitive import"));
    let transitive_alias = imported_at(source.find("or_hash").expect("transitive alias"));

    assert_eq!(nested.name().as_inner(), "get_root");
    assert_eq!(nested_alias.name().as_inner(), "hash");
    assert_eq!(transitive.name().as_inner(), "hash");
    assert_eq!(transitive_alias.name().as_inner(), "hash");

    let definition_uri = |function: &parse::Function| &doc.sources[function.span().file_id].uri;
    let merkle_uri =
        Uri::from_file_path(std::fs::canonicalize(merkle_path).expect("canonical merkle path"))
            .expect("merkle URI");
    let leaf_uri =
        Uri::from_file_path(std::fs::canonicalize(leaf_path).expect("canonical leaf path"))
            .expect("leaf URI");
    assert_eq!(definition_uri(nested), &merkle_uri);
    assert_eq!(definition_uri(nested_alias), &merkle_uri);
    assert_eq!(definition_uri(transitive), &leaf_uri);
    assert_eq!(definition_uri(transitive_alias), &leaf_uri);
}

#[test]
fn parse_program_respects_the_enum_feature_setting() {
    let source = "enum Choice { Yes, No, }\nfn main() {}\n";
    let (temp, path) = in_temp_project(source);

    let (disabled, _) = parse_program(
        source,
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );
    assert!(disabled.iter().any(|diagnostic| {
        matches!(
            diagnostic.error(),
            Error::UnstableFeature {
                feature: simplicityhl::UnstableFeature::Enums
            }
        )
    }));

    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": false, "enums": true }
    }))
    .expect("valid settings");
    let (enabled, document) = parse_program(source, &path, &settings, &[temp.path().to_path_buf()]);

    assert!(enabled.is_empty(), "enum should be enabled: {enabled:?}");
    assert!(document.is_some());
}

#[test]
fn function_selection_range_is_inside_its_document_symbol_range() {
    let source = "/* 😀 */ fn main() {}";
    let (temp, path) = in_temp_project(source);
    let (errors, doc) = parse_program(
        source,
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    let doc = doc.expect("document");
    let function = doc
        .functions
        .functions()
        .into_iter()
        .find(|function| function.name().as_inner() == "main")
        .expect("main function");
    let (start, end) = span_to_positions(function.span(), &doc.text).unwrap();
    let full_range = Range::new(start, end);
    let selection_range = doc.find_function_name_range(function).unwrap();

    assert!(selection_range.start >= full_range.start);
    assert!(selection_range.end <= full_range.end);
    let name_start = source.find("main").expect("function name");
    assert_eq!(
        selection_range,
        Range::new(
            offset_to_position(name_start, &doc.text).unwrap(),
            offset_to_position(name_start + "main".len(), &doc.text).unwrap(),
        )
    );
}

#[test]
fn stale_analysis_cannot_produce_an_out_of_bounds_selection_range() {
    let source = "fn main() {}";
    let (temp, path) = in_temp_project(source);
    let (_, doc) = parse_program(
        source,
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );
    let mut doc = doc.expect("document");
    let function = doc
        .functions
        .functions()
        .into_iter()
        .find(|function| function.name().as_inner() == "main")
        .expect("main function")
        .clone();

    doc.text = Rope::from_str(&format!("// {}\n{source}", "x".repeat(100)));

    assert!(doc.find_function_name_range(&function).is_err());
}

#[test]
fn looking_for_a_call_outside_a_function_is_an_empty_result() {
    let source = "/* heading */\nfn main() {}";
    let (temp, path) = in_temp_project(source);
    let (errors, doc) = parse_program(
        source,
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    let doc = doc.expect("document");

    assert!(doc
        .find_related_call(simplicityhl::error::Span::new(0, 0..0))
        .is_none());
}

#[test]
#[ignore = "TODO we need to also create a file with a path so that could work"]
fn test_parse_program_invalid_ast() {
    let (temp, path) = in_temp_project(invalid_program_on_ast());
    let (err, doc) = parse_program(
        invalid_program_on_ast(),
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );
    assert!(
        err.first()
            .expect("program should produce an error")
            .to_string()
            .contains("Expected expression of type `u32`, found type `()`"),
        "Expected error on return type"
    );
    assert!(doc.is_some(), "Expected problem in AST build, not parse");
}

#[test]
fn parse_program_resolves_a_manifest_dependency() {
    // End-to-end check that the manifest drives import resolution: `math` is only
    // reachable because Simplex.toml declares it, and `src_dir` points the package
    // root at `contracts` rather than the default `simf`.
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    let write = |path: PathBuf, source: &str| {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("create dir");
        std::fs::write(path, source).expect("write file");
    };
    write(
        root.join("Simplex.toml"),
        "[build]\nsrc_dir = 'contracts'\n[dependencies]\nmath = { path = 'vendor/math' }\n",
    );
    write(root.join("vendor/math/Simplex.toml"), "");
    write(
        root.join("vendor/math/simf/ops.simf"),
        "pub fn double(a: u32) -> u32 {\n    let (_, n): (bool, u32) = jet::add_32(a, a);\n    n\n}\n",
    );

    let source = "use math::ops::double;\nfn main() {\n    let _: u32 = double(2);\n}\n";
    let path = root.join("contracts/main.simf");
    write(path.clone(), source);

    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .expect("valid settings");

    let (err, doc) = parse_program(source, &path, &settings, &[root.to_path_buf()]);

    assert!(
        err.is_empty(),
        "expected the import to resolve, got {err:?}"
    );
    assert!(doc.is_some(), "expected a document");
}

#[test]
fn duplicate_imported_main_points_to_the_import() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    std::fs::write(root.join("Simplex.toml"), "").expect("write manifest");
    std::fs::create_dir(root.join("simf")).expect("create source dir");
    std::fs::write(
        root.join("simf/library.simf"),
        "pub fn helper() {}\nfn main() {}\n",
    )
    .expect("write imported module");

    let import = "use crate::library::helper;";
    let source = format!("{import}\nfn main() {{}}\n");
    let path = root.join("simf/main.simf");
    std::fs::write(&path, &source).expect("write entry file");
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .expect("valid settings");

    let (errors, _) = parse_program(&source, &path, &settings, &[root.to_path_buf()]);
    let duplicate_main = errors
        .iter()
        .find(|error| {
            matches!(
                error.error(),
                Error::FunctionRedefined { name } if name.as_inner() == "main"
            )
        })
        .expect("duplicate main diagnostic");

    let CompilerLocation::Code(span) = duplicate_main.location() else {
        panic!("duplicate main should point to source code");
    };
    assert_eq!(span.to_slice(&source), Some(import));
}

#[test]
fn imported_diagnostic_points_to_its_real_file() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    std::fs::write(root.join("Simplex.toml"), "").expect("write manifest");
    std::fs::create_dir(root.join("simf")).expect("create source dir");
    let library_path = root.join("simf/library.simf");
    let library_source = "pub fn broken() -> (u1, u256) { 0 }\n";
    std::fs::write(&library_path, library_source).expect("write imported module");

    let source = "use crate::library::broken;\nfn main() { broken(); }\n";
    let path = root.join("simf/main.simf");
    std::fs::write(&path, source).expect("write entry file");
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .expect("valid settings");

    let (_errors, document) = parse_program(source, &path, &settings, &[root.to_path_buf()]);
    let document = document.expect("document remains available after analysis errors");
    assert!(document.sources.len() > 1);
    let bundle = crate::workspace::diagnostics::DiagnosticBundle::from_snapshot(&document);
    let library_uri =
        Uri::from_file_path(std::fs::canonicalize(&library_path).expect("canonical library path"))
            .expect("library URI");
    let root_uri = Uri::from_file_path(std::fs::canonicalize(&path).expect("canonical root"))
        .expect("root URI");

    let imported_error = bundle
        .get(&library_uri)
        .and_then(|diagnostics| {
            diagnostics
                .iter()
                .find(|diagnostic| diagnostic.message.contains("Expected expression"))
        })
        .unwrap_or_else(|| panic!("expected imported diagnostic, got {bundle:?}"));
    assert_ne!(imported_error.range, Range::default());
    assert!(!bundle.get(&root_uri).is_some_and(|diagnostics| {
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == imported_error.message)
    }));
}

#[test]
fn lsp_diagnostics_preserve_secondary_labels_notes_and_help() {
    let source = "fn main() {}\n";
    let mut document = document_from_source(source);
    let diagnostic = CompilerDiagnostic::new(
        Error::CannotParse {
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

#[test]
fn parse_program_reports_a_missing_configured_manifest() {
    let (temp, path) = in_temp_project(sample_program());
    let mut settings = Settings::default();
    settings.project.simplex.manifest_path = "nowhere/Simplex.toml".to_string();

    let (err, _) = parse_program(
        sample_program(),
        &path,
        &settings,
        &[temp.path().to_path_buf()],
    );

    assert!(
        err.iter()
            .any(|e| e.to_string().contains("Simplex manifest was not found")),
        "a misconfigured manifest path should surface as a diagnostic, got {err:?}"
    );
}

#[test]
fn test_parse_program_invalid_parse() {
    let (temp, path) = in_temp_project(invalid_program_on_parsing());
    let (err, doc) = parse_program(
        invalid_program_on_parsing(),
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );
    match err
        .first()
        .expect("program should produce an error")
        .error()
        .clone()
    {
        Error::Syntax { .. } => {}
        _ => panic!("Expected `Syntax` error"),
    }

    assert!(doc.is_none(), "Expected no document to return");
}
