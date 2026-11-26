mod collector;
mod error;
mod lookup;
mod manifest;
#[cfg(test)]
mod tests;

use std::fs;
use std::path::{Path, PathBuf};

use simplicityhl::source::CanonPath;

use crate::config::ProjectSettings;

use self::collector::ProjectCollector;
pub use self::error::ProjectError;
use self::manifest::{load_manifest, locate_manifest};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyMapping {
    pub context: PathBuf,
    pub alias: String,
    pub target: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ProjectContext {
    pub source_root: PathBuf,
    pub dependencies: Vec<DependencyMapping>,
    pub package_roots: Vec<PathBuf>,
}

impl ProjectContext {
    pub fn discover(
        document_path: &Path,
        settings: &ProjectSettings,
        workspace_roots: &[PathBuf],
    ) -> Result<Self, ProjectError> {
        let workspace_root = containing_workspace(document_path, workspace_roots)
            .or_else(|| document_path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));

        let manifest_path = locate_manifest(document_path, &settings.simplex, &workspace_root)?;

        let (project_root, root_config) = if let Some(manifest_path) = &manifest_path {
            let project_root = canonicalize(
                manifest_path
                    .parent()
                    .expect("a manifest path always has a parent"),
            )?;
            (project_root, Some(load_manifest(manifest_path)?))
        } else {
            (canonicalize_existing_ancestor(&workspace_root)?, None)
        };

        let source_root = if !settings.source_directory.trim().is_empty() {
            resolve_existing(&project_root, &settings.source_directory)?
        } else if let Some(config) = &root_config {
            canonicalize(&project_root.join(&config.build.src_dir))?
        } else {
            canonicalize_existing_ancestor(
                document_path.parent().unwrap_or(project_root.as_path()),
            )?
        };

        let mut collector = ProjectCollector::new(project_root.clone());
        collector.package_roots.insert(source_root.clone());
        if let Some(config) = &root_config {
            collector.collect(config, &project_root, &source_root)?;
        }

        for (alias, dependency) in &settings.dependencies {
            collector.add_manual(&project_root, &source_root, alias, dependency)?;
        }

        let mut dependencies = collector
            .mappings
            .into_iter()
            .map(|((context, alias), target)| DependencyMapping {
                context,
                alias,
                target,
            })
            .collect::<Vec<_>>();
        dependencies.sort_by(|a, b| {
            b.context
                .as_os_str()
                .len()
                .cmp(&a.context.as_os_str().len())
                .then_with(|| a.alias.cmp(&b.alias))
        });

        let mut package_roots = collector.package_roots.into_iter().collect::<Vec<_>>();
        package_roots.sort_by_key(|path| std::cmp::Reverse(path.as_os_str().len()));

        Ok(Self {
            source_root,
            dependencies,
            package_roots,
        })
    }
}

fn resolve_existing(base: &Path, path: &str) -> Result<PathBuf, ProjectError> {
    let path = PathBuf::from(path);
    let resolved = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    canonicalize(&resolved)
}

fn canonicalize(path: &Path) -> Result<PathBuf, ProjectError> {
    fs::canonicalize(path).map_err(|source| ProjectError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })
}

fn canonicalize_existing_ancestor(path: &Path) -> Result<PathBuf, ProjectError> {
    let mut current = path;
    loop {
        match canonicalize(current) {
            Ok(path) => return Ok(path),
            Err(_error) if current.parent().is_some() => {
                current = current.parent().expect("checked above");
            }
            Err(error) => return Err(error),
        }
    }
}

fn to_canon(path: &Path) -> Result<CanonPath, ProjectError> {
    CanonPath::canonicalize(path).map_err(ProjectError::Compiler)
}

fn containing_workspace(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.as_os_str().len())
        .cloned()
}
