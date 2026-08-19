use ropey::Rope;
use tower_lsp_server::lsp_types::{
    self, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureInformation,
};

use crate::completion;
use crate::error::LspError;
use simplicityhl::parse::CallName;

pub fn span_contains(a: &simplicityhl::error::Span, b: &simplicityhl::error::Span) -> bool {
    a.file_id == b.file_id && a.start <= b.start && a.end >= b.end
}

/// Convert byte offset to [`lsp_types::Position`].
///
/// It's converting to UTF-16 column position because it's default to LSP settings. For more
/// context, see [`lsp_types::PositionEncodingKind`]
pub fn offset_to_position(offset: usize, rope: &Rope) -> Result<lsp_types::Position, LspError> {
    let line = rope.try_byte_to_line(offset)?;
    let first_byte_of_line = rope.try_line_to_byte(line)?;
    let column = offset - first_byte_of_line;

    let rope_line = rope
        .get_line(line)
        .ok_or_else(|| LspError::ConversionFailed("Offset to position".to_string()))?;

    let utf16_offset: usize = rope_line
        .get_byte_slice(..column)
        .ok_or_else(|| LspError::ConversionFailed("Offset to position".to_string()))?
        .chars()
        .map(char::len_utf16)
        .sum();

    Ok(lsp_types::Position::new(
        <u32>::try_from(line)?,
        <u32>::try_from(utf16_offset)?,
    ))
}

/// Convert [`lsp_types::Position`] to byte offset.
pub fn position_to_offset(position: lsp_types::Position, rope: &Rope) -> Result<usize, LspError> {
    let line_index = usize::try_from(position.line)?;
    let target_utf16 = usize::try_from(position.character)?;

    let line = rope
        .get_line(line_index)
        .ok_or_else(|| LspError::ConversionFailed("Position to offset".to_string()))?;

    let line_start = rope.try_line_to_byte(line_index)?;
    let mut utf16_offset_in_line = 0usize;
    let mut byte_offset_in_line = 0usize;

    // LSP positions use UTF-16 code units, but Rope is indexed by UTF-8 bytes. Walk the line
    // until we reach the requested UTF-16 boundary so navigation features resolve the right byte.
    for ch in line.chars() {
        if utf16_offset_in_line == target_utf16 {
            return Ok(line_start + byte_offset_in_line);
        }

        let ch_utf16 = ch.len_utf16();
        // Reject positions that would land inside a single scalar value encoded as multiple
        // UTF-16 code units, because spans can only point at byte boundaries between characters.
        if utf16_offset_in_line + ch_utf16 > target_utf16 {
            return Err(LspError::ConversionFailed(
                "Position points inside a UTF-16 code unit sequence".to_string(),
            ));
        }

        utf16_offset_in_line += ch_utf16;
        byte_offset_in_line += ch.len_utf8();
    }

    // LSP allows the cursor to sit at end-of-line, so accept that exact boundary after the scan.
    if utf16_offset_in_line == target_utf16 {
        Ok(line_start + byte_offset_in_line)
    } else {
        Err(LspError::ConversionFailed("Position to offset".to_string()))
    }
}

/// Convert [`simplicityhl::error::Span`] to [`tower_lsp_server::lsp_types::Position`]
///
/// Converting is required because [`simplicityhl::error::Span`] contains byte offsets instead of
/// `line` and `col` fields.
pub fn span_to_positions(
    span: &simplicityhl::error::Span,
    rope: &Rope,
) -> Result<(lsp_types::Position, lsp_types::Position), LspError> {
    Ok((
        offset_to_position(span.start, rope)?,
        offset_to_position(span.end, rope)?,
    ))
}

/// Convert [`tower_lsp_server::lsp_types::Position`] to [`simplicityhl::error::Span`]
///
/// Useful when [`tower_lsp_server::lsp_types::Position`] represents some singular point.
pub fn position_to_span(
    position: lsp_types::Position,
    rope: &Rope,
) -> Result<simplicityhl::error::Span, LspError> {
    let start_line = position_to_offset(position, rope)?;

    Ok(simplicityhl::error::Span::new(0, start_line..start_line))
}

/// Get document comments, using lines above given line index. Only used to
/// get documentation for custom functions.
pub fn get_comments_from_lines(line: u32, rope: &Rope) -> String {
    let mut lines = Vec::new();

    if line == 0 {
        return String::new();
    }

    for i in (0..line).rev() {
        let Some(rope_slice) = rope.get_line(i as usize) else {
            break;
        };
        let text = rope_slice.to_string();

        if text.starts_with("///") {
            let doc = text
                .strip_prefix("///")
                .unwrap_or("")
                .trim_end()
                .to_string();
            lines.push(doc);
        } else {
            break;
        }
    }

    lines.reverse();

    let mut result = String::new();
    let mut prev_line_was_text = false;

    for line in lines {
        let trimmed = line.trim();

        let is_md_block = trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with('-')
            || trimmed.starts_with('*')
            || trimmed.starts_with('>')
            || trimmed.starts_with("```")
            || trimmed.starts_with("    ");

        if result.is_empty() {
            result.push_str(trimmed);
        } else if prev_line_was_text && !is_md_block {
            result.push(' ');
            result.push_str(trimmed);
        } else {
            result.push('\n');
            result.push_str(trimmed);
        }

        prev_line_was_text = !trimmed.is_empty() && !is_md_block;
    }

    result
}

pub fn get_call_span(call: &simplicityhl::parse::Call) -> simplicityhl::error::Span {
    let length = call.name().to_string().len();

    simplicityhl::error::Span::new(
        call.span().file_id,
        call.span().start..call.span().start + length,
    )
}

/// Find the position of a key in the JSON text
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
    use ropey::Rope;

    #[test]
    fn test_get_comments_from_lines() {
        let text = Rope::from_str("/// This is a test.\n/// It has two lines.\nfn func() {}");
        let result = get_comments_from_lines(2, &text);
        assert_eq!(result, "This is a test. It has two lines.");

        let text = Rope::from_str("/// # Title\n/// - Point one\n/// - Point two\nfn func() {}");
        let result = get_comments_from_lines(3, &text);
        assert_eq!(result, "# Title\n- Point one\n- Point two");

        let text = Rope::from_str(
            "/// This is not part of the doc \n\n/// This is part of the doc\nfn func() {}",
        );
        let result = get_comments_from_lines(3, &text);
        assert_eq!(result, "This is part of the doc");

        let text = Rope::from_str("fn func() {}");
        let result = get_comments_from_lines(0, &text);
        assert_eq!(result, "");
    }

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

    /// Tests for UTF-16 encoding: <https://github.com/BlockstreamResearch/SimplicityHL/pull/223#discussion_r2989899313>
    #[test]
    fn test_span_to_positions_handles_multibyte_utf8_before_span() {
        let text = Rope::from_str("/// π\nfn foo() {}");

        // "/// " = 4 bytes, "π" = 2 bytes, "\n" = 1 byte, so `fn` starts at byte 7.
        let span = simplicityhl::error::Span::new(0, 7..9);

        let (start, end) = span_to_positions(&span, &text).expect("span conversion should succeed");

        assert_eq!(start, lsp_types::Position::new(1, 0));
        assert_eq!(end, lsp_types::Position::new(1, 2));
    }

    #[test]
    fn test_position_to_offset_uses_utf16_columns() {
        let text = Rope::from_str("😀x");

        // In LSP, 😀 occupies two UTF-16 code units, so column 2 is just after the emoji.
        let offset = position_to_offset(lsp_types::Position::new(0, 2), &text)
            .expect("position conversion should succeed");

        assert_eq!(offset, 4);
    }

    #[test]
    fn test_position_to_offset_keeps_line_start_at_zero() {
        let text = Rope::from_str("foo");

        let offset = position_to_offset(lsp_types::Position::new(0, 0), &text)
            .expect("line start should convert to byte offset 0");

        assert_eq!(offset, 0);
    }

    #[test]
    fn test_position_to_offset_does_not_shift_ascii_columns_left() {
        let text = Rope::from_str("    foo()");

        let offset = position_to_offset(lsp_types::Position::new(0, 4), &text)
            .expect("identifier start should map to its exact byte offset");
        let span = position_to_span(lsp_types::Position::new(0, 4), &text)
            .expect("identifier start should map to the same byte offset");

        assert_eq!(offset, 4);
        assert_eq!(span, simplicityhl::error::Span::new(0, 4..4));
    }

    #[test]
    fn test_position_to_offset_handles_single_utf16_multibyte_prefix() {
        let text = Rope::from_str("πx");

        // `π` is one UTF-16 code unit but two UTF-8 bytes, so column 1 should land after it.
        let offset = position_to_offset(lsp_types::Position::new(0, 1), &text)
            .expect("UTF-16 column after a BMP multibyte char should convert correctly");

        assert_eq!(offset, 2);
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
