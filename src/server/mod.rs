mod capabilities;
mod handlers;
mod transaction;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp_server::Client;

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

#[cfg(test)]
mod tests;
