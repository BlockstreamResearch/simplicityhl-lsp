use std::path::Path;

use ropey::Rope;
use simplicityhl::error::Span;
use tempfile::TempDir;
use tower_lsp_server::UriExt;

use super::*;
use crate::config::Settings;
use crate::text::offset_to_position;

fn imports_enabled() -> Settings {
    Settings::from_json(serde_json::json!({
        "experimentalFeatures": { "imports": true }
    }))
    .expect("valid settings")
}

fn write(path: impl AsRef<Path>, source: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().expect("path has parent")).unwrap();
    std::fs::write(path, source).unwrap();
}

fn temp_snapshot(source: &str) -> (TempDir, AnalysisSnapshot) {
    let temp = TempDir::new().unwrap();
    write(temp.path().join("Simplex.toml"), "");
    let path = temp.path().join("simf/main.simf");
    write(&path, source);
    let snapshot = AnalysisSnapshot::analyze(
        source,
        &path,
        &Settings::default(),
        &[temp.path().to_path_buf()],
    );
    (temp, snapshot)
}

#[test]
fn grouped_items_and_aliases_resolve_to_imported_definitions() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root.join("Simplex.toml"), "");
    let dependency = root.join("simf/math.simf");
    write(&dependency, "pub fn add() {}\npub fn subtract() {}\n");
    let source = "use crate::math::{add as plus, subtract};\nfn main() { plus(); subtract() }\n";
    let path = root.join("simf/main.simf");
    write(&path, source);
    let snapshot =
        AnalysisSnapshot::analyze(source, &path, &imports_enabled(), &[root.to_path_buf()]);
    assert!(snapshot.compiler_diagnostics.is_empty());

    for (needle, expected_name) in [
        ("add as", "add"),
        ("plus,", "add"),
        ("subtract}", "subtract"),
    ] {
        let offset = source.find(needle).unwrap() + 1;
        let function = snapshot
            .find_imported_function(Span::new(0, offset..offset))
            .expect("imported function");
        assert_eq!(function.name().as_inner(), expected_name);
        assert_eq!(
            snapshot.sources[function.span().file_id].uri,
            Uri::from_file_path(std::fs::canonicalize(&dependency).unwrap()).unwrap()
        );
    }
    let module = source.find("math").unwrap() + 1;
    assert!(snapshot
        .find_imported_function(Span::new(0, module..module))
        .is_none());
}

#[test]
fn non_crate_reexports_keep_original_uri_span_and_alias_identity() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(
        root.join("Simplex.toml"),
        "[dependencies]\nmerkle = { path = 'deps/merkle' }\nfacade = { path = 'deps/facade' }\n",
    );
    write(root.join("deps/merkle/Simplex.toml"), "");
    let merkle = root.join("deps/merkle/simf/build_root.simf");
    write(
        &merkle,
        "pub mod wrapper {\n pub fn get_root() {}\n pub fn hash() {}\n}\npub use crate::wrapper::{get_root, hash};\n",
    );
    write(
        root.join("deps/facade/Simplex.toml"),
        "[dependencies]\nleaf = { path = '../leaf' }\n",
    );
    write(
        root.join("deps/facade/simf/smth.simf"),
        "pub use leaf::ops::hash;\n",
    );
    write(root.join("deps/leaf/Simplex.toml"), "");
    let leaf = root.join("deps/leaf/simf/ops.simf");
    write(&leaf, "pub fn hash() {}\n");
    let source = "use merkle::build_root::{get_root, hash as and_hash};\nuse facade::smth::hash as or_hash;\nfn main() { get_root(); and_hash(); or_hash(); }\n";
    let path = root.join("simf/main.simf");
    write(&path, source);
    let snapshot =
        AnalysisSnapshot::analyze(source, &path, &imports_enabled(), &[root.to_path_buf()]);
    assert!(snapshot.compiler_diagnostics.is_empty());

    let imported = |offset: usize| {
        snapshot
            .find_imported_function(Span::new(0, offset + 1..offset + 1))
            .expect("imported function")
    };
    let cases = [
        (source.find("get_root").unwrap(), "get_root", &merkle),
        (source.find("and_hash").unwrap(), "hash", &merkle),
        (source.rfind("hash as").unwrap(), "hash", &leaf),
        (source.find("or_hash").unwrap(), "hash", &leaf),
    ];
    for (offset, expected_name, expected_path) in cases {
        let function = imported(offset);
        assert_eq!(function.name().as_inner(), expected_name);
        assert_eq!(
            snapshot.sources[function.span().file_id].uri,
            Uri::from_file_path(std::fs::canonicalize(expected_path).unwrap()).unwrap()
        );
    }

    let root_uri = Uri::from_file_path(&path).unwrap();
    let merkle_uri = Uri::from_file_path(std::fs::canonicalize(&merkle).unwrap()).unwrap();
    let leaf_uri = Uri::from_file_path(std::fs::canonicalize(&leaf).unwrap()).unwrap();
    let merkle_root = Location::new(
        merkle_uri.clone(),
        Range::new(Position::new(1, 1), Position::new(1, 21)),
    );
    let merkle_hash = Location::new(
        merkle_uri,
        Range::new(Position::new(2, 1), Position::new(2, 17)),
    );
    let leaf_hash = Location::new(
        leaf_uri,
        Range::new(Position::new(0, 0), Position::new(0, 16)),
    );
    let definition_cases = [
        (source.find("get_root").unwrap(), &merkle_root),
        (source.find("hash as").unwrap(), &merkle_hash),
        (source.find("and_hash").unwrap(), &merkle_hash),
        (source.rfind("hash as").unwrap(), &leaf_hash),
        (source.find("or_hash").unwrap(), &leaf_hash),
        (source.rfind("get_root").unwrap(), &merkle_root),
        (source.rfind("and_hash").unwrap(), &merkle_hash),
        (source.rfind("or_hash").unwrap(), &leaf_hash),
    ];
    for (offset, expected) in definition_cases {
        let position = offset_to_position(offset + 1, &snapshot.text).unwrap();
        let GotoDefinitionResponse::Scalar(location) = snapshot
            .definition_at(&root_uri, position)
            .unwrap()
            .expect("definition")
        else {
            panic!("expected one location");
        };
        assert_eq!(&location, expected);
    }
}

#[test]
fn function_selection_range_is_inside_its_full_range() {
    let source = "/* 😀 */ fn main() {}";
    let (_temp, snapshot) = temp_snapshot(source);
    let function = snapshot.functions.get_func("main").expect("main function");
    let (start, end) = span_to_positions(function.span(), &snapshot.text).unwrap();
    let selection = snapshot.find_function_name_range(function).unwrap();
    let name_start = source.find("main").unwrap();

    assert!(selection.start >= start && selection.end <= end);
    assert_eq!(
        selection,
        Range::new(
            offset_to_position(name_start, &snapshot.text).unwrap(),
            offset_to_position(name_start + "main".len(), &snapshot.text).unwrap(),
        )
    );
}

#[test]
fn stale_text_cannot_produce_an_out_of_bounds_selection_range() {
    let source = "fn main() {}";
    let (_temp, mut snapshot) = temp_snapshot(source);
    let function = snapshot.functions.get_func("main").unwrap().clone();
    snapshot.text = Rope::from_str(&format!("// {}\n{source}", "x".repeat(100)));

    assert!(snapshot.find_function_name_range(&function).is_err());
}

#[test]
fn looking_for_a_call_outside_a_function_is_empty() {
    let (_temp, snapshot) = temp_snapshot("/* heading */\nfn main() {}");
    assert!(snapshot.find_related_call(Span::new(0, 0..0)).is_none());
}
