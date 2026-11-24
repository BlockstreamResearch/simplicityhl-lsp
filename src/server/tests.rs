use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc as StdArc, Mutex as StdMutex};

use smplx_build::CONFIG_FILENAME;
use tempfile::TempDir;
use tokio::sync::Notify;
use tower_lsp_server::lsp_types::Uri;
use tower_lsp_server::UriExt;

use super::*;
use crate::analysis::AnalysisSnapshot;
use crate::config::Settings;
use crate::workspace::{AnalysisInput, DiagnosticUpdate, WorkspaceState};

#[test]
fn watched_file_registration_matches_the_protocol_contract() {
    assert_eq!(
        serde_json::to_value(super::capabilities::watched_files()).unwrap(),
        serde_json::json!([
            { "globPattern": "**/*.simf" },
            { "globPattern": format!("**/{CONFIG_FILENAME}") },
        ])
    );
}

type WitnessPublications = StdArc<StdMutex<Vec<(Option<i32>, Vec<String>)>>>;

async fn capture_witness_update(
    transaction: &DiagnosticTransaction,
    workspace: &RwLock<WorkspaceState>,
    input: AnalysisInput,
    publications: &WitnessPublications,
) {
    let diagnostics = crate::witness::validate(&input.text);
    transaction
        .run(
            workspace,
            |workspace| {
                workspace.diagnostics_if_current(
                    input.uri,
                    diagnostics,
                    input.version,
                    input.generation,
                )
            },
            {
                let publications = StdArc::clone(publications);
                move |updates| async move {
                    let update = &updates[0];
                    publications.lock().unwrap().push((
                        update.version,
                        update
                            .diagnostics
                            .iter()
                            .map(|item| item.message.clone())
                            .collect(),
                    ));
                }
            },
        )
        .await;
}

#[tokio::test]
async fn stale_witness_diagnostics_cannot_overwrite_a_newer_generation() {
    let uri = Uri::from_file_path(std::env::temp_dir().join("contract.wit")).unwrap();
    let workspace = RwLock::new(WorkspaceState::default());
    let transaction = DiagnosticTransaction::default();
    let publications = StdArc::new(StdMutex::new(Vec::new()));
    let invalid = r#"{"amount":{"value":1}}"#;
    let stale = workspace.write().await.begin_open(&uri, invalid, Some(1));

    let valid = r#"{"amount":{"value":1,"type":"u32"}}"#;
    let current = workspace
        .write()
        .await
        .begin_change(&uri, valid, Some(2))
        .expect("open witness change");
    capture_witness_update(&transaction, &workspace, current, &publications).await;
    capture_witness_update(&transaction, &workspace, stale, &publications).await;

    assert!(workspace.read().await.get(&uri).is_none());
    assert_eq!(*publications.lock().unwrap(), [(Some(2), Vec::new())]);
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
        fs::write(temp.path().join("Simplex.toml"), "").expect("write manifest");
        fs::create_dir(temp.path().join("simf")).expect("create source dir");

        let dependency_path = temp.path().join("simf/shared.simf");
        let root_path = temp.path().join("simf/main.simf");
        fs::write(&dependency_path, dependency_source).expect("write dependency");
        fs::write(&root_path, IMPORTING_ROOT).expect("write root");

        let root_uri =
            Uri::from_file_path(fs::canonicalize(&root_path).expect("canonical root path"))
                .expect("root URI");
        let dependency_uri = Uri::from_file_path(
            fs::canonicalize(&dependency_path).expect("canonical dependency path"),
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
        fs::write(&self.dependency_path, source).expect("write dependency");
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

fn workspace_folder(uri: Uri) -> tower_lsp_server::lsp_types::WorkspaceFolder {
    tower_lsp_server::lsp_types::WorkspaceFolder {
        uri,
        name: "folder".to_string(),
    }
}

#[test]
fn folder_change_removes_adds_and_deduplicates_roots() {
    use tower_lsp_server::lsp_types::WorkspaceFoldersChangeEvent;

    let (kept, removed, added) = (
        std::env::temp_dir().join("kept"),
        std::env::temp_dir().join("removed"),
        std::env::temp_dir().join("added"),
    );

    let mut config = ServerConfig {
        workspace_roots: vec![kept.clone(), removed.clone()],
        ..ServerConfig::default()
    };

    config.apply_folder_change(WorkspaceFoldersChangeEvent {
        added: [&added, &added, &kept]
            .into_iter()
            .map(|path| workspace_folder(Uri::from_file_path(path).unwrap()))
            .collect(),
        removed: vec![workspace_folder(Uri::from_file_path(&removed).unwrap())],
    });

    assert_eq!(config.workspace_roots, [kept, added]);
}
