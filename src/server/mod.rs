mod capabilities;
mod handlers;
#[cfg(test)]
mod tests;
mod transaction;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp_server::lsp_types::{WorkspaceFolder, WorkspaceFoldersChangeEvent};
use tower_lsp_server::{Client, UriExt};

use self::transaction::DiagnosticTransaction;
use crate::completion::CompletionProvider;
use crate::config::Settings;
use crate::workspace::WorkspaceState;

/// Client-supplied configuration, kept separate from the document cache so a
/// settings change does not need the document lock.
#[derive(Debug, Default)]
struct ServerConfig {
    settings: Settings,
    workspace_roots: Vec<PathBuf>,
    watched_files_registration: bool,
}

impl ServerConfig {
    /// Apply a workspace-folder change, keeping each root at most once.
    fn apply_folder_change(&mut self, event: WorkspaceFoldersChangeEvent) {
        let removed = folder_paths(event.removed).collect::<Vec<_>>();
        self.workspace_roots.retain(|root| !removed.contains(root));

        for path in folder_paths(event.added) {
            if !self.workspace_roots.contains(&path) {
                self.workspace_roots.push(path);
            }
        }
    }
}

fn folder_paths(folders: Vec<WorkspaceFolder>) -> impl Iterator<Item = PathBuf> {
    folders
        .into_iter()
        .filter_map(|folder| folder.uri.to_file_path().map(Cow::into_owned))
}

/// State shared by every LSP handler for the whole session, plus the
/// `client` used to send messages back to the editor.
#[derive(Debug)]
pub struct Backend {
    client: Client,
    workspace: Arc<RwLock<WorkspaceState>>,
    diagnostic_transaction: DiagnosticTransaction,
    config: Arc<RwLock<ServerConfig>>,
    completion_provider: CompletionProvider,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(RwLock::new(WorkspaceState::default())),
            diagnostic_transaction: DiagnosticTransaction::default(),
            config: Arc::new(RwLock::new(ServerConfig::default())),
            completion_provider: CompletionProvider::new(),
        }
    }
}
