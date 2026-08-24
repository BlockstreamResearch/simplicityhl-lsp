use tower_lsp_server::lsp_types::{
    self, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureInformation,
};

use crate::completion;
use simplicityhl::parse::CallName;

pub fn find_key_position(text: &str, key: &str) -> Option<lsp_types::Position> {
    let search = format!("\"{key}\"");
    for (line_num, line) in text.lines().enumerate() {
        if let Some(col) = line.find(&search) {
            return Some(lsp_types::Position::new(
                u32::try_from(line_num).ok()?,
                u32::try_from(col).ok()?,
            ));
        }
    }
    None
}

/// Find function call context from the current line.
/// Returns (`function_name`, `active_parameter_index`) if inside a function call.
pub fn find_function_call_context(line: &str) -> Option<(String, u32)> {
    let mut paren_depth = 0;
    let mut bracket_depth = 0;
    let mut angle_depth = 0;
    let mut last_open_paren = None;
    let mut comma_count = 0;

    // Scan from the end to find the innermost unclosed function call.
    // `char_indices` yields byte offsets, which is what `line` must be sliced by below;
    // counting characters instead would mis-address every byte after a multi-byte character.
    for (pos, ch) in line.char_indices().rev() {
        match ch {
            ')' => paren_depth += 1,
            '(' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                } else {
                    // Found unclosed '(' - this is our function call
                    last_open_paren = Some(pos);
                    break;
                }
            }
            ']' => bracket_depth += 1,
            '[' if bracket_depth > 0 => bracket_depth -= 1,
            '>' => angle_depth += 1,
            '<' if angle_depth > 0 => angle_depth -= 1,
            ',' if paren_depth == 0 && bracket_depth == 0 && angle_depth == 0 => {
                comma_count += 1;
            }
            _ => {}
        }
    }

    let open_paren_pos = last_open_paren?;

    // Extract function name before the '('
    let before_paren = &line[..open_paren_pos];
    let func_name = extract_function_name(before_paren)?;

    Some((func_name, comma_count))
}

/// Extract function name from text before an opening parenthesis.
/// Handles patterns like: `func_name`, `jet::add_32`, `fold::<f, 8>`
pub fn extract_function_name(text: &str) -> Option<String> {
    let trimmed = text.trim_end();

    // Skip generic parameters if present (e.g., `fold::<f, 8>`)
    let without_generics = if trimmed.ends_with('>') {
        let mut depth = 0usize;
        let mut start = None;
        // As above, `char_indices` keeps `start` a valid byte offset into `trimmed`.
        for (i, ch) in trimmed.char_indices().rev() {
            match ch {
                '>' => depth += 1,
                '<' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        start = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        match start {
            Some(pos) => {
                let before = &trimmed[..pos];
                // Remove the `::` before `<` if present
                before.strip_suffix("::").unwrap_or(before)
            }
            None => trimmed,
        }
    } else {
        trimmed
    };

    // Now find the function name - it should be an identifier possibly with `::`
    let mut name_chars = Vec::new();

    for ch in without_generics.chars().rev() {
        if ch.is_alphanumeric() || ch == '_' || ch == ':' {
            name_chars.push(ch);
        } else {
            break;
        }
    }

    if name_chars.is_empty() {
        return None;
    }

    name_chars.reverse();
    let name: String = name_chars.into_iter().collect();

    // Clean up leading colons
    let cleaned = name.trim_start_matches(':');
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

/// Create `SignatureInformation` from a `FunctionTemplate`.
pub fn create_signature_info(
    template: &completion::types::FunctionTemplate,
) -> SignatureInformation {
    let params: Vec<ParameterInformation> = template
        .args
        .iter()
        .map(|arg| ParameterInformation {
            label: ParameterLabel::Simple(arg.clone()),
            documentation: None,
        })
        .collect();

    let signature_label = format!(
        "fn {}({}) -> {}",
        template.display_name,
        template.args.join(", "),
        template.return_type
    );

    SignatureInformation {
        label: signature_label,
        documentation: if template.description.is_empty() {
            None
        } else {
            Some(lsp_types::Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: template.description.clone(),
            }))
        },
        parameters: Some(params),
        active_parameter: None,
    }
}

/// Find signature for builtin functions.
pub fn find_builtin_signature(name: &str) -> Option<SignatureInformation> {
    use simplicityhl::str::AliasName;
    use simplicityhl::types::AliasedType;

    let ty = AliasedType::from(AliasName::from_str_unchecked("T"));

    // Match common builtin function names
    let call_name = match name {
        "unwrap_left" => Some(CallName::UnwrapLeft(ty.clone())),
        "unwrap_right" => Some(CallName::UnwrapRight(ty.clone())),
        "unwrap" => Some(CallName::Unwrap),
        "is_none" => Some(CallName::IsNone(ty.clone())),
        "assert!" => Some(CallName::Assert),
        "panic!" => Some(CallName::Panic),
        "dbg!" => Some(CallName::Debug),
        _ => None,
    };

    let call_name = call_name?;
    let template = completion::builtin::match_callname(&call_name)?;
    Some(create_signature_info(&template))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_extract_function_name() {
        // Simple function name
        assert_eq!(extract_function_name("foo"), Some("foo".to_string()));
        assert_eq!(
            extract_function_name("my_func"),
            Some("my_func".to_string())
        );

        // With module prefix
        assert_eq!(
            extract_function_name("jet::add_32"),
            Some("jet::add_32".to_string())
        );

        // With generic parameters
        assert_eq!(
            extract_function_name("fold::<f, 8>"),
            Some("fold".to_string())
        );
        assert_eq!(
            extract_function_name("unwrap_left::<u8>"),
            Some("unwrap_left".to_string())
        );

        // With leading whitespace/expressions
        assert_eq!(
            extract_function_name("let x = foo"),
            Some("foo".to_string())
        );

        // Empty input
        assert_eq!(extract_function_name(""), None);
    }

    #[test]
    fn test_find_function_call_context() {
        // Simple function call
        assert_eq!(
            find_function_call_context("foo("),
            Some(("foo".to_string(), 0))
        );
        assert_eq!(
            find_function_call_context("foo(a, "),
            Some(("foo".to_string(), 1))
        );
        assert_eq!(
            find_function_call_context("foo(a, b, "),
            Some(("foo".to_string(), 2))
        );

        // Nested function calls
        assert_eq!(
            find_function_call_context("outer(inner(x), "),
            Some(("outer".to_string(), 1))
        );

        // With module prefix
        assert_eq!(
            find_function_call_context("jet::add_32(a, "),
            Some(("jet::add_32".to_string(), 1))
        );

        // No function call
        assert_eq!(find_function_call_context("let x = 5"), None);
    }

    #[test]
    fn test_find_function_call_context_handles_multibyte_arguments() {
        assert_eq!(
            find_function_call_context("add(é, "),
            Some(("add".to_string(), 1))
        );
        assert_eq!(
            find_function_call_context("sum(日本, "),
            Some(("sum".to_string(), 1))
        );
        assert_eq!(
            find_function_call_context("f(éé"),
            Some(("f".to_string(), 0))
        );
    }

    #[test]
    fn test_extract_function_name_handles_multibyte_before_generics() {
        assert_eq!(
            extract_function_name("é; fold::<f, 8>"),
            Some("fold".to_string())
        );
    }
}
