use super::{identity, store, Project};
use crate::workspace::WorkspaceState;

#[test]
fn references_exclude_unrelated_same_named_functions() {
    let project = Project::new();
    let source = "fn helper() {}\nfn main() { helper(); }\n";
    let first = project.file("first.simf", source);
    let second = project.file("second.simf", source);
    let mut state = WorkspaceState::default();
    store(&mut state, &project, &first, source, Some(1));
    store(&mut state, &project, &second, source, Some(1));

    let references = state.find_references_to(&identity(&state, &first.uri, "helper"));

    assert_eq!(references.len(), 1);
    assert_eq!(references[0].uri, first.uri);
    assert_ne!(references[0].uri, second.uri);
}

#[test]
fn references_follow_aliases_to_one_definition_across_roots() {
    let project = Project::new();
    project.file("shared.simf", "pub fn target() {}\n");
    let first = project.file(
        "first.simf",
        "use crate::shared::target as alpha;\nfn main() { alpha(); }\n",
    );
    let second = project.file(
        "second.simf",
        "use crate::shared::target as beta;\nfn main() { beta(); }\n",
    );
    let mut state = WorkspaceState::default();
    store(&mut state, &project, &first, &first.source, Some(1));
    store(&mut state, &project, &second, &second.source, Some(1));

    let references = state.find_references_to(&identity(&state, &first.uri, "alpha"));

    assert_eq!(references.len(), 2);
    assert_eq!(references[0].uri, first.uri);
    assert_eq!(references[1].uri, second.uri);
}

#[test]
fn references_deduplicate_shared_dependency_locations() {
    let project = Project::new();
    let shared = project.file(
        "shared.simf",
        "pub fn target() {}\npub fn invoke() { target(); }\n",
    );
    let source = "use crate::shared::{target, invoke};\nfn main() { target(); invoke(); }\n";
    let first = project.file("first.simf", source);
    let second = project.file("second.simf", source);
    let mut state = WorkspaceState::default();
    store(&mut state, &project, &first, source, Some(1));
    store(&mut state, &project, &second, source, Some(1));

    let references = state.find_references_to(&identity(&state, &first.uri, "target"));

    assert_eq!(references.len(), 3);
    assert_eq!(
        references
            .iter()
            .filter(|location| location.uri == shared.uri)
            .count(),
        1
    );
    assert!(references.iter().any(|location| location.uri == first.uri));
    assert!(references.iter().any(|location| location.uri == second.uri));
}

#[test]
fn dependency_references_use_the_owning_module_scope() {
    let project = Project::new();
    project.file("a.simf", "pub fn target() {}\n");
    let second_dependency = project.file(
        "b.simf",
        "pub fn target() {}\npub fn wrapper() { target(); }\n",
    );
    let root = project.file(
        "main.simf",
        "use crate::a::target;\nuse crate::b::wrapper;\nfn main() { target(); wrapper(); }\n",
    );
    let mut state = WorkspaceState::default();
    store(&mut state, &project, &root, &root.source, Some(1));

    let references = state.find_references_to(&identity(&state, &root.uri, "target"));

    assert_eq!(references.len(), 1);
    assert_eq!(references[0].uri, root.uri);
    assert!(!references
        .iter()
        .any(|location| location.uri == second_dependency.uri));
}
