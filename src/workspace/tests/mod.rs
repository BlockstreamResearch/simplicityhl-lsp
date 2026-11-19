mod diagnostics;
mod publication;
mod references;
mod tickets;

use std::fs;
use std::path::{Path, PathBuf};

use smplx_build::config::build::DEFAULT_SRC_DIR_NAME;
use tempfile::TempDir;
use tower_lsp_server::lsp_types::Uri;
use tower_lsp_server::UriExt;

use super::{AnalysisInput, DiagnosticUpdate, WorkspaceState};
use crate::analysis::AnalysisSnapshot;
use crate::config::Settings;
use crate::navigation::FunctionIdentity;

/// A temporary Simplex project with an empty `Simplex.toml`.
struct Project {
    temp: TempDir,
}

/// One source file written into a [`Project`].
struct ProjectFile {
    /// Canonical path, as the compiler reports it.
    path: PathBuf,
    uri: Uri,
    /// The text saved on disk.
    source: String,
}

impl Project {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp dir");
        fs::write(temp.path().join("Simplex.toml"), "").expect("write manifest");

        Self { temp }
    }

    /// Save `name` with `source` in the source directory, where `crate::` imports resolve.
    fn file(&self, name: &str, source: &str) -> ProjectFile {
        let path = self.temp.path().join(DEFAULT_SRC_DIR_NAME).join(name);
        fs::create_dir_all(path.parent().expect("file has a parent")).expect("create directory");
        fs::write(&path, source).expect("write source");

        let path = fs::canonicalize(&path).expect("canonical path");
        let uri = Uri::from_file_path(&path).expect("file URI");

        ProjectFile {
            path,
            uri,
            source: source.to_string(),
        }
    }

    fn analyze(&self, path: &Path, text: &str) -> AnalysisSnapshot {
        let settings_with_imports = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": true }
        }))
        .expect("valid settings");

        AnalysisSnapshot::analyze(
            text,
            path,
            &settings_with_imports,
            &[self.temp.path().to_path_buf()],
        )
    }
}

/// Run the analysis `ticket` asks for and hand the result back,
/// as the server does after analyzing outside the lock.
fn finish(
    state: &mut WorkspaceState,
    project: &Project,
    ticket: &AnalysisInput,
) -> Option<Vec<DiagnosticUpdate>> {
    let path = ticket.uri.to_file_path().expect("ticket for a file");
    let snapshot = project.analyze(&path, &ticket.text);
    state.replace_if_current(&ticket.uri, snapshot, ticket.version, ticket.generation)
}

/// `didOpen` with `text`, followed by its finished analysis.
fn open(
    state: &mut WorkspaceState,
    project: &Project,
    file: &ProjectFile,
    text: &str,
    version: i32,
) -> Vec<DiagnosticUpdate> {
    let ticket = state.begin_open(&file.uri, text, Some(version));
    finish(state, project, &ticket).expect("a fresh ticket is current")
}

fn store(
    state: &mut WorkspaceState,
    project: &Project,
    file: &ProjectFile,
    text: &str,
    version: Option<i32>,
) -> Vec<DiagnosticUpdate> {
    state.store_root(&file.uri, project.analyze(&file.path, text), version, 0)
}

fn identity(state: &WorkspaceState, uri: &Uri, name: &str) -> FunctionIdentity {
    let snapshot = state.get(uri).expect("root analysis");
    let function = snapshot
        .functions
        .get_func(name)
        .expect("function visible in the root");

    snapshot
        .function_identity(function)
        .expect("function identity")
}

fn update_for<'a>(updates: &'a [DiagnosticUpdate], uri: &Uri) -> &'a DiagnosticUpdate {
    updates
        .iter()
        .find(|update| &update.uri == uri)
        .expect("URI should be republished")
}

fn temporary_uri(name: &str) -> Uri {
    Uri::from_file_path(std::env::temp_dir().join(name)).expect("temporary file URI")
}
