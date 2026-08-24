use super::*;
use simplicityhl::error::Location as CompilerLocation;
use simplicityhl::UnstableFeatures;
use tempfile::TempDir;

fn temp_project(source: &str) -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("temp dir");
    std::fs::write(temp.path().join("Simplex.toml"), "").expect("write manifest");
    std::fs::create_dir(temp.path().join("simf")).expect("create source dir");
    let path = temp.path().join("simf/main.simf");
    std::fs::write(&path, source).expect("write source");
    (temp, path)
}

fn analyze_in(temp: &TempDir, path: &Path, source: &str, settings: &Settings) -> AnalysisSnapshot {
    AnalysisSnapshot::analyze(source, path, settings, &[temp.path().to_path_buf()])
}

#[test]
fn parse_failure_retains_the_exact_root_source() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("broken.simf");
    let source = "// 😀\nfn broken() -> u32 ";
    let snapshot = AnalysisSnapshot::analyze(
        source,
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );
    let expected_uri = Uri::from_file_path(&path).unwrap();

    assert_eq!(snapshot.text.to_string(), source);
    assert_eq!(snapshot.sources.len(), 1);
    assert_eq!(snapshot.sources[0].uri, expected_uri);
    assert_eq!(snapshot.sources[0].text.to_string(), source);
    assert!(snapshot
        .compiler_diagnostics
        .iter()
        .any(|diagnostic| matches!(diagnostic.error(), CompilerError::Syntax { .. })));
}

#[test]
fn transiently_missing_dependency_source_is_a_root_diagnostic() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("simf")).unwrap();
    std::fs::create_dir_all(root.join("vendor/library/simf")).unwrap();
    std::fs::write(
        root.join("Simplex.toml"),
        "[dependencies]\nlibrary = { path = 'vendor/library' }\n",
    )
    .unwrap();
    std::fs::write(root.join("vendor/library/Simplex.toml"), "").unwrap();
    std::fs::write(
        root.join("vendor/library/simf/ops.simf"),
        "pub fn verify() {}\n",
    )
    .unwrap();
    let path = root.join("simf/main.simf");
    let source = "use library::ops::verify;\nfn main() { verify(); }\n";
    std::fs::write(&path, source).unwrap();
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .unwrap();
    let roots = [root.to_path_buf()];

    let initial = AnalysisSnapshot::analyze(source, &path, &settings, &roots);
    assert!(initial.compiler_diagnostics.is_empty());
    let dependency_source = root.join("vendor/library/simf");
    std::fs::rename(&dependency_source, root.join("vendor/library/simf.moved")).unwrap();
    let expected_path = dependency_source.display().to_string();

    let missing = AnalysisSnapshot::analyze(source, &path, &settings, &roots);
    assert_eq!(missing.text.to_string(), source);
    assert_eq!(missing.sources[0].uri, Uri::from_file_path(&path).unwrap());
    assert!(missing.compiler_diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic.error(),
            CompilerError::CannotParse { msg } if msg.contains(&expected_path)
        )
    }));
}

#[test]
fn known_scope_does_not_fall_back_to_root_visible_functions() {
    let source = "fn target() {}\nfn owner() { target(); }\n";
    let mut diagnostics = DiagnosticManager::new();
    let program = parse::Program::parse_from_str_with_errors(
        0,
        source,
        &UnstableFeatures::none(),
        &mut diagnostics,
    )
    .expect("valid program");
    let path = std::env::temp_dir().join("module-scope-test.simf");
    let mut snapshot = AnalysisSnapshot::from_program(&program, source, &path);
    let owner = snapshot
        .functions
        .get_func("owner")
        .expect("owner function")
        .clone();

    assert!(snapshot.resolve_custom_call(&owner, "target").is_some());
    snapshot
        .call_scopes
        .insert(*owner.span(), Arc::new(HashMap::new()));
    assert!(snapshot.resolve_custom_call(&owner, "target").is_none());
}

#[test]
fn valid_program_collects_functions() {
    let source = "fn add(a: u32, b: u32) -> u32 { let (_, sum): (bool, u32) = jet::add_32(a, b); sum }\nfn main() {}";
    let (temp, path) = temp_project(source);
    let snapshot = analyze_in(&temp, &path, source, &Settings::default());

    assert!(
        snapshot.compiler_diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        snapshot.compiler_diagnostics
    );
    assert_eq!(snapshot.functions.iter().count(), 2);
}

#[test]
fn library_without_main_keeps_definition_metadata() {
    let source = "fn helper() {}\nfn caller() { helper() }\n";
    let (temp, path) = temp_project(source);
    let snapshot = analyze_in(&temp, &path, source, &Settings::default());

    assert!(snapshot.compiler_diagnostics.is_empty());
    let call_start = source.rfind("helper").expect("helper call");
    let call = snapshot
        .find_related_call(Span::new(0, call_start + 1..call_start + 1))
        .expect("helper call should remain navigable");
    let function = snapshot
        .functions
        .get_func(&call.name().to_string())
        .expect("helper definition");
    assert_eq!(function.name().as_inner(), "helper");
    assert_eq!(function.span().file_id, 0);
    assert_eq!(
        snapshot.sources[0].uri,
        Uri::from_file_path(std::fs::canonicalize(path).unwrap()).unwrap()
    );
}

#[test]
fn nested_main_does_not_conflict_with_the_synthetic_entry_point() {
    let source = "mod nested { fn main() {} }\n";
    let (temp, path) = temp_project(source);
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .unwrap();
    let snapshot = analyze_in(&temp, &path, source, &settings);

    assert!(!snapshot.compiler_diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic.error(),
            CompilerError::FunctionRedefined { name } if name.as_inner() == "main"
        )
    }));
    let expected = CompilerError::MainOutOfEntryFile.to_string();
    assert!(snapshot.compiler_diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic.error(), CompilerError::CannotParse { msg } if msg == &expected)
    }));
}

#[test]
fn enum_analysis_respects_the_feature_setting() {
    let source = "enum Choice { Yes, No, }\nfn main() {}\n";
    let (temp, path) = temp_project(source);
    let disabled = analyze_in(&temp, &path, source, &Settings::default());
    assert!(disabled.compiler_diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic.error(),
            CompilerError::UnstableFeature {
                feature: simplicityhl::UnstableFeature::Enums
            }
        )
    }));

    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": false, "enums": true }
    }))
    .unwrap();
    let enabled = analyze_in(&temp, &path, source, &settings);
    assert!(enabled.compiler_diagnostics.is_empty());
}

#[test]
fn invalid_ast_is_reported_without_discarding_the_snapshot() {
    let source = "fn add(a: u32, b: u32) -> u32 {}\nfn main() {}";
    let (temp, path) = temp_project(source);
    let snapshot = analyze_in(&temp, &path, source, &Settings::default());

    assert!(snapshot.compiler_diagnostics.iter().any(|diagnostic| {
        diagnostic
            .to_string()
            .contains("Expected expression of type `u32`, found type `()`")
    }));
    assert!(snapshot.functions.get_func("add").is_some());
}

#[test]
fn manifest_dependency_and_custom_source_directory_are_resolved() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let write = |path: PathBuf, source: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    };
    write(
        root.join("Simplex.toml"),
        "[build]\nsrc_dir = 'contracts'\n[dependencies]\nmath = { path = 'vendor/math' }\n",
    );
    write(root.join("vendor/math/Simplex.toml"), "");
    write(
        root.join("vendor/math/simf/ops.simf"),
        "pub fn double(a: u32) -> u32 { let (_, n): (bool, u32) = jet::add_32(a, a); n }\n",
    );
    let source = "use math::ops::double;\nfn main() { let _: u32 = double(2); }\n";
    let path = root.join("contracts/main.simf");
    write(path.clone(), source);
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .unwrap();

    let snapshot = AnalysisSnapshot::analyze(source, &path, &settings, &[root.to_path_buf()]);
    assert!(
        snapshot.compiler_diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        snapshot.compiler_diagnostics
    );
    assert!(snapshot.functions.get_func("double").is_some());
}

#[test]
fn duplicate_imported_mains_point_to_distinct_direct_and_transitive_uses() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    std::fs::write(root.join("Simplex.toml"), "").unwrap();
    std::fs::create_dir(root.join("simf")).unwrap();
    std::fs::write(
        root.join("simf/first.simf"),
        "pub fn first() {}\nfn main() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("simf/facade.simf"),
        "pub use crate::leaf::second;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("simf/leaf.simf"),
        "pub fn second() {}\nfn main() {}\n",
    )
    .unwrap();
    let first_import = "use crate::first::first;";
    let second_import = "use crate::facade::second;";
    let source = format!("{first_import}\n{second_import}\nfn main() {{}}\n");
    let path = root.join("simf/main.simf");
    std::fs::write(&path, &source).unwrap();
    let settings = Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .unwrap();

    let snapshot = AnalysisSnapshot::analyze(&source, &path, &settings, &[root.to_path_buf()]);
    let diagnostic = snapshot
        .compiler_diagnostics
        .iter()
        .find(|diagnostic| {
            matches!(
                diagnostic.error(),
                CompilerError::FunctionRedefined { name } if name.as_inner() == "main"
            )
        })
        .expect("duplicate main diagnostic");
    let CompilerLocation::Code(span) = diagnostic.location() else {
        panic!("duplicate main should point to source code");
    };
    assert_eq!(span.to_slice(&source), Some(first_import));
    assert!(diagnostic.secondary().iter().any(|label| {
        label.span.to_slice(&source) == Some(second_import)
            && label.message.contains("Another imported `main`")
    }));
}

#[test]
fn missing_configured_manifest_is_an_analysis_diagnostic() {
    let source = "fn main() {}\n";
    let (temp, path) = temp_project(source);
    let mut settings = Settings::default();
    settings.project.simplex.manifest_path = "nowhere/Simplex.toml".to_string();
    let snapshot = analyze_in(&temp, &path, source, &settings);

    assert!(snapshot
        .compiler_diagnostics
        .iter()
        .any(|diagnostic| diagnostic
            .to_string()
            .contains("Simplex manifest was not found")));
}
