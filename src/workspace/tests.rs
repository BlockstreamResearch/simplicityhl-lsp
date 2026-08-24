use std::path::Path;

use tempfile::TempDir;
use tower_lsp_server::UriExt;

use super::*;
use crate::config::Settings;

fn write(path: impl AsRef<Path>, source: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create directory");
    std::fs::write(path, source).expect("write source");
}

fn imports_enabled() -> Settings {
    Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .expect("valid settings")
}

fn analyze(source: &str, path: &Path, root: &Path) -> AnalysisSnapshot {
    AnalysisSnapshot::analyze(source, path, &imports_enabled(), &[root.to_path_buf()])
}

fn update_for<'a>(updates: &'a [DiagnosticUpdate], uri: &Uri) -> &'a DiagnosticUpdate {
    updates
        .iter()
        .find(|update| &update.uri == uri)
        .expect("URI should be republished")
}

fn canonical_uri(path: &Path) -> Uri {
    Uri::from_file_path(std::fs::canonicalize(path).expect("canonical path")).expect("file URI")
}

fn temporary_uri(name: &str) -> Uri {
    Uri::from_file_path(std::env::temp_dir().join(name)).expect("temporary file URI")
}

fn insert_analysis(state: &mut WorkspaceState, source: &str, path: &Path, root: &Path) -> Uri {
    let uri = canonical_uri(path);
    state.replace_inner(&uri, analyze(source, path, root), Some(1));
    uri
}

#[test]
fn references_exclude_unrelated_same_named_functions() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    let source = "fn helper() {}\nfn main() { helper(); }\n";
    let first_path = root.join("simf/first.simf");
    let second_path = root.join("simf/second.simf");
    write(&first_path, source);
    write(&second_path, source);

    let mut state = WorkspaceState::default();
    let first_uri = insert_analysis(&mut state, source, &first_path, root);
    let second_uri = insert_analysis(&mut state, source, &second_path, root);
    let target = {
        let snapshot = state.get(&first_uri).expect("first analysis");
        snapshot
            .function_identity(
                snapshot
                    .functions
                    .get_func("helper")
                    .expect("helper function"),
            )
            .expect("helper identity")
    };

    let references = state.find_references_to(&target);
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].uri, first_uri);
    assert_ne!(references[0].uri, second_uri);
}

#[test]
fn references_follow_aliases_to_one_definition_across_roots() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    write(root.join("simf/shared.simf"), "pub fn target() {}\n");
    let first_source = "use crate::shared::target as alpha;\nfn main() { alpha(); }\n";
    let second_source = "use crate::shared::target as beta;\nfn main() { beta(); }\n";
    let first_path = root.join("simf/first.simf");
    let second_path = root.join("simf/second.simf");
    write(&first_path, first_source);
    write(&second_path, second_source);

    let mut state = WorkspaceState::default();
    let first_uri = insert_analysis(&mut state, first_source, &first_path, root);
    let second_uri = insert_analysis(&mut state, second_source, &second_path, root);
    let target = {
        let snapshot = state.get(&first_uri).expect("first analysis");
        snapshot
            .function_identity(
                snapshot
                    .functions
                    .get_func("alpha")
                    .expect("aliased target"),
            )
            .expect("target identity")
    };

    let references = state.find_references_to(&target);
    assert_eq!(references.len(), 2);
    assert_eq!(references[0].uri, first_uri);
    assert_eq!(references[1].uri, second_uri);
}

#[test]
fn references_deduplicate_shared_dependency_locations() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    let shared_path = root.join("simf/shared.simf");
    write(
        &shared_path,
        "pub fn target() {}\npub fn invoke() { target(); }\n",
    );
    let source = "use crate::shared::{target, invoke};\nfn main() { target(); invoke(); }\n";
    let first_path = root.join("simf/first.simf");
    let second_path = root.join("simf/second.simf");
    write(&first_path, source);
    write(&second_path, source);

    let mut state = WorkspaceState::default();
    let first_uri = insert_analysis(&mut state, source, &first_path, root);
    let second_uri = insert_analysis(&mut state, source, &second_path, root);
    let shared_uri = canonical_uri(&shared_path);
    let target = {
        let snapshot = state.get(&first_uri).expect("first analysis");
        snapshot
            .function_identity(
                snapshot
                    .functions
                    .get_func("target")
                    .expect("imported target"),
            )
            .expect("target identity")
    };

    let references = state.find_references_to(&target);
    assert_eq!(references.len(), 3);
    assert_eq!(
        references
            .iter()
            .filter(|location| location.uri == shared_uri)
            .count(),
        1
    );
    assert!(references.iter().any(|location| location.uri == first_uri));
    assert!(references.iter().any(|location| location.uri == second_uri));
}

#[test]
fn dependency_references_use_the_owning_module_scope() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    write(root.join("simf/a.simf"), "pub fn target() {}\n");
    let second_dependency = root.join("simf/b.simf");
    write(
        &second_dependency,
        "pub fn target() {}\npub fn wrapper() { target(); }\n",
    );
    let source =
        "use crate::a::target;\nuse crate::b::wrapper;\nfn main() { target(); wrapper(); }\n";
    let root_path = root.join("simf/main.simf");
    write(&root_path, source);

    let mut state = WorkspaceState::default();
    let root_uri = insert_analysis(&mut state, source, &root_path, root);
    let second_dependency_uri = canonical_uri(&second_dependency);
    let target = {
        let snapshot = state.get(&root_uri).expect("root analysis");
        snapshot
            .function_identity(
                snapshot
                    .functions
                    .get_func("target")
                    .expect("first dependency target"),
            )
            .expect("target identity")
    };

    let references = state.find_references_to(&target);
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].uri, root_uri);
    assert!(!references
        .iter()
        .any(|location| location.uri == second_dependency_uri));
}

#[test]
fn close_generation_rejects_an_in_flight_analysis() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    let source = "fn main() {}\n";
    let path = root.join("simf/main.simf");
    write(&path, source);
    let uri = canonical_uri(&path);
    let mut state = WorkspaceState::default();

    let analysis_input = state.begin_open(&uri, source, Some(1));
    let snapshot = analyze(source, &path, root);
    let close_generation = state.begin_close(&uri).expect("close ticket");

    assert!(state
        .replace_if_current(&uri, snapshot, Some(1), analysis_input.generation)
        .is_none());
    assert!(state.remove_if_current(&uri, close_generation).is_some());
    assert!(state.get(&uri).is_none());
}

#[test]
fn newer_generation_wins_when_document_versions_are_equal() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    let old_source = "fn old() {}\nfn main() {}\n";
    let new_source = "fn new() {}\nfn main() {}\n";
    let path = root.join("simf/main.simf");
    write(&path, new_source);
    let uri = canonical_uri(&path);
    let mut state = WorkspaceState::default();

    let old_input = state.begin_open(&uri, old_source, Some(1));
    let old_snapshot = analyze(old_source, &path, root);
    let new_input = state
        .begin_change(&uri, new_source, Some(1))
        .expect("open document");
    let new_snapshot = analyze(new_source, &path, root);

    assert!(state
        .replace_if_current(&uri, new_snapshot, Some(1), new_input.generation)
        .is_some());
    assert!(state
        .replace_if_current(&uri, old_snapshot, Some(1), old_input.generation)
        .is_none());
    assert_eq!(
        state.get(&uri).expect("current analysis").text.to_string(),
        new_source
    );
}

#[test]
fn older_change_is_rejected_before_analysis() {
    let uri = temporary_uri("change-order.simf");
    let mut state = WorkspaceState::default();
    state.begin_open(&uri, "fn initial() {}\n", Some(1));
    state
        .begin_change(&uri, "fn newest() {}\n", Some(3))
        .expect("newer change");

    assert!(state
        .begin_change(&uri, "fn stale() {}\n", Some(2))
        .is_none());
    let current = state.begin_reanalysis().pop().expect("current document");
    assert_eq!(current.text.as_ref(), "fn newest() {}\n");
    assert_eq!(current.version, Some(3));
}

#[test]
fn reanalysis_includes_documents_with_pending_initial_analysis() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    let source = "fn main() {}\n";
    let path = root.join("simf/main.simf");
    write(&path, source);
    let uri = canonical_uri(&path);
    let mut state = WorkspaceState::default();

    let initial_input = state.begin_open(&uri, source, Some(1));
    assert!(state.get(&uri).is_none());

    let requests = state.begin_reanalysis();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.uri, uri);
    assert_eq!(request.text.as_ref(), source);
    assert_eq!(request.version, Some(1));
    assert_ne!(request.generation, initial_input.generation);
}

#[test]
fn pre_close_reanalysis_cannot_overwrite_a_reopened_document() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    let old_source = "fn old() {}\nfn main() {}\n";
    let new_source = "fn new() {}\nfn main() {}\n";
    let path = root.join("simf/main.simf");
    write(&path, new_source);
    let uri = canonical_uri(&path);
    let mut state = WorkspaceState::default();

    let old_input = state.begin_open(&uri, old_source, Some(5));
    state
        .replace_if_current(
            &uri,
            analyze(old_source, &path, root),
            old_input.version,
            old_input.generation,
        )
        .expect("initial analysis");
    let stale_request = state.begin_reanalysis().pop().expect("reanalysis request");
    let close_generation = state.begin_close(&uri).expect("close ticket");
    let reopened_input = state.begin_open(&uri, new_source, Some(1));

    assert!(state.remove_if_current(&uri, close_generation).is_none());
    assert!(state
        .replace_if_current(
            &uri,
            analyze(old_source, &path, root),
            stale_request.version,
            stale_request.generation,
        )
        .is_none());
    assert!(state
        .replace_if_current(
            &uri,
            analyze(new_source, &path, root),
            Some(1),
            reopened_input.generation,
        )
        .is_some());
    assert_eq!(
        state.get(&uri).expect("reopened analysis").text.to_string(),
        new_source
    );
    assert_eq!(state.get(&uri).expect("reopened analysis").version, Some(1));
}

#[test]
fn save_ticket_keeps_the_last_known_version() {
    let uri = temporary_uri("save-ticket.simf");
    let mut state = WorkspaceState::default();
    state.begin_open(&uri, "fn main() {}\n", Some(7));

    let save = state
        .begin_change(&uri, "fn main() {}\n", None)
        .expect("save ticket");

    assert_eq!(save.version, Some(7));
}

#[test]
fn closing_document_releases_its_buffer_payload() {
    let uri = temporary_uri("closed-buffer.simf");
    let mut state = WorkspaceState::default();
    state.begin_open(&uri, "fn main() {}\n", Some(3));

    state.begin_close(&uri).expect("close ticket");

    let document = state.documents.get(&uri).expect("generation tombstone");
    assert!(!document.open);
    assert!(document.text.is_none());
    assert_eq!(document.version, None);
    assert!(state.begin_close(&uri).is_none());

    let unknown = temporary_uri("unknown-close.simf");
    assert!(state.begin_close(&unknown).is_none());
    assert!(!state.documents.contains_key(&unknown));
}

#[test]
fn shared_dependencies_deduplicate_and_open_buffers_take_precedence() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    let dependency_path = root.join("simf/shared.simf");
    write(&dependency_path, "pub fn broken() -> u32 { false }\n");
    let root_source = "use crate::shared::broken;\nfn main() { broken(); }\n";
    let root_a = root.join("simf/a.simf");
    let root_b = root.join("simf/b.simf");
    write(&root_a, root_source);
    write(&root_b, root_source);

    let dependency_uri =
        Uri::from_file_path(std::fs::canonicalize(&dependency_path).expect("canonical dependency"))
            .expect("dependency URI");
    let first_root_uri =
        Uri::from_file_path(std::fs::canonicalize(&root_a).expect("canonical root"))
            .expect("root URI");
    let second_root_uri =
        Uri::from_file_path(std::fs::canonicalize(&root_b).expect("canonical root"))
            .expect("root URI");
    let mut state = WorkspaceState::default();

    let updates = state.replace_inner(
        &first_root_uri,
        analyze(root_source, &root_a, root),
        Some(1),
    );
    assert_eq!(update_for(&updates, &first_root_uri).version, Some(1));
    let imported = update_for(&updates, &dependency_uri);
    assert_eq!(imported.version, None);
    assert_eq!(imported.diagnostics.len(), 1);

    let updates = state.replace_inner(
        &second_root_uri,
        analyze(root_source, &root_b, root),
        Some(1),
    );
    assert_eq!(update_for(&updates, &dependency_uri).diagnostics.len(), 1);

    // The buffer differs from the saved dependency used by both roots. Its direct clean
    // analysis is authoritative until that document closes.
    let clean_dependency = "pub fn broken() -> u32 { 0 }\n";
    let updates = state.replace_inner(
        &dependency_uri,
        analyze(clean_dependency, &dependency_path, root),
        Some(7),
    );
    let direct = update_for(&updates, &dependency_uri);
    assert!(direct.diagnostics.is_empty());
    assert_eq!(direct.version, Some(7));

    let updates = state.remove(&dependency_uri);
    let restored = update_for(&updates, &dependency_uri);
    assert_eq!(restored.version, None);
    assert_eq!(restored.diagnostics.len(), 1);
}

#[test]
fn replacing_roots_clears_removed_dependency_diagnostics() {
    let temp = TempDir::new().expect("temp dir");
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    let dependency_path = root.join("simf/shared.simf");
    write(&dependency_path, "pub fn broken() -> u32 { false }\n");
    let root_path = root.join("simf/main.simf");
    let importing = "use crate::shared::broken;\nfn main() { broken(); }\n";
    write(&root_path, importing);

    let root_uri = Uri::from_file_path(std::fs::canonicalize(&root_path).expect("canonical root"))
        .expect("root URI");
    let dependency_uri =
        Uri::from_file_path(std::fs::canonicalize(&dependency_path).expect("canonical dependency"))
            .expect("dependency URI");
    let mut state = WorkspaceState::default();
    let initial = state.begin_open(&root_uri, importing, Some(1));
    state
        .replace_if_current(
            &root_uri,
            analyze(importing, &root_path, root),
            initial.version,
            initial.generation,
        )
        .expect("initial analysis");

    let clean = "fn main() {}\n";
    let stale_ticket = state
        .begin_change(&root_uri, importing, Some(2))
        .expect("stale ticket");
    let current = state
        .begin_change(&root_uri, clean, Some(3))
        .expect("current ticket");
    let updates = state
        .replace_if_current(
            &root_uri,
            analyze(clean, &root_path, root),
            current.version,
            current.generation,
        )
        .expect("newer analysis");
    assert!(update_for(&updates, &dependency_uri).diagnostics.is_empty());

    let stale_result = state.replace_if_current(
        &root_uri,
        analyze(importing, &root_path, root),
        stale_ticket.version,
        stale_ticket.generation,
    );
    assert!(stale_result.is_none());
}
