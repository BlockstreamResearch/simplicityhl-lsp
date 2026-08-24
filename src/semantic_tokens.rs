use miniscript::iter::TreeLike;
use simplicityhl::error::Span;
use simplicityhl::parse;
use tower_lsp_server::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend,
};

use crate::analysis::AnalysisSnapshot;
use crate::text::span_to_positions;

mod token_type {
    pub const FUNCTION: u32 = 0;
    pub const NAMESPACE: u32 = 5;
}

type RawToken = (u32, u32, u32, u32, u32);

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::FUNCTION,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::TYPE,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::NAMESPACE,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFINITION,
        ],
    }
}

pub fn tokens(snapshot: &AnalysisSnapshot) -> Vec<SemanticToken> {
    let source = snapshot.text.to_string();
    let lexer_tokens = simplicityhl::lexer::lex(0, &source, 0)
        .0
        .unwrap_or_default();
    let mut raw_tokens = Vec::new();

    for function in snapshot.functions.functions() {
        if function.span().file_id != 0 {
            continue;
        }

        if let Ok(name_range) = snapshot.find_function_name_range(function) {
            if name_range.start.line == name_range.end.line
                && name_range.start.character < name_range.end.character
            {
                raw_tokens.push((
                    name_range.start.line,
                    name_range.start.character,
                    name_range.end.character - name_range.start.character,
                    token_type::FUNCTION,
                    0b11,
                ));
            }
        }

        for expression in parse::ExprTree::Expression(function.body()).pre_order_iter() {
            let parse::ExprTree::Call(call) = expression else {
                continue;
            };
            for (span, token_type) in call_spans(call, &lexer_tokens) {
                if let Some(token) = raw_token(&span, &snapshot.text, token_type, 0) {
                    raw_tokens.push(token);
                }
            }
        }
    }

    encode(raw_tokens)
}

fn raw_token(span: &Span, text: &ropey::Rope, token_type: u32, modifiers: u32) -> Option<RawToken> {
    let (start, end) = span_to_positions(span, text).ok()?;
    if start.line != end.line || start.character >= end.character {
        return None;
    }
    Some((
        start.line,
        start.character,
        end.character - start.character,
        token_type,
        modifiers,
    ))
}

fn call_spans(call: &parse::Call, tokens: &simplicityhl::lexer::Tokens<'_>) -> Vec<(Span, u32)> {
    use simplicityhl::lexer::Token;

    let call_start = call.span().start;
    let token_at_start = tokens.iter().find(|(_, span)| span.start == call_start);
    let find_ident = |name: &str, must_start_call: bool| {
        tokens
            .iter()
            .find(|(token, span)| {
                (!must_start_call || span.start == call_start)
                    && span.start >= call_start
                    && span.end <= call.span().end
                    && matches!(token, Token::Ident(value) if *value == name)
            })
            .map(|(_, span)| *span)
    };

    let function = token_type::FUNCTION;
    match call.name() {
        parse::CallName::Jet(_) => token_at_start
            .filter(|(token, _)| matches!(token, Token::Jet(_)))
            .map(|(_, span)| {
                vec![
                    (
                        Span::new(span.file_id, span.start..span.start + 3),
                        token_type::NAMESPACE,
                    ),
                    (Span::new(span.file_id, span.start + 5..span.end), function),
                ]
            })
            .unwrap_or_default(),
        parse::CallName::Assert | parse::CallName::Panic | parse::CallName::Debug => token_at_start
            .filter(|(token, _)| matches!(token, Token::Macro(_)))
            .map(|(_, span)| vec![(*span, function)])
            .unwrap_or_default(),
        name => {
            let (callable, callable_starts_call, callback) = match name {
                parse::CallName::Custom(name) => (name.as_inner(), true, None),
                parse::CallName::Fold(name, _) => ("fold", true, Some(name.as_inner())),
                parse::CallName::ArrayFold(name, _) => ("array_fold", true, Some(name.as_inner())),
                parse::CallName::ForWhile(name) => ("for_while", true, Some(name.as_inner())),
                parse::CallName::UnwrapLeft(_) => ("unwrap_left", true, None),
                parse::CallName::UnwrapRight(_) => ("unwrap_right", true, None),
                parse::CallName::Unwrap => ("unwrap", true, None),
                parse::CallName::IsNone(_) => ("is_none", true, None),
                parse::CallName::TypeCast(_) => ("into", false, None),
                parse::CallName::Jet(_)
                | parse::CallName::Assert
                | parse::CallName::Panic
                | parse::CallName::Debug => unreachable!(),
            };
            [
                find_ident(callable, callable_starts_call),
                callback.and_then(|name| find_ident(name, false)),
            ]
            .into_iter()
            .flatten()
            .map(|span| (span, function))
            .collect()
        }
    }
}

fn encode(mut tokens: Vec<RawToken>) -> Vec<SemanticToken> {
    tokens.sort_unstable();
    tokens.dedup();

    let mut previous_line = 0;
    let mut previous_character = 0;
    tokens
        .into_iter()
        .map(|(line, character, length, token_type, modifiers)| {
            let delta_line = line - previous_line;
            let delta_start = if delta_line == 0 {
                character - previous_character
            } else {
                character
            };
            previous_line = line;
            previous_character = character;
            SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type,
                token_modifiers_bitset: modifiers,
            }
        })
        .collect()
}
