use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use simplicityhl::{UnstableFeature, UnstableFeatures};

/// Runtime configuration shared by every editor client.
///
/// VS Code sends this object below the `simplicityhl` key, while other clients
/// commonly send the section itself. [`Settings::from_json`] accepts both forms.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub experimental_features: ExperimentalFeatures,
    pub project: ProjectSettings,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ExperimentalFeatures {
    /// Enables `use`, `mod`, `pub`, aliases, and multi-file dependency resolution.
    pub imports: bool,
    // TODO: `enums` is absent: `UnstableFeature::Enums` does not exist in the
    // released compiler, so the setting would silently do nothing.
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectSettings {
    pub simplex: SimplexSettings,
    /// Optional source root used when no Simplex manifest is available.
    pub source_directory: String,
    /// Manual import-root mappings. These supplement, and on collision override,
    /// mappings discovered from `Simplex.toml`.
    pub dependencies: BTreeMap<String, ManualDependency>,
}

// TODO: maybe have a simplex as a dep to avoid code/logic duplication/invalidation
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct SimplexSettings {
    /// Automatically discover and load the nearest Simplex manifest.
    pub enabled: bool,
    /// Explicit manifest path. Relative values resolve from the containing workspace folder.
    pub manifest_path: String,
}

impl Default for SimplexSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            manifest_path: String::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ManualDependency {
    /// Shorthand: `"std": "../simplicityhl-std/simf"`.
    Path(String),
    /// Full form for context-specific compiler remappings.
    Detailed(ManualDependencyDetails),
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ManualDependencyDetails {
    pub path: String,
    pub context: String,
}

impl ManualDependency {
    pub fn path(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::Detailed(details) => &details.path,
        }
    }

    pub fn context(&self) -> Option<&str> {
        match self {
            Self::Path(_) => None,
            Self::Detailed(details) if details.context.trim().is_empty() => None,
            Self::Detailed(details) => Some(&details.context),
        }
    }
}

impl Settings {
    pub fn from_json(value: Value) -> Result<Self, serde_json::Error> {
        let value = value.get("simplicityhl").cloned().unwrap_or(value);
        serde_json::from_value(value)
    }

    pub fn unstable_features(&self) -> UnstableFeatures {
        let mut enabled = Vec::new();
        if self.experimental_features.imports {
            enabled.push(UnstableFeature::Imports);
        }
        UnstableFeatures::new(enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_experimental_syntax_disabled_and_simplex_detection_enabled() {
        let settings = Settings::default();

        assert!(!settings.experimental_features.imports);
        assert!(settings.project.simplex.enabled);
    }

    #[test]
    fn accepts_vscode_wrapped_configuration() {
        let settings = Settings::from_json(serde_json::json!({
            "simplicityhl": {
                "experimentalFeatures": { "imports": true, "enums": false },
                "project": {
                    "simplex": { "enabled": true, "manifestPath": "config/Simplex.toml" },
                    "sourceDirectory": "contracts",
                    "dependencies": {
                        "std": "../std/simf",
                        "math": { "path": "../math/simf", "context": "contracts/lib" }
                    }
                }
            }
        }))
        .expect("valid settings");

        assert!(settings.experimental_features.imports);
        assert_eq!(
            settings.project.simplex.manifest_path,
            "config/Simplex.toml"
        );
        assert_eq!(settings.project.dependencies["std"].path(), "../std/simf");
        assert_eq!(
            settings.project.dependencies["math"].context(),
            Some("contracts/lib")
        );
    }

    #[test]
    fn accepts_an_unwrapped_configuration_section() {
        // `enums` is not a field yet; an editor that still sends it must not break parsing.
        let settings = Settings::from_json(serde_json::json!({
            "experimentalFeatures": { "imports": true, "enums": true }
        }))
        .expect("valid settings");

        assert!(settings.experimental_features.imports);
    }
}
