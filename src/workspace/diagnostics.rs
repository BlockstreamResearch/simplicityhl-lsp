//! Conversion and aggregation inputs for compiler diagnostics owned by workspace state.

use std::collections::HashMap;

use simplicityhl::error::{
    Diagnostic as CompilerDiagnostic, Error as CompilerError, Location as CompilerLocation,
    Severity as CompilerSeverity,
};
use tower_lsp_server::lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, Range, Uri,
};

use crate::analysis::AnalysisSnapshot;
use crate::text::span_to_positions;

/// Diagnostics produced by one analysis root, grouped by the source that owns each range.
#[derive(Debug, Default)]
pub struct DiagnosticBundle(HashMap<Uri, Vec<Diagnostic>>);

impl DiagnosticBundle {
    pub fn from_snapshot(snapshot: &AnalysisSnapshot) -> Self {
        let mut bundle = Self::default();

        for diagnostic in snapshot
            .compiler_diagnostics
            .iter()
            .filter(|diagnostic| !hidden(diagnostic))
        {
            let (source, range) = match diagnostic.location() {
                CompilerLocation::Code(span) => {
                    let Some(source) = snapshot.sources.get(span.file_id) else {
                        continue;
                    };
                    let Ok((start, end)) = span_to_positions(span, &source.text) else {
                        continue;
                    };
                    (source, Range::new(start, end))
                }
                CompilerLocation::File(file_id) => {
                    let Some(source) = snapshot.sources.get(*file_id) else {
                        continue;
                    };
                    (source, Range::default())
                }
                CompilerLocation::Global => (snapshot.sources.root_source(), Range::default()),
            };

            let related_information = diagnostic
                .secondary()
                .iter()
                .filter_map(|label| {
                    let related_source = snapshot.sources.get(label.span.file_id)?;
                    let (start, end) = span_to_positions(&label.span, &related_source.text).ok()?;
                    Some(DiagnosticRelatedInformation {
                        location: Location::new(related_source.uri.clone(), Range::new(start, end)),
                        message: label.message.clone(),
                    })
                })
                .collect::<Vec<_>>();

            bundle
                .0
                .entry(source.uri.clone())
                .or_default()
                .push(Diagnostic {
                    range,
                    severity: Some(match diagnostic.severity() {
                        CompilerSeverity::Error => DiagnosticSeverity::ERROR,
                        CompilerSeverity::Warning => DiagnosticSeverity::WARNING,
                    }),
                    source: Some("simplicityhl".to_string()),
                    message: message(diagnostic),
                    related_information: (!related_information.is_empty())
                        .then_some(related_information),
                    ..Diagnostic::default()
                });
        }

        bundle
    }

    pub fn get(&self, uri: &Uri) -> Option<&[Diagnostic]> {
        self.0.get(uri).map(Vec::as_slice)
    }

    pub fn uris(&self) -> impl Iterator<Item = &Uri> {
        self.0.keys()
    }
}

fn hidden(diagnostic: &CompilerDiagnostic) -> bool {
    match diagnostic.error() {
        CompilerError::MainRequired => true,
        CompilerError::CannotParse { msg }
            if msg == &CompilerError::MainOutOfEntryFile.to_string() =>
        {
            true
        }
        _ => false,
    }
}

fn message(diagnostic: &CompilerDiagnostic) -> String {
    let mut message = diagnostic.error().to_string();
    for note in diagnostic.notes() {
        message.push_str("\n\nNote: ");
        message.push_str(note);
    }
    if let Some(help) = diagnostic.help() {
        message.push_str("\n\nHelp: ");
        message.push_str(help);
    }
    message
}

#[cfg(test)]
mod tests;
