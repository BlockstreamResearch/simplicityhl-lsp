use ropey::Rope;
use tower_lsp_server::lsp_types::{self};

use crate::error::LspError;

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

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    #[test]
    fn extracts_contiguous_markdown_comments() {
        let cases = [
            (
                "/// This is a test.\n/// It has two lines.\nfn func() {}",
                2,
                "This is a test. It has two lines.",
            ),
            (
                "/// # Title\n/// - Point one\n/// - Point two\nfn func() {}",
                3,
                "# Title\n- Point one\n- Point two",
            ),
            (
                "/// This is not part of the doc \n\n/// This is part of the doc\nfn func() {}",
                3,
                "This is part of the doc",
            ),
            ("fn func() {}", 0, ""),
        ];
        for (source, line, expected) in cases {
            assert_eq!(
                get_comments_from_lines(line, &Rope::from_str(source)),
                expected
            );
        }
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
    fn position_to_offset_uses_utf16_boundaries() {
        assert!(
            position_to_offset(lsp_types::Position::new(0, 1), &Rope::from_str("😀x")).is_err(),
            "a cursor cannot point into the middle of a UTF-16 surrogate pair"
        );

        for (source, utf16_column, byte_offset) in [
            ("😀x", 2, 4),
            ("foo", 0, 0),
            ("    foo()", 4, 4),
            ("πx", 1, 2),
        ] {
            assert_eq!(
                position_to_offset(
                    lsp_types::Position::new(0, utf16_column),
                    &Rope::from_str(source)
                )
                .unwrap(),
                byte_offset
            );
        }
        assert_eq!(
            position_to_span(lsp_types::Position::new(0, 4), &Rope::from_str("    foo()")).unwrap(),
            simplicityhl::error::Span::new(0, 4..4)
        );
    }
}
