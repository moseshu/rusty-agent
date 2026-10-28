// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Individual product configuration layers and their provenance.
#![allow(
    missing_docs,
    reason = "Preserve the source layer data and constructor surface shared by product consumers."
)]
use crate::{ConfigLayer, ConfigLayerMetadata, ConfigLayerSource, version_for_toml};
use ra_paths::AbsolutePathBuf;
use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigLayerEntry {
    pub name: ConfigLayerSource,
    pub config: TomlValue,
    pub version: String,
    pub disabled_reason: Option<String>,
    raw_toml: Option<RawTomlLayer>,
    hooks_config_folder_override: Option<AbsolutePathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
struct RawTomlLayer {
    contents: String,
    base_dir: AbsolutePathBuf,
}

impl ConfigLayerEntry {
    #[must_use]
    pub fn new(name: ConfigLayerSource, config: TomlValue) -> Self {
        let version = version_for_toml(&config);
        Self {
            name,
            config,
            version,
            disabled_reason: None,
            raw_toml: None,
            hooks_config_folder_override: None,
        }
    }

    #[must_use]
    pub fn new_with_raw_toml(
        name: ConfigLayerSource,
        config: TomlValue,
        raw_toml: String,
        raw_toml_base_dir: AbsolutePathBuf,
    ) -> Self {
        let version = version_for_toml(&config);
        Self {
            name,
            config,
            version,
            disabled_reason: None,
            raw_toml: Some(RawTomlLayer {
                contents: raw_toml,
                base_dir: raw_toml_base_dir,
            }),
            hooks_config_folder_override: None,
        }
    }

    pub fn new_disabled(
        name: ConfigLayerSource,
        config: TomlValue,
        disabled_reason: impl Into<String>,
    ) -> Self {
        let version = version_for_toml(&config);
        Self {
            name,
            config,
            version,
            disabled_reason: Some(disabled_reason.into()),
            raw_toml: None,
            hooks_config_folder_override: None,
        }
    }

    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.disabled_reason.is_some()
    }

    #[must_use]
    pub fn raw_toml(&self) -> Option<&str> {
        self.raw_toml
            .as_ref()
            .map(|raw_toml| raw_toml.contents.as_str())
    }

    #[must_use]
    pub fn raw_toml_base_dir(&self) -> Option<&AbsolutePathBuf> {
        self.raw_toml.as_ref().map(|raw_toml| &raw_toml.base_dir)
    }

    pub(crate) fn with_hooks_config_folder_override(
        mut self,
        hooks_config_folder_override: Option<AbsolutePathBuf>,
    ) -> Self {
        self.hooks_config_folder_override = hooks_config_folder_override;
        self
    }

    #[must_use]
    pub fn metadata(&self) -> ConfigLayerMetadata {
        ConfigLayerMetadata {
            name: self.name.clone(),
            version: self.version.clone(),
        }
    }

    #[must_use]
    pub fn as_layer(&self) -> ConfigLayer {
        ConfigLayer {
            name: self.name.clone(),
            version: self.version.clone(),
            config: serde_json::to_value(&self.config).unwrap_or(JsonValue::Null),
            disabled_reason: self.disabled_reason.clone(),
        }
    }

    /// Returns the product configuration folder associated with this config layer, if any.
    #[must_use]
    #[allow(
        clippy::match_same_arms,
        reason = "Keep source branches explicit for configuration parity review."
    )]
    pub fn config_folder(&self) -> Option<AbsolutePathBuf> {
        match &self.name {
            ConfigLayerSource::PackagedDefaults { .. } => None,
            ConfigLayerSource::Mdm { .. } => None,
            ConfigLayerSource::System { file } => file.parent(),
            ConfigLayerSource::EnterpriseManaged { .. } => None,
            ConfigLayerSource::User { file, .. } => file.parent(),
            ConfigLayerSource::Project { dot_codex_folder } => Some(dot_codex_folder.clone()),
            ConfigLayerSource::SessionFlags => None,
            ConfigLayerSource::LegacyManagedConfigTomlFromFile { .. } => None,
            ConfigLayerSource::LegacyManagedConfigTomlFromMdm => None,
        }
    }

    /// Returns the product configuration folder that should be used for hook declarations.
    ///
    /// Project layers normally use their own config folder. Linked Git worktrees
    /// can instead point hook discovery at the matching folder from the root
    /// checkout while the rest of the project config still comes from the
    /// worktree.
    #[must_use]
    pub fn hooks_config_folder(&self) -> Option<AbsolutePathBuf> {
        self.hooks_config_folder_override
            .clone()
            .or_else(|| self.config_folder())
    }
}
