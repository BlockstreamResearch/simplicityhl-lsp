use std::fs;

use tempfile::TempDir;

use super::*;
use crate::config::{ManualDependencyDetails, SimplexSettings};

fn write(path: &Path, source: &str) {
    fs::create_dir_all(path.parent().expect("test file has a parent")).unwrap();
    fs::write(path, source).unwrap();
}

#[test]
fn discovers_manifest_and_recursive_path_dependencies() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(
        &root.join(SIMPLEX_MANIFEST),
        "[build]\nsrc_dir = 'contracts'\n[dependencies]\nmerkle = { path = 'vendor/merkle' }\n",
    );
    write(&root.join("contracts/main.simf"), "fn main() {}\n");
    write(
        &root.join("vendor/merkle/Simplex.toml"),
        "[dependencies]\nmath = { path = '../math' }\n",
    );
    write(
        &root.join("vendor/merkle/simf/root.simf"),
        "use math::ops::add;\npub fn root() { add(); }\n",
    );
    write(&root.join("vendor/math/Simplex.toml"), "");
    write(&root.join("vendor/math/simf/ops.simf"), "pub fn add() {}\n");

    let context = ProjectContext::discover(
        &root.join("contracts/main.simf"),
        &ProjectSettings::default(),
        &[root.to_path_buf()],
    )
    .unwrap();

    assert_eq!(
        context.source_root,
        fs::canonicalize(root.join("contracts")).unwrap()
    );
    assert_eq!(context.dependencies.len(), 2);
    assert_eq!(
        context.import_root(&root.join("contracts/main.simf"), "merkle"),
        Some(
            fs::canonicalize(root.join("vendor/merkle/simf"))
                .unwrap()
                .as_path()
        )
    );
    assert_eq!(
        context.import_root(&root.join("vendor/merkle/simf/root.simf"), "math"),
        Some(
            fs::canonicalize(root.join("vendor/math/simf"))
                .unwrap()
                .as_path()
        )
    );
    assert_eq!(
        context.dependency_aliases(&root.join("contracts/main.simf")),
        vec!["merkle"]
    );
    assert_eq!(
        context.dependency_aliases(&root.join("vendor/merkle/simf/root.simf")),
        vec!["math"]
    );
}

#[test]
fn dependency_removed_after_discovery_is_reported() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let document = root.join("simf/main.simf");
    let dependency_source = root.join("vendor/library/simf");
    write(
        &root.join(SIMPLEX_MANIFEST),
        "[dependencies]\nlibrary = { path = 'vendor/library' }\n",
    );
    write(&document, "fn main() {}\n");
    write(&root.join("vendor/library/Simplex.toml"), "");
    write(&dependency_source.join("ops.simf"), "pub fn verify() {}\n");

    let context = ProjectContext::discover(
        &document,
        &ProjectSettings::default(),
        &[root.to_path_buf()],
    )
    .unwrap();
    fs::rename(&dependency_source, root.join("vendor/library/simf.moved")).unwrap();

    let error = context.dependency_map(&document).unwrap_err();
    let ProjectError::Compiler(message) = error else {
        panic!("expected compiler dependency-map error, got {error}");
    };
    assert!(message.contains("Failed to find library target path"));
    assert!(message.contains("simf"));
}

#[test]
fn resolves_simplex_git_install_directory_exactly() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let url = "https://github.com/BlockstreamResearch/simplicityhl-std";
    let installed = root
        .join("deps")
        .join(hashed_repository_path(url, None).unwrap());
    assert_eq!(
        installed.file_name().unwrap(),
        "simplicityhl-std-8bc347cc4ed271da"
    );
    write(
        &root.join(SIMPLEX_MANIFEST),
        &format!("[dependencies]\nstd = {{ git = '{url}' }}\n"),
    );
    write(&root.join("simf/main.simf"), "fn main() {}\n");
    write(&installed.join(SIMPLEX_MANIFEST), "");
    write(&installed.join("simf/lib.simf"), "pub fn helper() {}\n");

    let context = ProjectContext::discover(
        &root.join("simf/main.simf"),
        &ProjectSettings::default(),
        &[root.to_path_buf()],
    )
    .unwrap();

    assert_eq!(
        context.import_root(&root.join("simf/main.simf"), "std"),
        Some(fs::canonicalize(installed.join("simf")).unwrap().as_path())
    );
}

#[test]
fn resolves_simplex_git_install_directories_for_revision_and_tag() {
    for (field, reference, expected_directory) in [
        ("rev", "deadbeef", "simplicityhl-std-c7c631fb6d854c6d"),
        ("tag", "v1.2.3", "simplicityhl-std-38569687e465cad1"),
    ] {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let url = "https://github.com/BlockstreamResearch/simplicityhl-std";
        let installed = root
            .join("deps")
            .join(hashed_repository_path(url, Some(reference)).unwrap());
        assert_eq!(installed.file_name().unwrap(), expected_directory);
        write(
            &root.join(SIMPLEX_MANIFEST),
            &format!("[dependencies]\nstd = {{ git = '{url}', {field} = '{reference}' }}\n"),
        );
        write(&root.join("simf/main.simf"), "fn main() {}\n");
        write(&installed.join(SIMPLEX_MANIFEST), "");
        write(&installed.join("simf/lib.simf"), "pub fn helper() {}\n");

        let context = ProjectContext::discover(
            &root.join("simf/main.simf"),
            &ProjectSettings::default(),
            &[root.to_path_buf()],
        )
        .unwrap();

        assert_eq!(
            context.import_root(&root.join("simf/main.simf"), "std"),
            Some(fs::canonicalize(installed.join("simf")).unwrap().as_path())
        );
    }
}

#[test]
fn rejects_conflicting_simplex_git_references() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(
        &root.join(SIMPLEX_MANIFEST),
        "[dependencies]\nstd = { git = 'https://example.com/std', rev = 'deadbeef', tag = 'v1' }\n",
    );
    write(&root.join("simf/main.simf"), "fn main() {}\n");

    let error = ProjectContext::discover(
        &root.join("simf/main.simf"),
        &ProjectSettings::default(),
        &[root.to_path_buf()],
    )
    .unwrap_err();

    assert!(matches!(error, ProjectError::InvalidDependency { .. }));
}

#[test]
fn rejects_git_references_on_path_dependencies() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(
        &root.join(SIMPLEX_MANIFEST),
        "[dependencies]\nstd = { path = 'vendor/std', rev = 'deadbeef' }\n",
    );
    write(&root.join("simf/main.simf"), "fn main() {}\n");

    let error = ProjectContext::discover(
        &root.join("simf/main.simf"),
        &ProjectSettings::default(),
        &[root.to_path_buf()],
    )
    .unwrap_err();

    assert!(matches!(error, ProjectError::InvalidDependency { .. }));
}

#[test]
fn manual_mapping_overrides_manifest_mapping() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(
        &root.join(SIMPLEX_MANIFEST),
        "[dependencies]\nmath = { path = 'old_math' }\n",
    );
    write(&root.join("simf/main.simf"), "fn main() {}\n");
    write(&root.join("old_math/Simplex.toml"), "");
    write(&root.join("old_math/simf/old.simf"), "pub fn old() {}\n");
    write(&root.join("new_math/new.simf"), "pub fn new() {}\n");

    let mut settings = ProjectSettings {
        simplex: SimplexSettings::default(),
        ..ProjectSettings::default()
    };
    settings.dependencies.insert(
        "math".to_string(),
        ManualDependency::Detailed(ManualDependencyDetails {
            path: "new_math".to_string(),
            context: "simf".to_string(),
        }),
    );

    let context = ProjectContext::discover(
        &root.join("simf/main.simf"),
        &settings,
        &[root.to_path_buf()],
    )
    .unwrap();

    assert_eq!(
        context.import_root(&root.join("simf/main.simf"), "math"),
        Some(fs::canonicalize(root.join("new_math")).unwrap().as_path())
    );
}

#[test]
fn source_override_is_used_as_the_dependency_context() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(
        &root.join(SIMPLEX_MANIFEST),
        "[dependencies]\nmath = { path = 'math' }\n",
    );
    write(&root.join("contracts/main.simf"), "fn main() {}\n");
    write(&root.join("math/Simplex.toml"), "");
    write(&root.join("math/simf/math.simf"), "pub fn add() {}\n");
    let settings = ProjectSettings {
        source_directory: "contracts".to_string(),
        ..ProjectSettings::default()
    };

    let context = ProjectContext::discover(
        &root.join("contracts/main.simf"),
        &settings,
        &[root.to_path_buf()],
    )
    .unwrap();

    assert_eq!(
        context.dependencies[0].context,
        fs::canonicalize(root.join("contracts")).unwrap()
    );
}

#[test]
fn reports_a_missing_explicit_manifest() {
    let temp = TempDir::new().unwrap();
    write(&temp.path().join("simf/main.simf"), "fn main() {}\n");
    let mut settings = ProjectSettings::default();
    settings.simplex.manifest_path = "missing.toml".to_string();

    let error = ProjectContext::discover(
        &temp.path().join("simf/main.simf"),
        &settings,
        &[temp.path().to_path_buf()],
    )
    .unwrap_err();

    assert!(matches!(error, ProjectError::MissingConfiguredManifest(_)));
}
