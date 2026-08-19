use std::collections::HashSet;

use miniscript::iter::TreeLike;
use simplicityhl::parse::{self, CallName};
use tower_lsp_server::lsp_types::{self, Uri};

use crate::analysis::AnalysisSnapshot;
use crate::error::LspError;
use crate::utils::{get_call_span, offset_to_position, span_contains, span_to_positions};

/// Stable identity for one function definition across independently analyzed roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FunctionIdentity {
    uri: Uri,
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
        self.functions
            .functions()
            .iter()
            .filter_map(|function| {
                let source = self.sources.get(function.span().file_id)?;
                Some(
                    parse::ExprTree::Expression(function.body())
                        .pre_order_iter()
                        .filter_map(|expression| match expression {
                            parse::ExprTree::Call(call) => Some((call, get_call_span(call))),
                            _ => None,
                        })
                        .filter(|(call, _)| call.name() == call_name)
                        .map(|(_, span)| (span, source))
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

    /// Find calls whose locally visible name resolves to the requested definition.
    pub(crate) fn find_references_to(
        &self,
        target: &FunctionIdentity,
    ) -> Result<Vec<lsp_types::Location>, LspError> {
        let mut seen_functions = HashSet::new();
        self.functions
            .functions()
            .iter()
            .filter_map(|function| {
                let identity = self.function_identity(function)?;
                let key = (
                    identity.uri.as_str().to_owned(),
                    identity.start,
                    identity.end,
                );
                if !seen_functions.insert(key) {
                    return None;
                }
                let source = self.sources.get(function.span().file_id)?;
                Some(
                    parse::ExprTree::Expression(function.body())
                        .pre_order_iter()
                        .filter_map(|expression| match expression {
                            parse::ExprTree::Call(call) => Some((call, get_call_span(call))),
                            _ => None,
                        })
                        .filter(|(call, _)| {
                            let CallName::Custom(name) = call.name() else {
                                return false;
                            };
                            self.resolve_custom_call(function, name.as_inner())
                                .and_then(|resolved| self.function_identity(resolved))
                                .as_ref()
                                == Some(target)
                        })
                        .map(|(_, span)| (span, source))
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

    pub fn find_function_name_range(
        &self,
        function: &parse::Function,
    ) -> Result<lsp_types::Range, LspError> {
        let function_span = function.span();
        let source = match function_span.file_id {
            0 => &self.text,
            file_id => {
                &self
                    .sources
                    .get(file_id)
                    .ok_or_else(|| {
                        LspError::FunctionNotFound(format!(
                            "Source file for function {} not found",
                            function.name()
                        ))
                    })?
                    .text
            }
        };
        let function_source = source
            .get_byte_slice(function_span.start..function_span.end)
            .ok_or_else(|| {
                LspError::FunctionNotFound(format!(
                    "Source span for function {} is outside its document",
                    function.name()
                ))
            })?
            .to_string();
        let (tokens, _) = simplicityhl::lexer::lex(function_span.file_id, &function_source, 0);
        let Some(tokens) = tokens else {
            return Err(LspError::FunctionNotFound(format!(
                "Function with name {} not found",
                function.name()
            )));
        };
        let name_span = tokens
            .windows(2)
            .find_map(|pair| match pair {
                [
                    (simplicityhl::lexer::Token::Fn, _),
                    (simplicityhl::lexer::Token::Ident(name), span),
                ] if *name == function.name().as_inner() => Some(*span),
                _ => None,
            })
            .ok_or_else(|| {
                LspError::FunctionNotFound(format!(
                    "Function with name {} not found inside its source span",
                    function.name()
                ))
            })?;

        Ok(lsp_types::Range {
            start: offset_to_position(function_span.start + name_span.start, source)?,
            end: offset_to_position(function_span.start + name_span.end, source)?,
        })
    }

    /// Resolve the imported function named by a cursor inside a `use` declaration.
    pub fn find_imported_function(
        &self,
        token_span: simplicityhl::error::Span,
    ) -> Option<&parse::Function> {
        let use_decl = self
            .use_declarations
            .iter()
            .filter(|use_decl| span_contains(use_decl.span(), &token_span))
            .min_by_key(|use_decl| use_decl.span().end - use_decl.span().start)?;

        let source = self.text.to_string();
        let tokens = simplicityhl::lexer::lex(0, &source, 0).0?;
        let identifiers = tokens
            .iter()
            .filter(|(_, span)| {
                span.start >= use_decl.span().start && span.end <= use_decl.span().end
            })
            .skip_while(|(token, _)| !matches!(token, simplicityhl::lexer::Token::Use))
            .skip(1)
            .filter(|(token, _)| {
                matches!(
                    token,
                    simplicityhl::lexer::Token::Crate | simplicityhl::lexer::Token::Ident(_)
                )
            })
            .collect::<Vec<_>>();
        let selected_index = identifiers
            .iter()
            .position(|(_, span)| span_contains(span, &token_span))?;

        let mut item_index = use_decl.path().len();
        let items = match use_decl.items() {
            parse::UseItems::Single(item) => std::slice::from_ref(item),
            parse::UseItems::List(items) => items.as_slice(),
        };
        for (original, alias) in items {
            let selected_original = selected_index == item_index;
            item_index += 1;
            let selected_alias = alias.is_some() && selected_index == item_index;
            if alias.is_some() {
                item_index += 1;
            }
            if selected_original || selected_alias {
                return self
                    .functions
                    .get_func(alias.as_ref().unwrap_or(original).as_inner());
            }
        }
        None
    }

    /// Find the smallest call whose callable span contains the requested source position.
    pub fn find_related_call(
        &self,
        token_span: simplicityhl::error::Span,
    ) -> Option<&simplicityhl::parse::Call> {
        let function = self.functions.functions().into_iter().find(|function| {
            function.span().file_id == 0 && span_contains(function.span(), &token_span)
        })?;

        parse::ExprTree::Expression(function.body())
            .pre_order_iter()
            .filter_map(|expression| match expression {
                parse::ExprTree::Call(call) => Some((call, get_call_span(call))),
                _ => None,
            })
            .filter(|(_, span)| span_contains(span, &token_span))
            .map(|(call, _)| call)
            .last()
    }
}
