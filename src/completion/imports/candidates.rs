//! Project- and source-backed candidates for incomplete import declarations.

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

use super::context::{is_identifier, ImportCompletionContext, Query};

#[derive(Clone, Debug)]
struct Candidate {
    name: String,
    kind: CompletionItemKind,
    detail: String,
    documentation: Option<Documentation>,
}

impl Candidate {
    fn plain(name: impl Into<String>, kind: CompletionItemKind, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind,
            detail: detail.into(),
            documentation: None,
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
    let mut candidates = vec![Candidate::plain(
        "crate",
        CompletionItemKind::MODULE,
        "Current package",
    )];
    candidates.extend(
        project
            .dependency_aliases(current_path)
            .into_iter()
            .map(|alias| {
                let detail = project.import_root(current_path, alias).map_or_else(
                    || "Project dependency".to_string(),
                    |path| format!("Project dependency `{}`", path.display()),
                );
                Candidate::plain(alias, CompletionItemKind::MODULE, detail)
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
                    .map(|name| {
                        Candidate::plain(
                            name,
                            CompletionItemKind::MODULE,
                            format!("Module directory `{}`", path.display()),
                        )
                    });
            }

            (path
                .extension()
                .is_some_and(|extension| extension == "simf"))
            .then(|| path.file_stem().and_then(|name| name.to_str()))
            .flatten()
            .filter(|name| is_identifier(name))
            .map(|name| {
                Candidate::plain(
                    name,
                    CompletionItemKind::MODULE,
                    format!("Module file `{}`", path.display()),
                )
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
            parse::Item::Module(module) => Some(Candidate::plain(
                module.name().to_string(),
                CompletionItemKind::MODULE,
                "Inline module",
            )),
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
                candidates.push(Candidate::plain(
                    alias.name().to_string(),
                    CompletionItemKind::TYPE_PARAMETER,
                    format!("type {} = {}", alias.name(), alias.ty()),
                ));
            }
            parse::Item::EnumDeclaration(declaration)
                if matches!(declaration.visibility(), Visibility::Public) =>
            {
                candidates.push(Candidate::plain(
                    declaration.name().to_string(),
                    CompletionItemKind::ENUM,
                    format!("enum {}", declaration.name()),
                ));
            }
            parse::Item::Module(module) if matches!(module.visibility(), Visibility::Public) => {
                candidates.push(Candidate::plain(
                    module.name().to_string(),
                    CompletionItemKind::MODULE,
                    "Public inline module",
                ));
            }
            parse::Item::Use(use_decl) if matches!(use_decl.visibility(), Visibility::Public) => {
                let items = match use_decl.items() {
                    parse::UseItems::Single(item) => std::slice::from_ref(item),
                    parse::UseItems::List(items) => items.as_slice(),
                };
                candidates.extend(items.iter().map(|(original, alias)| {
                    Candidate::plain(
                        alias.as_ref().unwrap_or(original).to_string(),
                        CompletionItemKind::REFERENCE,
                        "Public re-export",
                    )
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
