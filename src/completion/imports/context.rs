use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Query {
    Roots,
    Path(Vec<String>),
    Suppressed,
}

/// The unfinished `use` declaration surrounding a completion request.
///
/// This is intentionally derived from source text rather than the compiler AST: while a user is
/// typing `use crate::math::`, there is no complete [`simplicityhl::parse::UseDecl`] for the
/// compiler to expose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportCompletionContext {
    pub(super) use_start: usize,
    pub(super) query: Query,
    pub(super) partial: String,
    pub(super) already_imported: BTreeSet<String>,
}

impl ImportCompletionContext {
    /// Locate an unfinished `use` declaration at `offset`.
    pub(crate) fn at(source: &str, offset: usize) -> Option<Self> {
        let prefix = source.get(..offset)?;
        let use_range = last_use_keyword(prefix)?;
        let declaration = prefix.get(use_range.end..)?.trim_start();

        // A semicolon ends the declaration. In that case normal expression completion should be
        // allowed to take over again.
        if declaration.contains(';') {
            return None;
        }

        if let Some(open_brace) = declaration.rfind('{') {
            if declaration[open_brace + 1..].contains('}') {
                return Some(Self::suppressed(use_range.start));
            }

            let raw_path = declaration[..open_brace].trim();
            let Some(path) = raw_path.strip_suffix("::") else {
                return Some(Self::suppressed(use_range.start));
            };
            let list_prefix = &declaration[open_brace + 1..];
            let (completed, partial) = list_prefix
                .rsplit_once(',')
                .map_or(("", list_prefix), |(completed, partial)| {
                    (completed, partial)
                });
            let already_imported = completed
                .split(',')
                .filter_map(imported_name)
                .collect::<BTreeSet<_>>();

            return Some(Self::path(
                use_range.start,
                path,
                partial.trim(),
                already_imported,
            ));
        }

        if let Some((path, partial)) = declaration.rsplit_once("::") {
            return Some(Self::path(
                use_range.start,
                path.trim(),
                partial.trim(),
                BTreeSet::new(),
            ));
        }

        let partial = declaration.trim();
        if !is_identifier_prefix(partial) {
            return Some(Self::suppressed(use_range.start));
        }

        Some(Self {
            use_start: use_range.start,
            query: Query::Roots,
            partial: partial.to_string(),
            already_imported: BTreeSet::new(),
        })
    }

    fn path(
        use_start: usize,
        path: &str,
        partial: &str,
        already_imported: BTreeSet<String>,
    ) -> Self {
        let segments = path
            .split("::")
            .map(str::trim)
            .map(str::to_string)
            .collect::<Vec<_>>();
        let valid_path = !segments.is_empty()
            && (segments[0] == "crate" || is_identifier(&segments[0]))
            && segments[1..].iter().all(|segment| is_identifier(segment))
            && is_identifier_prefix(partial);

        Self {
            use_start,
            query: if valid_path {
                Query::Path(segments)
            } else {
                Query::Suppressed
            },
            partial: partial.to_string(),
            already_imported,
        }
    }

    fn suppressed(use_start: usize) -> Self {
        Self {
            use_start,
            query: Query::Suppressed,
            partial: String::new(),
            already_imported: BTreeSet::new(),
        }
    }
}

fn imported_name(item: &str) -> Option<String> {
    let name = item.split_whitespace().next()?;
    is_identifier(name).then(|| name.to_string())
}

fn is_identifier_prefix(value: &str) -> bool {
    value.is_empty()
        || (value
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
            && value
                .bytes()
                .skip(1)
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
}

pub(super) fn is_identifier(value: &str) -> bool {
    is_identifier_prefix(value) && !value.is_empty() && !simplicityhl::lexer::is_keyword(value)
}

fn last_use_keyword(source: &str) -> Option<std::ops::Range<usize>> {
    let (tokens, _) = simplicityhl::lexer::lex(0, source, 0);
    let tokens = tokens?;
    let last_token_end = tokens.last().map_or(0, |(_, span)| span.end);
    if !source.get(last_token_end..)?.trim().is_empty() {
        return None;
    }

    tokens.into_iter().rev().find_map(|(token, span)| {
        matches!(token, simplicityhl::lexer::Token::Use).then_some(span.start..span.end)
    })
}
