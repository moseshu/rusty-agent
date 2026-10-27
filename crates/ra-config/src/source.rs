// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product configuration source identity, precedence and materialized metadata.

use ra_paths::AbsolutePathBuf;
use serde_json::Value as JsonValue;

/// Provenance for one layer in the effective product configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigLayerSource {
    /// Default configuration supplied with the installed product package.
    PackagedDefaults {
        /// File identifying the packaged defaults.
        file: AbsolutePathBuf,
    },
    /// Managed preferences delivered by MDM.
    Mdm {
        /// Managed preferences domain.
        domain: String,
        /// Managed preferences key.
        key: String,
    },
    /// Host-wide configuration loaded from a file.
    System {
        /// Host-wide configuration file.
        file: AbsolutePathBuf,
    },
    /// Configuration delivered by an enterprise cloud bundle.
    EnterpriseManaged {
        /// Fragment identity supplied by the managed source.
        id: String,
        /// Human-readable managed fragment name.
        name: String,
    },
    /// User configuration, optionally augmented by a selected profile.
    User {
        /// Base or selected profile configuration file.
        file: AbsolutePathBuf,
        /// Selected profile name, absent for the base user file.
        profile: Option<String>,
    },
    /// Configuration loaded from a product project configuration directory.
    Project {
        /// Project configuration directory; the upstream field name is retained.
        dot_codex_folder: AbsolutePathBuf,
    },
    /// Overrides supplied for the current session.
    SessionFlags,
    /// Legacy managed configuration loaded from a file.
    LegacyManagedConfigTomlFromFile {
        /// Legacy managed configuration file.
        file: AbsolutePathBuf,
    },
    /// Legacy managed configuration delivered by MDM.
    LegacyManagedConfigTomlFromMdm,
}

impl ConfigLayerSource {
    /// A setting from a layer with a higher precedence overrides a setting
    /// from a layer with a lower precedence.
    #[must_use]
    pub fn precedence(&self) -> i16 {
        match self {
            ConfigLayerSource::PackagedDefaults { .. } => -10,
            ConfigLayerSource::Mdm { .. } => 0,
            ConfigLayerSource::System { .. } => 10,
            ConfigLayerSource::EnterpriseManaged { .. } => 15,
            ConfigLayerSource::User { profile, .. } => {
                if profile.is_some() {
                    21
                } else {
                    20
                }
            }
            ConfigLayerSource::Project { .. } => 25,
            ConfigLayerSource::SessionFlags => 30,
            ConfigLayerSource::LegacyManagedConfigTomlFromFile { .. } => 40,
            ConfigLayerSource::LegacyManagedConfigTomlFromMdm => 50,
        }
    }
}

/// Compares [`ConfigLayerSource`] by precedence, so `A < B` means settings
/// from layer `A` will be overridden by settings from layer `B`.
impl PartialOrd for ConfigLayerSource {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.precedence().cmp(&other.precedence()))
    }
}

/// Identity and version information for a configuration layer.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigLayerMetadata {
    /// Full source identity and scope.
    pub name: ConfigLayerSource,
    /// Stable configuration version.
    pub version: String,
}

/// A materialized configuration layer and its provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigLayer {
    /// Full source identity and scope.
    pub name: ConfigLayerSource,
    /// Stable configuration version.
    pub version: String,
    /// Materialized configuration document.
    pub config: JsonValue,
    /// Why this layer is excluded from effective values, if disabled.
    pub disabled_reason: Option<String>,
}

/// Formats the original source identity for product status output.
#[must_use]
pub fn format_config_layer_source(source: &ConfigLayerSource, config_toml_file: &str) -> String {
    match source {
        ConfigLayerSource::PackagedDefaults { file } => {
            format!("packaged defaults ({})", file.as_path().display())
        }
        ConfigLayerSource::Mdm { domain, key } => {
            format!("MDM ({domain}:{key})")
        }
        ConfigLayerSource::System { file } => {
            format!("system ({})", file.as_path().display())
        }
        ConfigLayerSource::EnterpriseManaged { id, name } => {
            format!("enterprise-managed ({name}, {id})")
        }
        ConfigLayerSource::User { file, .. } => {
            format!("user ({})", file.as_path().display())
        }
        ConfigLayerSource::Project { dot_codex_folder } => {
            format!(
                "project ({}/{config_toml_file})",
                dot_codex_folder.as_path().display()
            )
        }
        ConfigLayerSource::SessionFlags => "session-flags".to_string(),
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => {
            format!("legacy managed_config.toml ({})", file.as_path().display())
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            "legacy managed_config.toml (MDM)".to_string()
        }
    }
}
