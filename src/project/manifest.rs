use std::fs;
use std::path::{Path, PathBuf};

use smplx_build::{BuildConfig, DependencyConfig, CONFIG_FILENAME};

use crate::config::SimplexSettings;

use super::ProjectError;

pub(super) struct SimplexConfig {
    pub(super) build: BuildConfig,
    pub(super) dependencies: DependencyConfig,
}

pub(super) fn load_manifest(path: &Path) -> Result<SimplexConfig, ProjectError> {
    let text = read_to_string(path)?;
    let invalid = |source| ProjectError::Manifest {
        path: path.to_path_buf(),
        source: Box::new(source),
    };

    Ok(SimplexConfig {
        build: BuildConfig::from_source(&text).map_err(invalid)?,
        dependencies: DependencyConfig::from_source(&text).map_err(invalid)?,
    })
}

fn read_to_string(path: &Path) -> Result<String, ProjectError> {
    fs::read_to_string(path).map_err(|source| ProjectError::Read {
        path: path.to_path_buf(),
        source,
    })
}

/// The manifest for `document_path`: the configured one if set, otherwise the nearest one.
/// `None` when Simplex support is off or no manifest exists.
pub(super) fn locate_manifest(
    document_path: &Path,
    settings: &SimplexSettings,
    workspace_root: &Path,
) -> Result<Option<PathBuf>, ProjectError> {
    if !settings.enabled {
        return Ok(None);
    }

    let configured = settings.manifest_path.trim();
    if configured.is_empty() {
        return Ok(find_manifest(document_path));
    }

    let configured = workspace_root.join(configured);
    manifest_at(&configured)
        .map(Some)
        .ok_or(ProjectError::MissingConfiguredManifest(configured))
}

fn manifest_at(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }

    manifest_in(path)
}

fn find_manifest(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    start.ancestors().find_map(manifest_in)
}

pub(super) fn manifest_in(directory: &Path) -> Option<PathBuf> {
    Some(directory.join(CONFIG_FILENAME)).filter(|path| path.is_file())
}
