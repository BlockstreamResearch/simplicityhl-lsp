pub(crate) mod diagnostics;
mod publication;
#[cfg(test)]
mod tests;
mod tickets;

use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp_server::lsp_types::{Diagnostic, Location, Uri};

use crate::analysis::AnalysisSnapshot;
use crate::navigation::FunctionIdentity;

use self::diagnostics::DiagnosticBundle;

/// Server-side counter of editor events for one document; see [`DocumentState::generation`].
pub(crate) type AnalysisGeneration = u64;

/// What the server knows about one document the editor opened.
#[derive(Clone, Debug, Default)]
struct DocumentState {
    /// Increased on every open, change, save, close and reanalysis of this document.
    generation: AnalysisGeneration,
    /// Latest text, or `None` after the editor closed the document.
    text: Option<Arc<str>>,
    /// The editor's number for this text: rejects edits that arrive out of order and
    /// tells the editor which text published diagnostics belong to.
    version: Option<i32>,
}

/// The latest finished analysis of one open document.
#[derive(Debug)]
struct RootAnalysis {
    snapshot: AnalysisSnapshot,
    /// Diagnostics this analysis produced, grouped by the file they belong to.
    diagnostics: DiagnosticBundle,
    /// The ticket this analysis started with, compared with [`DocumentState::generation`]
    /// to tell whether a newer analysis of the same document is still pending.
    generation: AnalysisGeneration,
    version: Option<i32>,
}

/// A ticket: everything one analysis needs, captured under the workspace lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AnalysisInput {
    pub uri: Uri,
    pub text: Arc<str>,
    pub version: Option<i32>,
    pub generation: AnalysisGeneration,
}

/// One `publishDiagnostics` message.
#[derive(Debug)]
pub struct DiagnosticUpdate {
    pub uri: Uri,
    pub diagnostics: Vec<Diagnostic>,
    pub version: Option<i32>,
}

/// Open documents, their finished analyses, and the policy for publishing diagnostics across them.
#[derive(Debug, Default)]
pub struct WorkspaceState {
    roots: HashMap<Uri, RootAnalysis>,
    documents: HashMap<Uri, DocumentState>,
}

impl WorkspaceState {
    /// The latest finished analysis of an open document.
    pub fn get(&self, uri: &Uri) -> Option<&AnalysisSnapshot> {
        self.roots.get(uri).map(|root| &root.snapshot)
    }

    /// The latest finished analysis of every open document.
    pub fn values(&self) -> impl Iterator<Item = &AnalysisSnapshot> {
        self.roots.values().map(|root| &root.snapshot)
    }

    /// Latest text of an open document, including one whose analysis failed.
    pub(crate) fn text(&self, uri: &Uri) -> Option<Arc<str>> {
        self.documents.get(uri)?.text.clone()
    }

    pub(crate) fn find_references_to(&self, target: &FunctionIdentity) -> Vec<Location> {
        let mut locations = self
            .values()
            .filter_map(|snapshot| snapshot.find_references_to(target).ok())
            .flatten()
            .collect::<Vec<_>>();

        // By file, then by range.
        locations.sort_unstable_by(|left, right| {
            let left_key = (left.uri.as_str(), left.range.start, left.range.end);
            let right_key = (right.uri.as_str(), right.range.start, right.range.end);
            left_key.cmp(&right_key)
        });
        locations.dedup();
        locations
    }
}
