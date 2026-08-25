use std::collections::HashSet;

use miniscript::iter::TreeLike;
use simplicityhl::parse::{self, CallName};
use tower_lsp_server::lsp_types;

use crate::analysis::AnalysisSnapshot;
use crate::error::LspError;
use crate::text::{get_call_span, span_to_positions};

/// Stable identity for one function definition across independently analyzed roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FunctionIdentity {
    uri: lsp_types::Uri,
    start: usize,
    end: usize,
}

impl AnalysisSnapshot {
    pub(crate) fn function_identity(&self, function: &parse::Function) -> Option<FunctionIdentity> {
        let span = function.span();
        let source = self.sources.get(span.file_id)?;
        Some(FunctionIdentity {
            uri: source.uri.clone(),
            start: span.start,
            end: span.end,
        })
    }

    pub fn find_all_references(
        &self,
        call_name: &CallName,
    ) -> Result<Vec<lsp_types::Location>, LspError> {
        self.reference_locations(|_| true, |_, call| call.name() == call_name)
    }

    /// Find calls whose locally visible name resolves to the requested definition.
    pub(crate) fn find_references_to(
        &self,
        target: &FunctionIdentity,
    ) -> Result<Vec<lsp_types::Location>, LspError> {
        let mut seen = HashSet::new();
        self.reference_locations(
            |function| {
                self.function_identity(function).is_some_and(|identity| {
                    seen.insert((
                        identity.uri.as_str().to_owned(),
                        identity.start,
                        identity.end,
                    ))
                })
            },
            |function, call| {
                let CallName::Custom(name) = call.name() else {
                    return false;
                };
                self.resolve_custom_call(function, name.as_inner())
                    .and_then(|resolved| self.function_identity(resolved))
                    .as_ref()
                    == Some(target)
            },
        )
    }

    fn reference_locations(
        &self,
        mut include_function: impl FnMut(&parse::Function) -> bool,
        mut include_call: impl FnMut(&parse::Function, &parse::Call) -> bool,
    ) -> Result<Vec<lsp_types::Location>, LspError> {
        self.functions
            .iter()
            .filter(|function| include_function(function))
            .filter_map(|function| {
                let source = self.sources.get(function.span().file_id)?;
                Some(
                    parse::ExprTree::Expression(function.body())
                        .pre_order_iter()
                        .filter_map(|expression| match expression {
                            parse::ExprTree::Call(call) if include_call(function, call) => {
                                Some(get_call_span(call))
                            }
                            _ => None,
                        })
                        .map(|span| (span, source))
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .map(|(span, source)| {
                let (start, end) = span_to_positions(&span, &source.text)?;
                Ok(lsp_types::Location {
                    range: lsp_types::Range { start, end },
                    uri: source.uri.clone(),
                })
            })
            .collect()
    }
}
