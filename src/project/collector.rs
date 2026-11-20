use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use smplx_build::config::{Dependency, DEFAULT_DEPENDENCY_DIR};
use smplx_build::ArtifactsResolver;

use crate::config::ManualDependency;

use super::manifest::{load_manifest, manifest_in, SimplexConfig};
use super::{canonicalize, resolve_existing, to_canon, ProjectError};

pub(super) struct ProjectCollector {
    install_root: PathBuf,
    visited: HashSet<PathBuf>,
    pub(super) mappings: BTreeMap<(PathBuf, String), PathBuf>,
    pub(super) package_roots: HashSet<PathBuf>,
}

impl ProjectCollector {
    pub(super) fn new(install_root: PathBuf) -> Self {
        Self {
            install_root,
            visited: HashSet::new(),
            mappings: BTreeMap::new(),
            package_roots: HashSet::new(),
        }
    }

    pub(super) fn collect(
        &mut self,
        config: &SimplexConfig,
        package_root: &Path,
        source_root: &Path,
    ) -> Result<(), ProjectError> {
        self.visited.insert(package_root.to_path_buf());

        // `DependencyConfig` is a HashMap; sort so diagnostics don't depend on iteration order.
        let mut dependencies = config.dependencies.inner.iter().collect::<Vec<_>>();
        dependencies.sort_unstable_by_key(|(name, _)| *name);

        for (name, dependency) in dependencies {
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

            // A dependency may only define import contexts inside its own package.
            if !dependency_source.starts_with(&dependency_root) {
                return Err(ProjectError::DependencySourceOutsidePackage {
                    name: name.clone(),
                    root: dependency_root,
                    source_dir: dependency_source,
                });
            }

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

    /// Add a dependency from settings. It replaces a discovered one with the same context and alias.
    pub(super) fn add_manual(
        &mut self,
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

        self.mappings
            .insert((context, alias.to_string()), target.clone());
        self.package_roots.insert(target);

        Ok(())
    }

    fn resolve_dependency(
        &self,
        name: &str,
        dependency: &Dependency,
        package_root: &Path,
    ) -> Result<PathBuf, ProjectError> {
        let deps_dir = self.install_root.join(DEFAULT_DEPENDENCY_DIR);
        ArtifactsResolver::resolve_dep_context(dependency, &to_canon(package_root)?, &deps_dir)
            .map(|root| root.as_path().to_path_buf())
            .map_err(|source| ProjectError::Dependency {
                name: name.to_string(),
                source: Box::new(source),
            })
    }
}
