use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::context::Query;
use super::*;
use crate::config::ProjectSettings;
use crate::project::ProjectContext;

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
    assert_eq!(
        serde_json::to_value(&items).unwrap(),
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
