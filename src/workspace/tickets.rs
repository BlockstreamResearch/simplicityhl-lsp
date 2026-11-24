use std::sync::Arc;

use tower_lsp_server::lsp_types::{Diagnostic, Uri};

use super::{AnalysisGeneration, AnalysisInput, DiagnosticUpdate, DocumentState, WorkspaceState};
use crate::analysis::AnalysisSnapshot;

impl DocumentState {
    /// Record a new event for this document and return its generation.
    fn advance(&mut self) -> AnalysisGeneration {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Record a new event and capture the current text as the input of an analysis.
    /// `None` if the document is closed.
    fn ticket(&mut self, uri: &Uri) -> Option<AnalysisInput> {
        Some(AnalysisInput {
            uri: uri.clone(),
            text: Arc::clone(self.text.as_ref()?),
            version: self.version,
            generation: self.advance(),
        })
    }
}

impl WorkspaceState {
    /// `didOpen`: store the text and return a ticket to analyze it.
    pub(crate) fn begin_open(
        &mut self,
        uri: &Uri,
        text: &str,
        version: Option<i32>,
    ) -> AnalysisInput {
        let state = self.documents.entry(uri.clone()).or_default();
        state.text = Some(Arc::from(text));
        state.version = version;
        state.ticket(uri).expect("an opened document has text")
    }

    /// `didChange` or `didSave`: store the new text and return a ticket to analyze it.
    ///
    /// `None` if the document is not open, or if `version` is older than the stored one
    pub(crate) fn begin_change(
        &mut self,
        uri: &Uri,
        text: &str,
        version: Option<i32>,
    ) -> Option<AnalysisInput> {
        let state = self.documents.get_mut(uri)?;
        state.text.as_ref()?;

        if matches!(
            (version, state.version),
            (Some(incoming), Some(current)) if incoming < current
        ) {
            return None;
        }

        state.text = Some(Arc::from(text));
        if version.is_some() {
            state.version = version;
        }
        state.ticket(uri)
    }

    pub(crate) fn begin_reanalysis(&mut self) -> Vec<AnalysisInput> {
        self.documents
            .iter_mut()
            .filter_map(|(uri, state)| state.ticket(uri))
            .collect()
    }

    /// `didClose`: forget the text and return the generation that `remove_if_current`
    /// must match. `None` if the document is not open.
    pub(crate) fn begin_close(&mut self, uri: &Uri) -> Option<AnalysisGeneration> {
        let state = self.documents.get_mut(uri)?;
        state.text.as_ref()?;
        state.text = None;
        state.version = None;
        Some(state.advance())
    }

    /// Store a finished analysis if its ticket is still
    /// current, and return the diagnostics to publish.
    pub(crate) fn replace_if_current(
        &mut self,
        uri: &Uri,
        snapshot: AnalysisSnapshot,
        version: Option<i32>,
        generation: AnalysisGeneration,
    ) -> Option<Vec<DiagnosticUpdate>> {
        self.is_current(uri, generation, true)
            .then(|| self.store_root(uri, snapshot, version, generation))
    }

    /// Drop the analysis of a closed document if no
    /// newer event happened since `begin_close`.
    pub(crate) fn remove_if_current(
        &mut self,
        uri: &Uri,
        generation: AnalysisGeneration,
    ) -> Option<Vec<DiagnosticUpdate>> {
        self.is_current(uri, generation, false)
            .then(|| self.remove(uri))
    }

    pub(crate) fn diagnostics_if_current(
        &self,
        uri: Uri,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
        generation: AnalysisGeneration,
    ) -> Option<Vec<DiagnosticUpdate>> {
        self.is_current(&uri, generation, true).then(|| {
            vec![DiagnosticUpdate {
                uri,
                diagnostics,
                version,
            }]
        })
    }

    fn is_current(&self, uri: &Uri, generation: AnalysisGeneration, open: bool) -> bool {
        // `state.text.is_some() == open` is need to understand that currently we inside file
        // and we can get text from it. If it is not true, then code can break because we do not load the text.
        // Currently this situation is impossible, but it's safe guard for future.
        self.documents
            .get(uri)
            .is_some_and(|state| state.generation == generation && state.text.is_some() == open)
    }
}
