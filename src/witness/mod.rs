use std::collections::HashMap;

use tower_lsp_server::lsp_types::{Diagnostic, Position, Range};

/// Validate a witness (`.wit`) document and return diagnostics in LSP coordinates.
pub fn validate(text: &str) -> Vec<Diagnostic> {
    let json: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            let position = json_error_position(text, &error);
            return vec![Diagnostic::new_simple(
                Range::new(
                    position,
                    Position::new(position.line, position.character.saturating_add(1)),
                ),
                format!("JSON syntax error: {error}"),
            )];
        }
    };

    let Some(witnesses) = json.as_object() else {
        return vec![Diagnostic::new_simple(
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            "Witness file must be a JSON object".to_string(),
        )];
    };

    let positions = key_positions(text);
    let mut diagnostics = Vec::new();
    for (name, value) in witnesses {
        let Some(witness) = value.as_object() else {
            push_at_key(
                &mut diagnostics,
                &positions,
                name,
                format!("Witness '{name}' must be an object with 'value' and 'type' fields"),
            );
            continue;
        };

        for field in ["value", "type"] {
            if !witness.contains_key(field) {
                push_at_key(
                    &mut diagnostics,
                    &positions,
                    name,
                    format!("Witness '{name}' is missing required '{field}' field"),
                );
            }
        }
    }
    diagnostics
}

fn push_at_key(
    diagnostics: &mut Vec<Diagnostic>,
    positions: &HashMap<String, Position>,
    key: &str,
    message: String,
) {
    if let Some(&position) = positions.get(key) {
        diagnostics.push(Diagnostic::new_simple(
            Range::new(position, position),
            message,
        ));
    }
}

// TODO: Replace this scanner with a standard span-aware JSON parser once one can provide decoded
// top-level object keys together with their original source ranges.
fn key_positions(text: &str) -> HashMap<String, Position> {
    let mut positions = HashMap::new();
    let mut depth = 0_usize;
    let mut line = 0_usize;
    let mut utf16_column = 0_usize;
    let mut expecting_top_level_key = false;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_is_key = false;
    let mut string_start = 0_usize;
    let mut string_position = Position::new(0, 0);

    for (byte_index, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
                if string_is_key {
                    let quoted = &text[string_start..byte_index + character.len_utf8()];
                    if let Ok(key) = serde_json::from_str::<String>(quoted) {
                        positions.insert(key, string_position);
                    }
                    expecting_top_level_key = false;
                }
            }
        } else {
            match character {
                '"' => {
                    in_string = true;
                    string_is_key = depth == 1 && expecting_top_level_key;
                    string_start = byte_index;
                    string_position = Position::new(
                        u32::try_from(line).unwrap_or(u32::MAX),
                        u32::try_from(utf16_column).unwrap_or(u32::MAX),
                    );
                }
                '{' | '[' => {
                    depth += 1;
                    if depth == 1 && character == '{' {
                        expecting_top_level_key = true;
                    }
                }
                '}' | ']' => {
                    if depth == 1 {
                        expecting_top_level_key = false;
                    }
                    depth = depth.saturating_sub(1);
                }
                ',' if depth == 1 => expecting_top_level_key = true,
                _ => {}
            }
        }

        if character == '\n' {
            line += 1;
            utf16_column = 0;
        } else {
            utf16_column += character.len_utf16();
        }
    }

    positions
}

fn json_error_position(text: &str, error: &serde_json::Error) -> Position {
    let line_number = error.line().saturating_sub(1);
    let byte_column = error.column().saturating_sub(1);
    let utf16_column = text
        .lines()
        .nth(line_number)
        .map(|line| utf16_column_at_byte(line, byte_column))
        .unwrap_or_default();
    Position::new(
        u32::try_from(line_number).unwrap_or(u32::MAX),
        u32::try_from(utf16_column).unwrap_or(u32::MAX),
    )
}

fn utf16_column_at_byte(line: &str, byte_column: usize) -> usize {
    let mut boundary = byte_column.min(line.len());
    while !line.is_char_boundary(boundary) {
        boundary -= 1;
    }
    line[..boundary].encode_utf16().count()
}

#[cfg(test)]
mod tests;
