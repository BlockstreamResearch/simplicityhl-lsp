use tower_lsp_server::lsp_types::{Diagnostic, Uri};

use super::diagnostics::DiagnosticBundle;
use super::{AnalysisGeneration, DiagnosticUpdate, RootAnalysis, WorkspaceState};
use crate::analysis::AnalysisSnapshot;

impl WorkspaceState {
    pub(super) fn store_root(
        &mut self,
        uri: &Uri,
        snapshot: AnalysisSnapshot,
        version: Option<i32>,
        generation: AnalysisGeneration,
    ) -> Vec<DiagnosticUpdate> {
        let diagnostics = DiagnosticBundle::from_snapshot(&snapshot);
        let mut affected = self.reported_files(uri);

        affected.extend(diagnostics.uris().cloned());
        affected.push(uri.clone());

        self.roots.insert(
            uri.clone(),
            RootAnalysis {
                snapshot,
                diagnostics,
                generation,
                version,
            },
        );
        self.updates(affected)
    }

    pub(super) fn remove(&mut self, uri: &Uri) -> Vec<DiagnosticUpdate> {
        let mut affected = self.reported_files(uri);
        self.roots.remove(uri);

        affected.push(uri.clone());
        self.updates(affected)
    }

    /// Files the current root analysis of `uri` has published diagnostics to.
    fn reported_files(&self, uri: &Uri) -> Vec<Uri> {
        self.roots
            .get(uri)
            .map(|root| root.diagnostics.uris().cloned().collect())
            .unwrap_or_default()
    }

    fn updates(&self, mut affected: Vec<Uri>) -> Vec<DiagnosticUpdate> {
        affected.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        affected.dedup();
        affected
            .into_iter()
            .filter(|uri| !self.own_analysis_pending(uri))
            .map(|uri| DiagnosticUpdate {
                diagnostics: self.diagnostics_for(&uri),
                version: self.roots.get(&uri).and_then(|root| root.version),
                uri,
            })
            .collect()
    }

    fn own_analysis_pending(&self, uri: &Uri) -> bool {
        let Some(document) = self.documents.get(uri) else {
            return false;
        };

        match (&document.text, self.roots.get(uri)) {
            (Some(_), Some(root)) => root.generation != document.generation,
            (Some(_), None) | (None, Some(_)) => true,
            (None, None) => false,
        }
    }

    fn diagnostics_for(&self, uri: &Uri) -> Vec<Diagnostic> {
        // An open file's unsaved analysis is authoritative over saved dependency copies.
        if let Some(root) = self.roots.get(uri) {
            return root
                .diagnostics
                .get(uri)
                .map(<[Diagnostic]>::to_vec)
                .unwrap_or_default();
        }

        // Otherwise merge what every open root reported for it, in a stable order.
        let mut origins = self.roots.iter().collect::<Vec<_>>();
        origins.sort_unstable_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        let mut diagnostics = Vec::new();

        for (_, root) in origins {
            let Some(contributed) = root.diagnostics.get(uri) else {
                continue;
            };

            for diag in contributed {
                if !diagnostics.contains(diag) {
                    diagnostics.push(diag.clone());
                }
            }
        }

        diagnostics
    }
}
