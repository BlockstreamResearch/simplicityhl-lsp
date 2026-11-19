use super::{finish, open, temporary_uri, Project};
use crate::workspace::WorkspaceState;

#[test]
fn close_generation_rejects_an_in_flight_analysis() {
    let project = Project::new();
    let file = project.file("main.simf", "fn main() {}\n");
    let mut state = WorkspaceState::default();

    let ticket = state.begin_open(&file.uri, &file.source, Some(1));
    let close_generation = state.begin_close(&file.uri).expect("close ticket");

    assert!(finish(&mut state, &project, &ticket).is_none());
    assert!(state
        .remove_if_current(&file.uri, close_generation)
        .is_some());
    assert!(state.get(&file.uri).is_none());
}

#[test]
fn newer_generation_wins_when_document_versions_are_equal() {
    let project = Project::new();
    let old_source = "fn old() {}\nfn main() {}\n";
    let file = project.file("main.simf", "fn new() {}\nfn main() {}\n");
    let mut state = WorkspaceState::default();

    let old_ticket = state.begin_open(&file.uri, old_source, Some(1));
    let new_ticket = state
        .begin_change(&file.uri, &file.source, Some(1))
        .expect("open document");

    assert!(finish(&mut state, &project, &new_ticket).is_some());
    assert!(finish(&mut state, &project, &old_ticket).is_none());
    assert_eq!(
        state
            .get(&file.uri)
            .expect("current analysis")
            .text
            .to_string(),
        file.source
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
    let project = Project::new();
    let file = project.file("main.simf", "fn main() {}\n");
    let mut state = WorkspaceState::default();

    let initial = state.begin_open(&file.uri, &file.source, Some(1));
    assert!(state.get(&file.uri).is_none());

    let requests = state.begin_reanalysis();
    assert_eq!(requests.len(), 1);

    let request = &requests[0];
    assert_eq!(request.uri, file.uri);
    assert_eq!(request.text.as_ref(), file.source);
    assert_eq!(request.version, Some(1));
    assert_ne!(request.generation, initial.generation);
}

#[test]
fn pre_close_reanalysis_cannot_overwrite_a_reopened_document() {
    let project = Project::new();
    let old_source = "fn old() {}\nfn main() {}\n";
    let file = project.file("main.simf", "fn new() {}\nfn main() {}\n");
    let mut state = WorkspaceState::default();

    open(&mut state, &project, &file, old_source, 5);
    let stale_request = state.begin_reanalysis().pop().expect("reanalysis request");
    let close_generation = state.begin_close(&file.uri).expect("close ticket");
    let reopened = state.begin_open(&file.uri, &file.source, Some(1));

    assert!(state
        .remove_if_current(&file.uri, close_generation)
        .is_none());
    assert!(finish(&mut state, &project, &stale_request).is_none());
    assert!(finish(&mut state, &project, &reopened).is_some());
    assert_eq!(
        state
            .get(&file.uri)
            .expect("reopened analysis")
            .text
            .to_string(),
        file.source
    );
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
    assert!(document.text.is_none());
    assert_eq!(document.version, None);
    assert!(state.begin_close(&uri).is_none());

    let unknown = temporary_uri("unknown-close.simf");
    assert!(state.begin_close(&unknown).is_none());
    assert!(!state.documents.contains_key(&unknown));
}
