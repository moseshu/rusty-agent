// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product configuration read and user-layer write records.
//!
//! The typed `Config` fields cover the source fields whose values exist in the product schema.
//! Account login restrictions and account app-connector settings are not part of this product;
//! any other effective key is still returned through `additional`.
#![allow(
    missing_docs,
    reason = "Retain source configuration wire field names and serialization."
)]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;

use ra_config::ApprovalsReviewer;
use ra_config::AskForApproval;
use ra_config::AutoCompactTokenLimitScope;
use ra_config::ReasoningEffort;
use ra_config::ReasoningSummary;
use ra_config::SandboxMode;
use ra_config::Verbosity;
use ra_config::WebSearchMode;
use ra_config::WebSearchToolConfig;
use ra_paths::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ConfigLayerSource {
    /// Default configuration supplied with the installed product package.
    #[serde(rename_all = "camelCase")]
    PackagedDefaults {
        /// Path to the packaged default configuration file.
        file: AbsolutePathBuf,
    },

    /// Managed preferences layer delivered by MDM (macOS only).
    #[serde(rename_all = "camelCase")]
    Mdm {
        domain: String,
        key: String,
    },

    /// Managed config layer from a file (usually `managed_config.toml`).
    #[serde(rename_all = "camelCase")]
    System {
        /// This is the path to the system config.toml file, though it is not
        /// guaranteed to exist.
        file: AbsolutePathBuf,
    },

    /// Enterprise-managed config layer delivered by the cloud config bundle.
    #[serde(rename_all = "camelCase")]
    EnterpriseManaged {
        /// Stable identifier for the delivered layer.
        id: String,

        /// Admin-facing name for the delivered layer. This is surfaced in
        /// diagnostics so users know which cloud layer needs administrator
        /// attention.
        name: String,
    },

    /// User config layer from the product home `config.toml`. This layer is
    /// special in that it is expected to be:
    /// - writable by the user
    /// - generally outside the workspace directory
    #[serde(rename_all = "camelCase")]
    User {
        /// This is the path to the user's config.toml file, though it is not
        /// guaranteed to exist.
        file: AbsolutePathBuf,

        /// Name of the selected profile-v2 config layered on top of the base
        /// user config, when this layer represents one.
        profile: Option<String>,
    },

    /// Path to a project configuration folder. There could be multiple of
    /// these between `cwd` and the project/repo root. The source wire field
    /// name is retained.
    #[serde(rename_all = "camelCase")]
    Project {
        dot_codex_folder: AbsolutePathBuf,
    },

    /// Session-layer overrides supplied via `-c`/`--config`.
    SessionFlags,

    /// `managed_config.toml` was designed to be a config that was loaded
    /// as the last layer on top of everything else. This scheme did not quite
    /// work out as intended, but we keep this variant as a "best effort" while
    /// we phase out `managed_config.toml` in favor of `requirements.toml`.
    #[serde(rename_all = "camelCase")]
    LegacyManagedConfigTomlFromFile {
        file: AbsolutePathBuf,
    },

    LegacyManagedConfigTomlFromMdm,
}

impl ConfigLayerSource {
    /// A settings from a layer with a higher precedence will override a setting
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

/// Compares [`ConfigLayerSource`] by precedence, so `A < B` means settings from
/// layer `A` will be overridden by settings from layer `B`.
impl PartialOrd for ConfigLayerSource {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.precedence().cmp(&other.precedence()))
    }
}

impl From<ra_config::ConfigLayerSource> for ConfigLayerSource {
    fn from(source: ra_config::ConfigLayerSource) -> Self {
        use ra_config::ConfigLayerSource as Source;
        match source {
            Source::PackagedDefaults { file } => Self::PackagedDefaults { file },
            Source::Mdm { domain, key } => Self::Mdm { domain, key },
            Source::System { file } => Self::System { file },
            Source::EnterpriseManaged { id, name } => Self::EnterpriseManaged { id, name },
            Source::User { file, profile } => Self::User { file, profile },
            Source::Project { dot_codex_folder } => Self::Project { dot_codex_folder },
            Source::SessionFlags => Self::SessionFlags,
            Source::LegacyManagedConfigTomlFromFile { file } => {
                Self::LegacyManagedConfigTomlFromFile { file }
            }
            Source::LegacyManagedConfigTomlFromMdm => Self::LegacyManagedConfigTomlFromMdm,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SandboxWorkspaceWrite {
    #[serde(default)]
    pub writable_roots: Vec<PathBuf>,
    #[serde(default)]
    pub network_access: bool,
    #[serde(default)]
    pub exclude_tmpdir_env_var: bool,
    #[serde(default)]
    pub exclude_slash_tmp: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ToolsV2 {
    pub web_search: Option<WebSearchToolConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct AnalyticsConfig {
    pub enabled: Option<bool>,
    #[serde(default, flatten)]
    pub additional: HashMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AllowDenyRequirement {
    Allow,
    Deny,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct BrowserUseConfig {
    pub allow_history_access: Option<bool>,
    pub default_origin_policy: Option<BrowserUseOriginPolicyConfig>,
    pub origins: Option<BTreeMap<String, BrowserUseOriginPolicyConfig>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct BrowserUseOriginPolicyConfig {
    pub access: Option<AllowDenyRequirement>,
    pub downloads: Option<AllowDenyRequirement>,
    pub uploads: Option<AllowDenyRequirement>,
    pub full_cdp_access: Option<AllowDenyRequirement>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ComputerUseConfig {
    pub default_app_access: Option<AllowDenyRequirement>,
    pub macos: Option<ComputerUseMacosConfig>,
    pub windows: Option<ComputerUseWindowsConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ComputerUseMacosConfig {
    pub bundle_ids: Option<BTreeMap<String, AllowDenyRequirement>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ComputerUseWindowsConfig {
    pub aumids: Option<BTreeMap<String, AllowDenyRequirement>>,
    pub exes: Option<Vec<ComputerUseWindowsExeConfig>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ComputerUseWindowsExeConfig {
    pub publisher_name: String,
    pub product_name: String,
    pub binary_name: Option<String>,
    pub access: AllowDenyRequirement,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct Config {
    pub model: Option<String>,
    pub review_model: Option<String>,
    pub model_context_window: Option<i64>,
    pub model_auto_compact_token_limit: Option<i64>,
    pub model_auto_compact_token_limit_scope: Option<AutoCompactTokenLimitScope>,
    pub model_provider: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    /// \[UNSTABLE\] Optional default for where approval requests are routed for
    /// review.
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox_mode: Option<SandboxMode>,
    pub sandbox_workspace_write: Option<SandboxWorkspaceWrite>,
    pub web_search: Option<WebSearchMode>,
    pub tools: Option<ToolsV2>,
    pub instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub compact_prompt: Option<String>,
    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub model_reasoning_summary: Option<ReasoningSummary>,
    pub model_verbosity: Option<Verbosity>,
    pub service_tier: Option<String>,
    pub analytics: Option<AnalyticsConfig>,
    pub browser_use: Option<BrowserUseConfig>,
    pub computer_use: Option<ComputerUseConfig>,
    pub desktop: Option<HashMap<String, JsonValue>>,
    #[serde(default, flatten)]
    pub additional: HashMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLayerMetadata {
    pub name: ConfigLayerSource,
    pub version: String,
}

impl From<ra_config::ConfigLayerMetadata> for ConfigLayerMetadata {
    fn from(metadata: ra_config::ConfigLayerMetadata) -> Self {
        Self {
            name: metadata.name.into(),
            version: metadata.version,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLayer {
    pub name: ConfigLayerSource,
    pub version: String,
    pub config: JsonValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

impl From<ra_config::ConfigLayer> for ConfigLayer {
    fn from(layer: ra_config::ConfigLayer) -> Self {
        Self {
            name: layer.name.into(),
            version: layer.version,
            config: layer.config,
            disabled_reason: layer.disabled_reason,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum MergeStrategy {
    Replace,
    Upsert,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WriteStatus {
    Ok,
    OkOverridden,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OverriddenMetadata {
    pub message: String,
    pub overriding_layer: ConfigLayerMetadata,
    pub effective_value: JsonValue,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWriteResponse {
    pub status: WriteStatus,
    pub version: String,
    /// Canonical path to the config file that was written.
    pub file_path: AbsolutePathBuf,
    pub overridden_metadata: Option<OverriddenMetadata>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ConfigWriteErrorCode {
    ConfigLayerReadonly,
    ConfigRequirementReadonly,
    ConfigVersionConflict,
    ConfigValidationError,
    ConfigPathNotFound,
    ConfigSchemaUnknownKey,
    UserLayerNotFound,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReadParams {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_layers: bool,
    /// Optional working directory to resolve project config layers. If specified,
    /// return the effective config as seen from that directory (i.e., including any
    /// project layers between `cwd` and the project/repo root).
    pub cwd: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReadResponse {
    pub config: Config,
    pub origins: HashMap<String, ConfigLayerMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<ConfigLayer>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigValueWriteParams {
    pub key_path: String,
    pub value: JsonValue,
    pub merge_strategy: MergeStrategy,
    /// Path to the config file to write; defaults to the user's `config.toml` when omitted.
    pub file_path: Option<String>,
    pub expected_version: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigBatchWriteParams {
    pub edits: Vec<ConfigEdit>,
    /// Path to the config file to write; defaults to the user's `config.toml` when omitted.
    pub file_path: Option<String>,
    pub expected_version: Option<String>,
    /// When true, hot-reload updated runtime settings into loaded threads after writing.
    /// Session-static model, reasoning-effort, Plan-mode reasoning-effort, and service-tier
    /// defaults are not reloaded. The deprecated personality setting is also not reloaded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reload_user_config: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigEdit {
    pub key_path: String,
    pub value: JsonValue,
    pub merge_strategy: MergeStrategy,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillsConfigWriteParams {
    /// Path-based selector.
    pub path: Option<AbsolutePathBuf>,
    /// Name-based selector.
    pub name: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillsConfigWriteResponse {
    pub effective_enabled: bool,
}
