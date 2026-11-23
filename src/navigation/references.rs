use std::collections::HashSet;

use miniscript::iter::TreeLike;
use simplicityhl::parse::{self, CallName};
use tower_lsp_server::lsp_types;

use crate::analysis::AnalysisSnapshot;
use crate::error::LspError;
use crate::text::{get_call_span, position_to_span, span_contains, span_to_positions};

/// What a references request should search for.
pub(crate) enum ReferenceTarget<'a> {
    /// A jet or built-in call: every call with this name in the same document.
    Name(&'a CallName),
    /// A function defined in code: calls that resolve to it, in every open document.
    Function(FunctionIdentity),
}

/// Stable identity for one function definition across independently analyzed roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FunctionIdentity {
    uri: lsp_types::Uri,
    start: usize,
    end: usize,
}

impl AnalysisSnapshot {
    /// The call or function definition name at `position`.
    pub(crate) fn reference_target(
        &self,
        position: lsp_types::Position,
    ) -> Result<Option<ReferenceTarget<'_>>, LspError> {
        let token_span = position_to_span(position, &self.text)?;

        let function = match self.find_related_call(token_span).map(parse::Call::name) {
            Some(CallName::Custom(name)) => self.functions.get_func(name.as_str()),
            Some(name) => return Ok(Some(ReferenceTarget::Name(name))),
            None => {
                let Some(function) = self
                    .functions
                    .iter()
                    .find(|func| span_contains(func.span(), &token_span))
                else {
                    return Ok(None);
                };

                let range = self.find_function_name_range(function)?;
                if !(range.start..=range.end).contains(&position) {
                    return Ok(None);
                }

                Some(function)
            }
        };

        Ok(function
            .and_then(|func| self.function_identity(func))
            .map(ReferenceTarget::Function))
    }

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
        // Keyed by the URI string.
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
            |function, call| match call.name() {
                CallName::Custom(name) => self
                    .resolve_custom_call(function, name.as_str())
                    .and_then(|resolved| self.function_identity(resolved))
                    .is_some_and(|identity| &identity == target),
                _ => false,
            },
        )
    }

    fn reference_locations(
        &self,
        mut include_function: impl FnMut(&parse::Function) -> bool,
        mut include_call: impl FnMut(&parse::Function, &parse::Call) -> bool,
    ) -> Result<Vec<lsp_types::Location>, LspError> {
        let mut locations = Vec::new();
        for function in self
            .functions
            .iter()
            .filter(|function| include_function(function))
        {
            let Some(source) = self.sources.get(function.span().file_id) else {
                continue;
            };

            for expression in parse::ExprTree::Expression(function.body()).pre_order_iter() {
                let parse::ExprTree::Call(call) = expression else {
                    continue;
                };

                if include_call(function, call) {
                    let (start, end) = span_to_positions(&get_call_span(call), &source.text)?;

                    locations.push(lsp_types::Location::new(
                        source.uri.clone(),
                        lsp_types::Range::new(start, end),
                    ));
                }
            }
        }

        Ok(locations)
    }
}
