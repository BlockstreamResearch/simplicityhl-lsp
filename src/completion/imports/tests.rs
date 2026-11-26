use std::fs;

use simplicityhl::{UnstableFeature, UnstableFeatures};
use tempfile::TempDir;
use tower_lsp_server::lsp_types::CompletionItem;

use super::context::Query;
use super::*;
use crate::config::ProjectSettings;
use crate::project::ProjectContext;

/// A temporary Simplex project with an empty `Simplex.toml`, completing in `simf/main.simf`.
struct Project {
    temp: TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self {
            temp: TempDir::new().unwrap(),
        };
        project.write("Simplex.toml", "");
        project
    }

    fn write(&self, relative_path: &str, source: &str) -> &Self {
        let path = self.temp.path().join(relative_path);

        fs::create_dir_all(path.parent().expect("path has a parent")).unwrap();
        fs::write(path, source).unwrap();

        self
    }

    /// Completion at the end of `source`, saved as `simf/main.simf`, with only `imports` enabled.
    fn complete(&self, source: &str) -> Vec<CompletionItem> {
        self.write("simf/main.simf", source);

        let main = self.temp.path().join("simf/main.simf");
        let project = ProjectContext::discover(
            &main,
            &ProjectSettings::default(),
            &[self.temp.path().to_path_buf()],
        )
        .unwrap();

        let context = ImportCompletionContext::at(source, source.len()).expect("import context");
        let features = UnstableFeatures::new(vec![UnstableFeature::Imports]);
        complete_import(&context, source, &main, &project, &features)
    }

    fn labels(&self, source: &str) -> Vec<String> {
        self.complete(source)
            .into_iter()
            .map(|item| item.label)
            .collect()
    }
}

#[test]
fn completes_dependency_roots_modules_and_public_functions() {
    let project = Project::new();
    project
        .write(
            "Simplex.toml",
            "[dependencies]\nstd = { path = 'vendor/std' }\n",
        )
        .write("vendor/std/Simplex.toml", "")
        .write(
            "vendor/std/simf/math.simf",
            "// Add two words.\npub fn add(a: u32, b: u32) -> u32 { a }\nfn hidden() {}\n",
        );

    assert_eq!(project.labels("use st"), vec!["std"]);
    assert_eq!(project.labels("use std::ma"), vec!["math"]);
    assert_eq!(project.labels("use std::math::"), vec!["add"]);
}

#[test]
fn completes_grouped_imports_without_repeating_selected_items() {
    let project = Project::new();
    project.write("simf/math.simf", "pub fn add() {}\npub fn subtract() {}\n");

    assert_eq!(project.labels("use crate::math::{add, "), vec!["subtract"]);
}

#[test]
fn completes_current_file_inline_modules() {
    let source = "pub mod math { pub fn add() {} fn hidden() {} }\nuse crate::math::";

    assert_eq!(Project::new().labels(source), vec!["add"]);
}

#[test]
fn crate_root_excludes_the_current_file_and_its_already_visible_items() {
    let source = "pub fn helper() {}\nfn hidden() {}\nmod inline_math {}\nuse crate::";
    let labels = Project::new().labels(source);

    for hidden in ["main", "helper", "hidden"] {
        assert!(!labels.contains(&hidden.to_string()), "{hidden}");
    }
    assert!(labels.contains(&"inline_math".to_string()));
}

#[test]
fn ignores_use_keywords_in_comments() {
    for source in [
        "fn main() {}\n// use crate::",
        "fn main() {}\n/* use crate:: */",
        "fn main() {}\n/* use crate::",
        "use crate::\n/* unfinished",
        "use crate::\n// unfinished",
    ] {
        assert!(
            ImportCompletionContext::at(source, source.len()).is_none(),
            "{source}"
        );
    }
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
    let project = Project::new();
    project.write(
        "simf/math.simf",
        "/// Add two words.\npub fn add(a: u32, b: u32) -> u32 { a }\n",
    );

    assert_eq!(
        serde_json::to_value(project.complete("use crate::math::")).unwrap(),
        serde_json::json!([{
            "label": "add",
            "kind": 3,
            "detail": "fn(a: u32, b: u32) -> u32",
            "documentation": {
                "kind": "markdown",
                "value": "Add two words."
            }
        }])
    );
}

/// The target file is parsed with the user's features, as the compiler parses it.
/// Suppose we have a file with `pub enum Shape { ... }` and `fn area() {}`.
/// Currently, if enums are off, the `parse_from_str_with_errors` function from `SimplicityHL`
/// fails and returns `None`, so we cannot autocomplete `area`.
///
/// Revisit and change this once `parse_from_str_with_errors`
/// can work with multiple errors.
#[test]
fn file_using_a_disabled_feature_offers_no_items() {
    let project = Project::new();
    project
        .write("simf/plain.simf", "pub fn area() {}\n")
        .write(
            "simf/shapes.simf",
            "pub enum Shape { Circle, Square }\npub fn area() {}\n",
        );

    assert_eq!(project.labels("use crate::plain::"), vec!["area"]);
    assert!(project.labels("use crate::shapes::").is_empty());
}
