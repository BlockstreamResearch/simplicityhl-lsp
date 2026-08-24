use std::str::FromStr;

use simplicityhl::parse::CallName;
use tower_lsp_server::lsp_types::{
    self, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Position, SignatureHelp,
    SignatureInformation,
};

use crate::analysis::AnalysisSnapshot;
use crate::completion;
use crate::error::LspError;
use crate::text::position_to_offset;

pub fn at(
    snapshot: &AnalysisSnapshot,
    position: Position,
) -> Result<Option<SignatureHelp>, LspError> {
    let line_index = usize::try_from(position.line)?;
    let source_line = snapshot
        .text
        .get_line(line_index)
        .ok_or_else(|| LspError::Internal("Line not found".to_string()))?;
    if usize::try_from(position.character)?
        > source_line.chars().map(char::len_utf16).sum::<usize>()
    {
        return Ok(None);
    }

    let cursor = position_to_offset(position, &snapshot.text)?;
    let line_start = snapshot.text.try_line_to_byte(line_index)?;
    let line = snapshot
        .text
        .get_byte_slice(line_start..cursor)
        .ok_or_else(|| LspError::Internal("Cursor is outside the current line".to_string()))?;
    let Some((function_name, active_parameter)) = call_context(&line.to_string()) else {
        return Ok(None);
    };

    let signature = if let Some(jet_name) = function_name.strip_prefix("jet::") {
        simplicityhl::simplicity::jet::Elements::from_str(jet_name)
            .ok()
            .map(completion::jet::jet_to_template)
            .as_ref()
            .map(signature_information)
    } else if let Some((function, documentation)) = snapshot.functions.get(&function_name) {
        Some(signature_information(&completion::function_to_template(
            function,
            documentation,
        )))
    } else {
        builtin_signature(&function_name)
    };

    Ok(signature.map(|signature| SignatureHelp {
        signatures: vec![signature],
        active_signature: Some(0),
        active_parameter: Some(active_parameter),
    }))
}

/// Find the innermost unclosed function call and its active argument.
fn call_context(line: &str) -> Option<(String, u32)> {
    let mut parenthesis_depth = 0;
    let mut bracket_depth = 0;
    let mut angle_depth = 0;
    let mut comma_count = 0;
    let open_parenthesis = line
        .char_indices()
        .rev()
        .find_map(|(position, character)| {
            match character {
                ')' => parenthesis_depth += 1,
                '(' if parenthesis_depth > 0 => parenthesis_depth -= 1,
                '(' => return Some(position),
                ']' => bracket_depth += 1,
                '[' if bracket_depth > 0 => bracket_depth -= 1,
                '>' => angle_depth += 1,
                '<' if angle_depth > 0 => angle_depth -= 1,
                ',' if parenthesis_depth == 0 && bracket_depth == 0 && angle_depth == 0 => {
                    comma_count += 1;
                }
                _ => {}
            }
            None
        })?;

    function_name(&line[..open_parenthesis]).map(|name| (name, comma_count))
}

/// Extract an identifier, qualified name, or generic callable before `(`.
fn function_name(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let without_generics = if trimmed.ends_with('>') {
        let mut depth = 0usize;
        let start = trimmed
            .char_indices()
            .rev()
            .find_map(|(position, character)| {
                match character {
                    '>' => depth += 1,
                    '<' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return Some(position);
                        }
                    }
                    _ => {}
                }
                None
            });
        start.map_or(trimmed, |position| {
            let before = &trimmed[..position];
            before.strip_suffix("::").unwrap_or(before)
        })
    } else {
        trimmed
    };

    let start = without_generics
        .char_indices()
        .rev()
        .take_while(|(_, character)| {
            character.is_alphanumeric() || *character == '_' || *character == ':'
        })
        .map(|(position, _)| position)
        .last()?;
    let name = without_generics[start..].trim_start_matches(':');
    (!name.is_empty()).then(|| name.to_string())
}

fn signature_information(template: &completion::types::FunctionTemplate) -> SignatureInformation {
    SignatureInformation {
        label: format!(
            "fn {}({}) -> {}",
            template.display_name,
            template.args.join(", "),
            template.return_type
        ),
        documentation: (!template.description.is_empty()).then(|| {
            lsp_types::Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: template.description.clone(),
            })
        }),
        parameters: Some(
            template
                .args
                .iter()
                .cloned()
                .map(|label| ParameterInformation {
                    label: ParameterLabel::Simple(label),
                    documentation: None,
                })
                .collect(),
        ),
        active_parameter: None,
    }
}

fn builtin_signature(name: &str) -> Option<SignatureInformation> {
    use simplicityhl::str::AliasName;
    use simplicityhl::types::AliasedType;

    let generic = AliasedType::from(AliasName::from_str_unchecked("T"));
    let call = match name {
        "unwrap_left" => CallName::UnwrapLeft(generic.clone()),
        "unwrap_right" => CallName::UnwrapRight(generic.clone()),
        "unwrap" => CallName::Unwrap,
        "is_none" => CallName::IsNone(generic),
        "assert!" => CallName::Assert,
        "panic!" => CallName::Panic,
        "dbg!" => CallName::Debug,
        _ => return None,
    };
    completion::builtin::match_callname(&call)
        .as_ref()
        .map(signature_information)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::UriExt;

    #[test]
    fn extracts_callable_names() {
        let cases = [
            ("foo", Some("foo")),
            ("my_func", Some("my_func")),
            ("jet::add_32", Some("jet::add_32")),
            ("fold::<f, 8>", Some("fold")),
            ("unwrap_left::<u8>", Some("unwrap_left")),
            ("let x = foo", Some("foo")),
            ("é; fold::<f, 8>", Some("fold")),
            ("", None),
        ];
        for (text, expected) in cases {
            assert_eq!(function_name(text).as_deref(), expected);
        }
    }

    #[test]
    fn finds_nested_call_context_and_active_parameter() {
        let cases = [
            ("foo(", Some(("foo", 0))),
            ("foo(a, ", Some(("foo", 1))),
            ("foo(a, b, ", Some(("foo", 2))),
            ("outer(inner(x), ", Some(("outer", 1))),
            ("jet::add_32(a, ", Some(("jet::add_32", 1))),
            ("add(é, ", Some(("add", 1))),
            ("sum(日本, ", Some(("sum", 1))),
            ("f(éé", Some(("f", 0))),
            ("let x = 5", None),
        ];
        for (line, expected) in cases {
            assert_eq!(
                call_context(line),
                expected.map(|(name, index)| (name.to_string(), index))
            );
        }
    }

    #[test]
    fn utf16_cursor_selects_the_exact_line_prefix() {
        let source = "😀 jet::add_32(1, ";
        let snapshot = AnalysisSnapshot::new(
            tower_lsp_server::lsp_types::Uri::from_file_path(
                std::env::temp_dir().join("signature.simf"),
            )
            .unwrap(),
            ropey::Rope::from_str(source),
        );
        let help = at(&snapshot, Position::new(0, 18))
            .expect("valid UTF-16 cursor")
            .expect("jet signature");

        assert_eq!(help.active_parameter, Some(1));
        assert!(help.signatures[0].label.starts_with("fn add_32("));
        assert!(at(&snapshot, Position::new(0, 1)).is_err());
        assert!(at(&snapshot, Position::new(0, 19)).unwrap().is_none());
    }
}
