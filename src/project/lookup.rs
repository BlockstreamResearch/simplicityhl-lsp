use std::fs;
use std::path::{Path, PathBuf};

use simplicityhl::resolution::{DependencyMap, DependencyMapBuilder};

use super::{to_canon, DependencyMapping, ProjectContext, ProjectError};

impl ProjectContext {
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
