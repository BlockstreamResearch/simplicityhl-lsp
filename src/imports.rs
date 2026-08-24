use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use ropey::Rope;
use simplicityhl::error::DiagnosticManager;
use simplicityhl::parse::{self, ParseFromStrWithErrors, Visibility};
use simplicityhl::UnstableFeatures;
use tower_lsp_server::lsp_types::{
    CompletionItem, CompletionItemKind, Documentation, MarkupContent, MarkupKind,
};

use crate::completion;
use crate::project::ProjectContext;
use crate::text::{get_comments_from_lines, offset_to_position};

#[derive(Clone, Debug)]
struct Candidate {
    name: String,
    kind: CompletionItemKind,
    detail: String,
    documentation: Option<Documentation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Query {
    Roots,
    Path(Vec<String>),
    Suppressed,
}

/// The unfinished `use` declaration surrounding a completion request.
///
/// This is intentionally derived from source text rather than the compiler AST: while a user is
/// typing `use crate::math::`, there is no complete [`parse::UseDecl`] for the compiler to expose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportCompletionContext {
    use_start: usize,
    query: Query,
    partial: String,
    already_imported: BTreeSet<String>,
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

/// Complete the module path or item list described by `context`.
pub(crate) fn complete_import(
    context: &ImportCompletionContext,
    source: &str,
    current_path: &Path,
    project: &ProjectContext,
) -> Vec<CompletionItem> {
    let candidates = match &context.query {
        Query::Roots => root_candidates(project, current_path),
        Query::Path(segments) => {
            let Some((root_alias, relative_segments)) = segments.split_first() else {
                return Vec::new();
            };
            let Some(root) = project.import_root(current_path, root_alias) else {
                return Vec::new();
            };

            let source_before_use = source.get(..context.use_start).unwrap_or_default();
            let mut candidates =
                candidates_at(root, relative_segments, current_path, source_before_use);

            // Top-level items in the current file are already in scope, but its inline modules
            // are useful path segments. Parse only the complete text before the unfinished use.
            if root_alias == "crate" && relative_segments.is_empty() {
                candidates.extend(parse_inline_module_candidates(source_before_use));
            }
            candidates
        }
        Query::Suppressed => Vec::new(),
    };

    to_completion_items(candidates, &context.partial, &context.already_imported)
}

fn root_candidates(project: &ProjectContext, current_path: &Path) -> Vec<Candidate> {
    let mut candidates = vec![Candidate {
        name: "crate".to_string(),
        kind: CompletionItemKind::MODULE,
        detail: "Current package".to_string(),
        documentation: None,
    }];
    candidates.extend(
        project
            .dependency_aliases(current_path)
            .into_iter()
            .map(|alias| Candidate {
                name: alias.to_string(),
                kind: CompletionItemKind::MODULE,
                detail: project.import_root(current_path, alias).map_or_else(
                    || "Project dependency".to_string(),
                    |path| format!("Project dependency `{}`", path.display()),
                ),
                documentation: None,
            }),
    );
    candidates
}

fn to_completion_items(
    candidates: Vec<Candidate>,
    partial: &str,
    already_imported: &BTreeSet<String>,
) -> Vec<CompletionItem> {
    let mut unique = BTreeMap::new();
    for candidate in candidates {
        if candidate.name.starts_with(partial) && !already_imported.contains(&candidate.name) {
            unique.insert(
                (candidate.name.clone(), candidate.detail.clone()),
                candidate,
            );
        }
    }

    unique
        .into_values()
        .map(|candidate| CompletionItem {
            label: candidate.name,
            kind: Some(candidate.kind),
            detail: Some(candidate.detail),
            documentation: candidate.documentation,
            ..CompletionItem::default()
        })
        .collect()
}

fn candidates_at(
    root: &Path,
    segments: &[String],
    current_path: &Path,
    source_before_use: &str,
) -> Vec<Candidate> {
    let mut cursor = root.to_path_buf();
    for (index, segment) in segments.iter().enumerate() {
        let directory = cursor.join(segment);
        if directory.is_dir() {
            cursor = directory;
            continue;
        }

        let file = cursor.join(format!("{segment}.simf"));
        if file.is_file() {
            return parse_file_candidates(&file, &segments[index + 1..]);
        }

        // If filesystem routing never left the package root, `crate::` may instead be
        // navigating inline modules in the current source file.
        if cursor == root {
            return parse_source_candidates(source_before_use, &segments[index..]);
        }
        return Vec::new();
    }

    list_directory(&cursor, current_path)
}

fn list_directory(directory: &Path, current_path: &Path) -> Vec<Candidate> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let canonical_current =
        fs::canonicalize(current_path).unwrap_or_else(|_| current_path.to_path_buf());

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let canonical_path = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if canonical_path == canonical_current {
                return None;
            }
            if path.is_dir() {
                return path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| is_identifier(name))
                    .map(|name| Candidate {
                        name: name.to_string(),
                        kind: CompletionItemKind::MODULE,
                        detail: format!("Module directory `{}`", path.display()),
                        documentation: None,
                    });
            }

            (path
                .extension()
                .is_some_and(|extension| extension == "simf"))
            .then(|| path.file_stem().and_then(|name| name.to_str()))
            .flatten()
            .filter(|name| is_identifier(name))
            .map(|name| Candidate {
                name: name.to_string(),
                kind: CompletionItemKind::MODULE,
                detail: format!("Module file `{}`", path.display()),
                documentation: None,
            })
        })
        .collect()
}

fn parse_file_candidates(path: &Path, inline_segments: &[String]) -> Vec<Candidate> {
    let Ok(source) = fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_source_candidates(&source, inline_segments)
}

fn parse_program(source: &str) -> Option<parse::Program> {
    let mut diagnostics = DiagnosticManager::new();
    parse::Program::parse_from_str_with_errors(
        0,
        source,
        &UnstableFeatures::all(),
        &mut diagnostics,
    )
}

fn parse_source_candidates(source: &str, inline_segments: &[String]) -> Vec<Candidate> {
    let Some(program) = parse_program(source) else {
        return Vec::new();
    };
    candidates_from_items(program.items(), source, inline_segments)
}

fn parse_inline_module_candidates(source: &str) -> Vec<Candidate> {
    let Some(program) = parse_program(source) else {
        return Vec::new();
    };

    program
        .items()
        .iter()
        .filter_map(|item| match item {
            parse::Item::Module(module) => Some(Candidate {
                name: module.name().to_string(),
                kind: CompletionItemKind::MODULE,
                detail: "Inline module".to_string(),
                documentation: None,
            }),
            _ => None,
        })
        .collect()
}

fn candidates_from_items(
    items: &[parse::Item],
    source: &str,
    inline_segments: &[String],
) -> Vec<Candidate> {
    if let Some((segment, rest)) = inline_segments.split_first() {
        let Some(module) = items.iter().find_map(|item| match item {
            parse::Item::Module(module) if module.name().as_inner() == segment => Some(module),
            _ => None,
        }) else {
            return Vec::new();
        };
        return candidates_from_items(module.items(), source, rest);
    }

    let rope = Rope::from_str(source);
    let mut candidates = Vec::new();
    for item in items {
        match item {
            parse::Item::Function(function)
                if matches!(function.visibility(), Visibility::Public) =>
            {
                let start_line = offset_to_position(function.span().start, &rope)
                    .unwrap_or_default()
                    .line;
                let documentation = get_comments_from_lines(start_line, &rope);
                let template = completion::function_to_template(function, &documentation);
                candidates.push(Candidate {
                    name: function.name().to_string(),
                    kind: CompletionItemKind::FUNCTION,
                    detail: template.get_signature(),
                    documentation: (!documentation.is_empty()).then_some(
                        Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: documentation,
                        }),
                    ),
                });
            }
            parse::Item::TypeAlias(alias) if matches!(alias.visibility(), Visibility::Public) => {
                candidates.push(Candidate {
                    name: alias.name().to_string(),
                    kind: CompletionItemKind::TYPE_PARAMETER,
                    detail: format!("type {} = {}", alias.name(), alias.ty()),
                    documentation: None,
                });
            }
            parse::Item::EnumDeclaration(declaration)
                if matches!(declaration.visibility(), Visibility::Public) =>
            {
                candidates.push(Candidate {
                    name: declaration.name().to_string(),
                    kind: CompletionItemKind::ENUM,
                    detail: format!("enum {}", declaration.name()),
                    documentation: None,
                });
            }
            parse::Item::Module(module) if matches!(module.visibility(), Visibility::Public) => {
                candidates.push(Candidate {
                    name: module.name().to_string(),
                    kind: CompletionItemKind::MODULE,
                    detail: "Public inline module".to_string(),
                    documentation: None,
                });
            }
            parse::Item::Use(use_decl) if matches!(use_decl.visibility(), Visibility::Public) => {
                let items = match use_decl.items() {
                    parse::UseItems::Single(item) => std::slice::from_ref(item),
                    parse::UseItems::List(items) => items.as_slice(),
                };
                candidates.extend(items.iter().map(|(original, alias)| Candidate {
                    name: alias.as_ref().unwrap_or(original).to_string(),
                    kind: CompletionItemKind::REFERENCE,
                    detail: "Public re-export".to_string(),
                    documentation: None,
                }));
            }
            parse::Item::TypeAlias(_)
            | parse::Item::Function(_)
            | parse::Item::Use(_)
            | parse::Item::EnumDeclaration(_)
            | parse::Item::Module(_)
            | parse::Item::Ignored => {}
        }
    }
    candidates
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

fn is_identifier(value: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::config::ProjectSettings;

    fn write(path: &Path, source: &str) {
        fs::create_dir_all(path.parent().expect("test file has a parent")).unwrap();
        fs::write(path, source).unwrap();
    }

    fn labels_for(root: &Path, source: &str) -> Vec<String> {
        write(root, source);
        let project = ProjectContext::discover(
            root,
            &ProjectSettings::default(),
            &[root
                .parent()
                .and_then(Path::parent)
                .expect("source has a project root")
                .to_path_buf()],
        )
        .unwrap();
        let context = ImportCompletionContext::at(source, source.len()).expect("import context");
        complete_import(&context, source, root, &project)
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    #[test]
    fn completes_dependency_roots_modules_and_public_functions() {
        let temp = TempDir::new().unwrap();
        write(
            &temp.path().join("Simplex.toml"),
            "[dependencies]\nstd = { path = 'vendor/std' }\n",
        );
        write(&temp.path().join("vendor/std/Simplex.toml"), "");
        write(
            &temp.path().join("vendor/std/simf/math.simf"),
            "// Add two words.\npub fn add(a: u32, b: u32) -> u32 { a }\nfn hidden() {}\n",
        );
        let root = temp.path().join("simf/main.simf");

        assert_eq!(labels_for(&root, "use st"), vec!["std"]);
        assert_eq!(labels_for(&root, "use std::ma"), vec!["math"]);
        assert_eq!(labels_for(&root, "use std::math::"), vec!["add"]);
    }

    #[test]
    fn completes_grouped_imports_without_repeating_selected_items() {
        let temp = TempDir::new().unwrap();
        write(&temp.path().join("Simplex.toml"), "");
        write(
            &temp.path().join("simf/math.simf"),
            "pub fn add() {}\npub fn subtract() {}\n",
        );
        let root = temp.path().join("simf/main.simf");

        assert_eq!(
            labels_for(&root, "use crate::math::{add, "),
            vec!["subtract"]
        );
    }

    #[test]
    fn completes_current_file_inline_modules() {
        let temp = TempDir::new().unwrap();
        write(&temp.path().join("Simplex.toml"), "");
        let root = temp.path().join("simf/main.simf");
        let source = "pub mod math { pub fn add() {} fn hidden() {} }\nuse crate::math::";

        assert_eq!(labels_for(&root, source), vec!["add"]);
    }

    #[test]
    fn crate_root_excludes_the_current_file_and_its_already_visible_items() {
        let temp = TempDir::new().unwrap();
        write(&temp.path().join("Simplex.toml"), "");
        let root = temp.path().join("simf/main.simf");
        let source = "pub fn helper() {}\nfn hidden() {}\nmod inline_math {}\nuse crate::";
        let labels = labels_for(&root, source);

        assert!(!labels.contains(&"main".to_string()));
        assert!(!labels.contains(&"helper".to_string()));
        assert!(!labels.contains(&"hidden".to_string()));
        assert!(labels.contains(&"inline_math".to_string()));
    }

    #[test]
    fn ignores_use_keywords_in_comments() {
        let source = "fn main() {}\n// use crate::";
        assert!(ImportCompletionContext::at(source, source.len()).is_none());

        let source = "fn main() {}\n/* use crate:: */";
        assert!(ImportCompletionContext::at(source, source.len()).is_none());

        let source = "fn main() {}\n/* use crate::";
        assert!(ImportCompletionContext::at(source, source.len()).is_none());

        let source = "use crate::\n/* unfinished";
        assert!(ImportCompletionContext::at(source, source.len()).is_none());

        let source = "use crate::\n// unfinished";
        assert!(ImportCompletionContext::at(source, source.len()).is_none());
    }

    #[test]
    fn malformed_import_paths_do_not_offer_misleading_candidates() {
        for source in ["use crate::::", "use crate::math:{", "use ::math::"] {
            let context = ImportCompletionContext::at(source, source.len()).unwrap();
            assert_eq!(context.query, Query::Suppressed, "{source}");
        }
    }

    #[test]
    fn function_completion_includes_signature_and_documentation() {
        let temp = TempDir::new().unwrap();
        write(&temp.path().join("Simplex.toml"), "");
        write(
            &temp.path().join("simf/math.simf"),
            "/// Add two words.\npub fn add(a: u32, b: u32) -> u32 { a }\n",
        );
        let root = temp.path().join("simf/main.simf");
        let source = "use crate::math::";
        write(&root, source);
        let project = ProjectContext::discover(
            &root,
            &ProjectSettings::default(),
            &[temp.path().to_path_buf()],
        )
        .unwrap();
        let context = ImportCompletionContext::at(source, source.len()).unwrap();
        let items = complete_import(&context, source, &root, &project);

        assert_eq!(
            items[0].detail.as_deref(),
            Some("fn(a: u32, b: u32) -> u32")
        );
        assert!(items[0].documentation.is_some());
        assert!(items[0].insert_text.is_none());
    }
}
