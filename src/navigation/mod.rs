mod references;
mod symbols;
#[cfg(test)]
mod tests;

pub(crate) use references::{FunctionIdentity, ReferenceTarget};

use miniscript::iter::TreeLike;
use simplicityhl::parse::CallName;
use simplicityhl::{error, lexer, parse};
use tower_lsp_server::lsp_types::{self, GotoDefinitionResponse, Location, Position, Range, Uri};

use crate::analysis::{use_items, AnalysisSnapshot};
use crate::error::LspError;
use crate::text::{get_call_span, offset_to_position, span_contains, span_to_positions};

impl AnalysisSnapshot {
    pub fn definition_at(
        &self,
        uri: &Uri,
        token_position: Position,
    ) -> Result<Option<GotoDefinitionResponse>, LspError> {
        let token_span = crate::text::position_to_span(token_position, &self.text)?;

        // The cursor is on an item in a `use` line.
        if let Some(function) = self.find_imported_function(token_span) {
            return self.definition_of(function);
        }

        // The cursor is on a function's own name.
        let Some(call) = self.find_related_call(token_span) else {
            let Some(function) = self
                .functions
                .iter()
                .find(|function| span_contains(function.span(), &token_span))
            else {
                return Ok(None);
            };

            let range = self.find_function_name_range(function)?;
            return Ok((range.start..=range.end)
                .contains(&token_position)
                .then(|| Location::new(uri.clone(), range).into()));
        };

        // The cursor is on a call.
        match call.name() {
            CallName::Custom(name) => self
                .functions
                .get_func(name.as_str())
                .map_or(Ok(None), |function| self.definition_of(function)),
            _ => Ok(None),
        }
    }

    /// The whole definition of `function`, in the file it belongs to.
    fn definition_of(
        &self,
        function: &parse::Function,
    ) -> Result<Option<GotoDefinitionResponse>, LspError> {
        let Some(source) = self.sources.get(function.span().file_id) else {
            return Ok(None);
        };

        let (start, end) = span_to_positions(function.as_ref(), &source.text)?;
        Ok(Some(
            Location::new(source.uri.clone(), Range::new(start, end)).into(),
        ))
    }

    pub fn find_function_name_range(
        &self,
        function: &parse::Function,
    ) -> Result<lsp_types::Range, LspError> {
        let name = function.name();
        let span = function.span();

        // The root's text is `self.text`, which every feature converts root positions against.
        let source = match span.file_id {
            0 => &self.text,
            file_id => {
                &self
                    .sources
                    .get(file_id)
                    .ok_or_else(|| {
                        LspError::FunctionNotFound(format!(
                            "Source file for function {name} not found"
                        ))
                    })?
                    .text
            }
        };
        let function_source = source
            .get_byte_slice(span.start..span.end)
            .ok_or_else(|| {
                LspError::FunctionNotFound(format!(
                    "Source span for function {name} is outside its document"
                ))
            })?
            .to_string();

        // The parse tree has no span for the name, so find the `fn <name>` tokens in the source.
        let tokens = lexer::lex(span.file_id, &function_source, 0)
            .0
            .ok_or_else(|| {
                LspError::FunctionNotFound(format!("Function with name {name} not found"))
            })?;
        let name_span = tokens
            .windows(2)
            .find_map(|pair| match pair {
                [(lexer::Token::Fn, _), (lexer::Token::Ident(ident), ident_span)]
                    if *ident == name.as_str() =>
                {
                    Some(*ident_span)
                }
                _ => None,
            })
            .ok_or_else(|| {
                LspError::FunctionNotFound(format!(
                    "Function with name {name} not found inside its source span"
                ))
            })?;

        Ok(lsp_types::Range::new(
            offset_to_position(span.start + name_span.start, source)?,
            offset_to_position(span.start + name_span.end, source)?,
        ))
    }

    /// Resolve the imported function named by a cursor inside a `use` declaration.
    pub fn find_imported_function(&self, token_span: error::Span) -> Option<&parse::Function> {
        let use_decl = self
            .use_declarations
            .iter()
            .filter(|use_decl| span_contains(use_decl.span(), &token_span))
            .min_by_key(|use_decl| use_decl.span().end - use_decl.span().start)?;

        // Identifiers after `use`, in order: the path segments, then each item and its alias.
        let source = self.text.to_string();
        let tokens = lexer::lex(0, &source, 0).0?;
        let selected = tokens
            .iter()
            .filter(|(_, span)| {
                span.start >= use_decl.span().start && span.end <= use_decl.span().end
            })
            .skip_while(|(token, _)| !matches!(token, lexer::Token::Use))
            .skip(1)
            .filter(|(token, _)| matches!(token, lexer::Token::Crate | lexer::Token::Ident(_)))
            .position(|(_, span)| span_contains(span, &token_span))?;

        // Each item takes one identifier, plus one for its alias; both name the local binding.
        let mut local_names = use_items(use_decl).iter().flat_map(|(original, alias)| {
            std::iter::repeat_n(
                alias.as_ref().unwrap_or(original),
                1 + usize::from(alias.is_some()),
            )
        });
        let local_name = local_names.nth(selected.checked_sub(use_decl.path().len())?)?;
        self.functions.get_func(local_name.as_str())
    }

    /// Find the smallest call whose callable span contains the requested source position.
    pub fn find_related_call(&self, token_span: error::Span) -> Option<&parse::Call> {
        let function = self.functions.iter().find(|function| {
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
