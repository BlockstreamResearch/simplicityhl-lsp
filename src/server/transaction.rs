use std::future::Future;

use tokio::sync::{Mutex, RwLock};

use crate::workspace::{DiagnosticUpdate, WorkspaceState};

/// Serializes one workspace diagnostic transition with its complete publication batch.
#[derive(Debug, Default)]
pub(super) struct DiagnosticTransaction {
    gate: Mutex<()>,
}

impl DiagnosticTransaction {
    pub(super) async fn run<F, P, Fut>(
        &self,
        workspace: &RwLock<WorkspaceState>,
        transition: F,
        publish: P,
    ) where
        F: FnOnce(&mut WorkspaceState) -> Option<Vec<DiagnosticUpdate>>,
        P: FnOnce(Vec<DiagnosticUpdate>) -> Fut,
        Fut: Future<Output = ()>,
    {
        let _transaction = self.gate.lock().await;
        let updates = {
            let mut workspace = workspace.write().await;
            transition(&mut workspace)
        };
        if let Some(updates) = updates {
            publish(updates).await;
        }
    }
}
