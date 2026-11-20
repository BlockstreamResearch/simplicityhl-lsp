use std::path::PathBuf;

use smplx_build::error::BuildError;
use smplx_build::CONFIG_FILENAME;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("Unable to read `{path}`: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Invalid Simplex manifest `{path}`: {source}")]
    Manifest {
        path: PathBuf,
        source: Box<BuildError>,
    },
    #[error("Unable to resolve `{path}`: {source}")]
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Configured Simplex manifest was not found at `{0}`")]
    MissingConfiguredManifest(PathBuf),
    #[error("Unable to resolve dependency `{name}`: {source}")]
    Dependency {
        name: String,
        source: Box<BuildError>,
    },
    #[error("Dependency `{name}` has no `{expected}` in `{root}`", expected = CONFIG_FILENAME)]
    MissingDependencyManifest { name: String, root: PathBuf },
    #[error("Dependency `{name}` src_dir resolves outside its package: `{source_dir}` (package root: `{root}`)")]
    DependencySourceOutsidePackage {
        name: String,
        root: PathBuf,
        source_dir: PathBuf,
    },
    #[error("Unable to build compiler dependency mappings: {0}")]
    Compiler(String),
}
