use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use simplicityhl::resolution::{DependencyMap, DependencyMapBuilder};
use simplicityhl::source::CanonPath;
use thiserror::Error;

use crate::config::{ManualDependency, ProjectSettings};

pub const SIMPLEX_MANIFEST: &str = "Simplex.toml";
const SIMPLEX_MANIFEST_LOWERCASE: &str = "simplex.toml";
const DEFAULT_SOURCE_DIRECTORY: &str = "simf";
const DEFAULT_DEPENDENCY_DIRECTORY: &str = "deps";

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

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("Unable to read `{path}`: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Unable to parse `{path}`: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("Unable to resolve `{path}`: {source}")]
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Configured Simplex manifest was not found at `{0}`")]
    MissingConfiguredManifest(PathBuf),
    #[error("Dependency `{name}` in `{manifest}` must set exactly one of `path` or `git`")]
    InvalidDependency { name: String, manifest: PathBuf },
    #[error("Git dependency `{name}` from `{url}` is not installed at `{expected}`")]
    MissingGitDependency {
        name: String,
        url: String,
        expected: PathBuf,
    },
    #[error("Dependency `{name}` is missing a Simplex manifest in `{root}`")]
    MissingDependencyManifest { name: String, root: PathBuf },
    #[error("Invalid git dependency URL `{0}`")]
    InvalidGitUrl(String),
    #[error("Unable to build compiler dependency mappings: {0}")]
    Compiler(String),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct SimplexConfig {
    build: BuildConfig,
    dependencies: BTreeMap<String, DependencyConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct BuildConfig {
    src_dir: String,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            src_dir: DEFAULT_SOURCE_DIRECTORY.to_string(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct DependencyConfig {
    path: Option<String>,
    git: Option<String>,
}

struct ProjectCollector {
    install_root: PathBuf,
    visited: HashSet<PathBuf>,
    mappings: BTreeMap<(PathBuf, String), PathBuf>,
    package_roots: HashSet<PathBuf>,
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

        let manifest_path = if settings.simplex.enabled {
            if settings.simplex.manifest_path.trim().is_empty() {
                find_manifest(document_path)
            } else {
                let configured = PathBuf::from(settings.simplex.manifest_path.trim());
                let configured = if configured.is_absolute() {
                    configured
                } else {
                    workspace_root.join(configured)
                };
                Some(
                    manifest_at(&configured)
                        .ok_or(ProjectError::MissingConfiguredManifest(configured))?,
                )
            }
        } else {
            None
        };

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
            add_manual_dependency(
                &mut collector,
                &project_root,
                &source_root,
                alias,
                dependency,
            )?;
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

    pub fn dependency_map(&self, document_path: &Path) -> Result<DependencyMap, ProjectError> {
        let mut builder = DependencyMapBuilder::new();
        for mapping in &self.dependencies {
            builder.add_dependency(
                to_canon(&mapping.context)?,
                mapping.alias.clone(),
                to_canon(&mapping.target)?,
            );
        }

        let package_root = self.package_root_for(document_path);
        builder
            .build(to_canon(package_root)?)
            .map_err(|error| ProjectError::Compiler(error.to_string()))
    }

    pub fn package_root_for(&self, document_path: &Path) -> &Path {
        let canonical_document =
            fs::canonicalize(document_path).unwrap_or_else(|_| document_path.to_path_buf());
        self.package_roots
            .iter()
            .find(|root| canonical_document.starts_with(root))
            .map_or(self.source_root.as_path(), PathBuf::as_path)
    }

    fn visible_mappings<'a>(
        &'a self,
        document_path: &Path,
    ) -> impl Iterator<Item = &'a DependencyMapping> + 'a {
        let canonical_document =
            fs::canonicalize(document_path).unwrap_or_else(|_| document_path.to_path_buf());
        self.dependencies
            .iter()
            .filter(move |mapping| canonical_document.starts_with(&mapping.context))
    }

    /// Resolve the directory an import alias points at, from the perspective of
    /// `document_path`.
    pub fn import_root(&self, document_path: &Path, alias: &str) -> Option<&Path> {
        if alias == "crate" {
            return Some(self.package_root_for(document_path));
        }

        self.visible_mappings(document_path)
            .find(|mapping| mapping.alias == alias)
            .map(|mapping| mapping.target.as_path())
    }

    /// Return dependency aliases that are visible from `document_path`.
    ///
    /// A package can provide both a broad mapping and a more specific override for the
    /// same alias. Completion only needs to display that logical alias once; [`Self::import_root`]
    /// selects the effective target when the user continues the path.
    pub fn dependency_aliases(&self, document_path: &Path) -> Vec<&str> {
        let mut aliases = self
            .visible_mappings(document_path)
            .map(|mapping| mapping.alias.as_str())
            .collect::<Vec<_>>();
        aliases.sort_unstable();
        aliases.dedup();
        aliases
    }
}

impl ProjectCollector {
    fn new(install_root: PathBuf) -> Self {
        Self {
            install_root,
            visited: HashSet::new(),
            mappings: BTreeMap::new(),
            package_roots: HashSet::new(),
        }
    }

    fn collect(
        &mut self,
        config: &SimplexConfig,
        package_root: &Path,
        source_root: &Path,
    ) -> Result<(), ProjectError> {
        self.visited.insert(package_root.to_path_buf());

        for (name, dependency) in &config.dependencies {
            let dependency_root = self.resolve_dependency(name, dependency, package_root)?;
            let manifest_path = manifest_in(&dependency_root).ok_or_else(|| {
                ProjectError::MissingDependencyManifest {
                    name: name.clone(),
                    root: dependency_root.clone(),
                }
            })?;
            let dependency_config = load_manifest(&manifest_path)?;
            let dependency_source =
                canonicalize(&dependency_root.join(&dependency_config.build.src_dir))?;

            self.mappings.insert(
                (source_root.to_path_buf(), name.clone()),
                dependency_source.clone(),
            );
            self.package_roots.insert(dependency_source.clone());

            if self.visited.insert(dependency_root.clone()) {
                self.collect(&dependency_config, &dependency_root, &dependency_source)?;
            }
        }

        Ok(())
    }

    fn resolve_dependency(
        &self,
        name: &str,
        dependency: &DependencyConfig,
        package_root: &Path,
    ) -> Result<PathBuf, ProjectError> {
        match (&dependency.path, &dependency.git) {
            (Some(path), None) => canonicalize(&package_root.join(path)),
            (None, Some(url)) => {
                let relative = hashed_repository_path(url)?;
                let expected = self
                    .install_root
                    .join(DEFAULT_DEPENDENCY_DIRECTORY)
                    .join(relative);
                canonicalize(&expected).map_err(|_| ProjectError::MissingGitDependency {
                    name: name.to_string(),
                    url: url.clone(),
                    expected,
                })
            }
            (Some(_), Some(_)) | (None, None) => Err(ProjectError::InvalidDependency {
                name: name.to_string(),
                manifest: package_root.join(SIMPLEX_MANIFEST),
            }),
        }
    }
}

fn add_manual_dependency(
    collector: &mut ProjectCollector,
    project_root: &Path,
    source_root: &Path,
    alias: &str,
    dependency: &ManualDependency,
) -> Result<(), ProjectError> {
    let target = resolve_existing(project_root, dependency.path())?;
    let context = dependency.context().map_or_else(
        || Ok(source_root.to_path_buf()),
        |path| resolve_existing(project_root, path),
    )?;
    collector
        .mappings
        .insert((context, alias.to_string()), target.clone());
    collector.package_roots.insert(target);
    Ok(())
}

fn load_manifest(path: &Path) -> Result<SimplexConfig, ProjectError> {
    let source = read_to_string(path)?;
    toml::from_str(&source).map_err(|source| ProjectError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn read_to_string(path: &Path) -> Result<String, ProjectError> {
    fs::read_to_string(path).map_err(|source| ProjectError::Read {
        path: path.to_path_buf(),
        source,
    })
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

fn manifest_at(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    manifest_in(path)
}

fn manifest_in(directory: &Path) -> Option<PathBuf> {
    [SIMPLEX_MANIFEST, SIMPLEX_MANIFEST_LOWERCASE]
        .into_iter()
        .map(|name| directory.join(name))
        .find(|path| path.is_file())
}

pub fn find_manifest(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    start.ancestors().find_map(manifest_in)
}

fn hashed_repository_path(url: &str) -> Result<PathBuf, ProjectError> {
    let clean_url = url.strip_suffix(".git").unwrap_or(url);
    let repository_name = clean_url
        .split('/')
        .next_back()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ProjectError::InvalidGitUrl(url.to_string()))?;

    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    Ok(PathBuf::from(format!(
        "{repository_name}-{:016x}",
        hasher.finish()
    )))
}

#[cfg(test)]
mod tests;
