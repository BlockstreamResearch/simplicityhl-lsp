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

    for function in snapshot.functions.iter() {
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

#[cfg(test)]
mod tests {
    use ropey::Rope;
    use simplicityhl::error::DiagnosticManager;
    use simplicityhl::parse::ParseFromStrWithErrors;
    use simplicityhl::UnstableFeatures;

    use super::*;
    use crate::text::offset_to_position;

    fn snapshot(source: &str) -> AnalysisSnapshot {
        let mut diagnostics = DiagnosticManager::new();
        let program = parse::Program::parse_from_str_with_errors(
            0,
            source,
            &UnstableFeatures::none(),
            &mut diagnostics,
        )
            .unwrap_or_else(|| panic!("source should parse: {diagnostics:?}"));
        AnalysisSnapshot::from_program(
            &program,
            source,
            &std::env::temp_dir().join("semantic_tokens.simf"),
        )
    }

    fn decode(tokens: &[SemanticToken]) -> Vec<RawToken> {
        let mut line = 0;
        let mut character = 0;
        tokens
            .iter()
            .map(|token| {
                line += token.delta_line;
                character = if token.delta_line == 0 {
                    character + token.delta_start
                } else {
                    token.delta_start
                };
                (
                    line,
                    character,
                    token.length,
                    token.token_type,
                    token.token_modifiers_bitset,
                )
            })
            .collect()
    }

    fn expected(source: &str, offset: usize, text: &str, kind: u32) -> RawToken {
        let rope = Rope::from_str(source);
        let start = offset_to_position(offset, &rope).expect("valid token start");
        let end = offset_to_position(offset + text.len(), &rope).expect("valid token end");
        (
            start.line,
            start.character,
            end.character - start.character,
            kind,
            0,
        )
    }

    #[test]
    fn splits_generic_callable_components() {
        let source = "// 😀 keeps UTF-16 columns honest\nfn consume_budget(acc: u32, item: u32) -> u32 { acc }\nfn main() { array_fold::<consume_budget, 320>(witness::PADDING, true); }\n";
        let snapshot = snapshot(source);
        let decoded = decode(&tokens(&snapshot));
        let builtin = source.find("array_fold").expect("array_fold call");
        let callback = source.rfind("consume_budget").expect("callback argument");

        assert!(decoded.contains(&expected(
            source,
            builtin,
            "array_fold",
            token_type::FUNCTION
        )));
        assert!(decoded.contains(&expected(
            source,
            callback,
            "consume_budget",
            token_type::FUNCTION,
        )));

        let bound = source.find("320").expect("array bound");
        let bound = offset_to_position(bound, &snapshot.text).unwrap();
        assert!(!decoded.iter().any(|token| {
            token.0 == bound.line
                && token.1 <= bound.character
                && token.1 + token.2 > bound.character
        }));
    }

    #[test]
    fn bounds_each_builtin_to_its_identifier() {
        let source = "fn step(acc: u32, item: u32) -> u32 { acc }\nfn main() {\nfold::<step, 2>(0, 0);\nfor_while::<step>(0);\nunwrap_left::<u32>(0);\n<u32>::into(0);\njet::add_32(0, 0);\nassert!(true);\n}\n";
        let decoded = decode(&tokens(&snapshot(source)));

        for name in ["fold", "for_while", "unwrap_left", "into", "assert!"] {
            let offset = source
                .find(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert!(decoded.contains(&expected(source, offset, name, token_type::FUNCTION,)));
        }

        for offset in [
            source.find("step, 2").unwrap(),
            source.rfind("step").unwrap(),
        ] {
            assert!(decoded.contains(&expected(source, offset, "step", token_type::FUNCTION,)));
        }

        let jet = source.find("jet::add_32").unwrap();
        assert!(decoded.contains(&expected(source, jet, "jet", token_type::NAMESPACE)));
        assert!(decoded.contains(&expected(
            source,
            jet + "jet::".len(),
            "add_32",
            token_type::FUNCTION,
        )));
    }
}