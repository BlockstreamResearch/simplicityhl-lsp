use ropey::Rope;
use tower_lsp_server::lsp_types::{
    self, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureInformation,
};

use miniscript::iter::TreeLike;
use simplicityhl::parse::{self, CallName};
use tower_lsp_server::UriExt;

use crate::backend::{Document, SourceFile};
use crate::completion;
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

impl Document {
    pub fn find_all_references(
        &self,
        call_name: &CallName,
    ) -> Result<Vec<lsp_types::Location>, LspError> {
        self.functions
            .functions()
            .iter()
            .filter_map(|func| {
                let uri = self.linearization_map.get(func.span().file_id)?;
                Some(
                    parse::ExprTree::Expression(func.body())
                        .pre_order_iter()
                        .filter_map(|expr| {
                            if let parse::ExprTree::Call(call) = expr {
                                Some((call, get_call_span(call)))
                            } else {
                                None
                            }
                        })
                        .filter(|(call, _)| call.name() == call_name)
                        .map(|(_, span)| (span, uri))
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .map(|(span, source_file)| {
                let (start, end) = span_to_positions(&span, &source_file.text)?;
                Ok(lsp_types::Location {
                    range: lsp_types::Range { start, end },
                    uri: source_file.uri.clone(),
                })
            })
            .collect::<Result<Vec<_>, LspError>>()
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
                    .linearization_map
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
    ///
    /// `UseDecl` stores names but not an individual span for each path/item segment. The lexer
    /// supplies those spans; the declaration supplies the semantic split between path segments,
    /// imported names, and aliases. Combining them prevents a path component that happens to have
    /// the same spelling as a function from being treated as the imported item.
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
        let (tokens, _) = simplicityhl::lexer::lex(0, &source, 0);
        let tokens = tokens?;
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

        // All identifiers before this boundary belong to the module path. The remainder is
        // `(original, optional alias)` for each imported item, in source order.
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
                let local_name = alias.as_ref().unwrap_or(original);
                return self.functions.get_func(local_name.as_inner());
            }
        }

        None
    }

    /// Find [`simplicityhl::parse::Call`] which contains given [`simplicityhl::error::Span`], which also have minimal Span.
    pub fn find_related_call(
        &self,
        token_span: simplicityhl::error::Span,
    ) -> Option<&simplicityhl::parse::Call> {
        let func = self
            .functions
            .functions()
            .into_iter()
            .find(|func| span_contains(func.span(), &token_span) && func.span().file_id == 0)?;

        let call = parse::ExprTree::Expression(func.body())
            .pre_order_iter()
            .filter_map(|expr| {
                if let parse::ExprTree::Call(call) = expr {
                    // Only include if call span can be obtained
                    Some((call, get_call_span(call)))
                } else {
                    None
                }
            })
            .filter(|(_, span)| span_contains(span, &token_span))
            .map(|(call, _)| call)
            .last();

        call
    }

    /// Append functions imported via `use` declarations to [`Document`],
    /// respecting aliases (e.g. `use crate::a::func as func2`).
    pub fn populate_visible_functions(&mut self, template_program: &simplicityhl::TemplateProgram) {
        let Some(source_map) = template_program.source_map() else {
            return;
        };

        // Populate linearization_map from module_registry.
        let mut modules: Vec<_> = source_map.iter().map(|(p, id)| (*id, p)).collect();
        modules.sort_by_key(|(id, _)| *id);

        self.linearization_map = modules
            .iter()
            .map(|(file_id, path)| {
                let uri = lsp_types::Uri::from_file_path(path.as_path()).expect("valid file URI");
                let text = if *file_id == 0 {
                    self.text.clone()
                } else {
                    Rope::from_str(
                        &std::fs::read_to_string(path.as_path())
                            .expect("failed to read module source"),
                    )
                };
                SourceFile { uri, text }
            })
            .collect();

        let resolved_program = template_program.resolved_program();

        for item in resolved_program.items() {
            let parse::Item::Module(module) = item else {
                continue;
            };
            let Some(0) = module
                .name()
                .as_inner()
                .strip_prefix("unit_")
                .and_then(|s| s.parse::<usize>().ok())
            else {
                continue;
            };

            for inner_item in module.items() {
                let parse::Item::Use(use_decl) = inner_item else {
                    continue;
                };

                let path = use_decl.path();
                let Some(target_module_str) = path.get(1) else {
                    continue;
                };
                let Some(target_file_id) = target_module_str
                    .as_inner()
                    .strip_prefix("unit_")
                    .and_then(|s| s.parse::<usize>().ok())
                else {
                    continue;
                };

                let items = match use_decl.items() {
                    parse::UseItems::Single(elem) => std::slice::from_ref(elem),
                    parse::UseItems::List(elems) => elems.as_slice(),
                };

                for (original_name, alias) in items {
                    let local_name = alias.as_ref().unwrap_or(original_name);
                    let mut visited = std::collections::HashSet::new();
                    let Some(func) = resolve_function(
                        resolved_program,
                        target_file_id,
                        &path[2..],
                        original_name.as_inner(),
                        &mut visited,
                    ) else {
                        continue;
                    };

                    let Some(source_file) = self.linearization_map.get(func.span().file_id) else {
                        continue;
                    };

                    let start_line = offset_to_position(func.span().start, &source_file.text)
                        .unwrap_or_default()
                        .line;
                    let doc_comments = get_comments_from_lines(start_line, &source_file.text);

                    self.functions
                        .insert(local_name.to_string(), (*func).clone(), doc_comments);
                }
            }
        }
    }
}

type ResolutionKey = (usize, Vec<String>, String);

fn resolve_function<'a>(
    program: &'a parse::Program,
    file_id: usize,
    module_path: &[simplicityhl::str::Identifier],
    name: &str,
    visited: &mut std::collections::HashSet<ResolutionKey>,
) -> Option<&'a parse::Function> {
    let key = (
        file_id,
        module_path
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        name.to_string(),
    );
    if !visited.insert(key) {
        return None;
    }

    let items = module_items(program, file_id, module_path)?;
    if let Some(function) = items.iter().find_map(|item| match item {
        parse::Item::Function(function) if function.name().as_inner() == name => Some(function),
        _ => None,
    }) {
        return Some(function);
    }

    for use_decl in items.iter().filter_map(|item| match item {
        parse::Item::Use(use_decl) => Some(use_decl),
        _ => None,
    }) {
        let imported_items = match use_decl.items() {
            parse::UseItems::Single(item) => std::slice::from_ref(item),
            parse::UseItems::List(items) => items.as_slice(),
        };
        for (original, alias) in imported_items {
            let local_name = alias.as_ref().unwrap_or(original);
            if local_name.as_inner() != name {
                continue;
            }

            let path = use_decl.path();
            let target_file_id = path
                .get(1)?
                .as_inner()
                .strip_prefix("unit_")?
                .parse::<usize>()
                .ok()?;
            if let Some(function) = resolve_function(
                program,
                target_file_id,
                &path[2..],
                original.as_inner(),
                visited,
            ) {
                return Some(function);
            }
        }
    }

    None
}

fn module_items<'a>(
    program: &'a parse::Program,
    file_id: usize,
    module_path: &[simplicityhl::str::Identifier],
) -> Option<&'a [parse::Item]> {
    let unit_name = format!("unit_{file_id}");
    let unit = program.items().iter().find_map(|item| match item {
        parse::Item::Module(module) if module.name().as_inner() == unit_name => Some(module),
        _ => None,
    })?;
    let mut items = unit.items();

    for segment in module_path {
        let module = items.iter().find_map(|item| match item {
            parse::Item::Module(module) if module.name().as_inner() == segment.as_inner() => {
                Some(module)
            }
            _ => None,
        })?;
        items = module.items();
    }

    Some(items)
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
