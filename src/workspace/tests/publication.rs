use super::{finish, open, store, update_for, Project};
use crate::workspace::WorkspaceState;

#[test]
fn shared_dependencies_deduplicate_and_open_buffers_take_precedence() {
    let project = Project::new();
    let dependency = project.file("shared.simf", "pub fn broken() -> u32 { false }\n");
    let root_source = "use crate::shared::broken;\nfn main() { broken(); }\n";
    let first_root = project.file("a.simf", root_source);
    let second_root = project.file("b.simf", root_source);
    let mut state = WorkspaceState::default();

    let updates = store(&mut state, &project, &first_root, root_source, Some(1));
    assert_eq!(update_for(&updates, &first_root.uri).version, Some(1));
    let imported = update_for(&updates, &dependency.uri);
    assert_eq!(imported.version, None);
    assert_eq!(imported.diagnostics.len(), 1);

    let updates = store(&mut state, &project, &second_root, root_source, Some(1));
    assert_eq!(update_for(&updates, &dependency.uri).diagnostics.len(), 1);

    // The buffer differs from the saved dependency used by both roots.
    // Its direct clean analysis is authoritative until that document closes.
    let clean_dependency = "pub fn broken() -> u32 { 0 }\n";
    let updates = store(&mut state, &project, &dependency, clean_dependency, Some(7));
    let direct = update_for(&updates, &dependency.uri);
    assert!(direct.diagnostics.is_empty());
    assert_eq!(direct.version, Some(7));

    let updates = state.remove(&dependency.uri);
    let restored = update_for(&updates, &dependency.uri);
    assert_eq!(restored.version, None);
    assert_eq!(restored.diagnostics.len(), 1);
}

#[test]
fn pending_open_buffer_analysis_suppresses_cross_root_republication() {
    let project = Project::new();
    let dependency = project.file("shared.simf", "pub fn broken() -> u32 { false }\n");
    let root = project.file(
        "main.simf",
        "use crate::shared::broken;\nfn main() { broken(); }\n",
    );
    let clean_dependency = "pub fn broken() -> u32 { 0 }\n";
    let mut state = WorkspaceState::default();
    open(&mut state, &project, &root, &root.source, 1);
    open(&mut state, &project, &dependency, clean_dependency, 1);

    let pending = state
        .begin_change(&dependency.uri, clean_dependency, Some(2))
        .expect("pending dependency analysis");
    let root_change = state
        .begin_change(&root.uri, &root.source, Some(2))
        .expect("root change");
    let cross_root_updates = finish(&mut state, &project, &root_change).expect("root reanalysis");
    assert!(!cross_root_updates
        .iter()
        .any(|update| update.uri == dependency.uri));

    let current = finish(&mut state, &project, &pending).expect("current dependency analysis");
    let dependency_update = update_for(&current, &dependency.uri);
    assert_eq!(dependency_update.version, Some(2));
    assert!(dependency_update.diagnostics.is_empty());
}

#[test]
fn replacing_roots_clears_removed_dependency_diagnostics() {
    let project = Project::new();
    let dependency = project.file("shared.simf", "pub fn broken() -> u32 { false }\n");
    let root = project.file(
        "main.simf",
        "use crate::shared::broken;\nfn main() { broken(); }\n",
    );
    let mut state = WorkspaceState::default();
    open(&mut state, &project, &root, &root.source, 1);

    let stale_ticket = state
        .begin_change(&root.uri, &root.source, Some(2))
        .expect("stale ticket");
    let current = state
        .begin_change(&root.uri, "fn main() {}\n", Some(3))
        .expect("current ticket");

    let updates = finish(&mut state, &project, &current).expect("newer analysis");
    assert!(update_for(&updates, &dependency.uri).diagnostics.is_empty());
    assert!(finish(&mut state, &project, &stale_ticket).is_none());
}
