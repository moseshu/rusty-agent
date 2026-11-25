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
use ra_paths::PathUri;
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

/// The answer to `configRequirements/read`: Codex's `ConfigRequirementsReadResponse`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigRequirementsReadResponse {
    /// Whether Fast and Ultra Fast requirements are enforced independently.
    /// Older servers omit this field and use Fast mode as a shared speed gate.
    #[serde(default)]
    pub supports_independent_speed_modes: Option<bool>,
    /// Null if no requirements are configured (e.g. no requirements.toml/MDM entries).
    pub requirements: Option<ConfigRequirements>,
}

/// The managed requirements a product applies: Codex's `ConfigRequirements` without its account
/// login, credential store and `ChatGPT` backend fields, which this product does not have.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigRequirements {
    /// Exact provider selection required by managed policy.
    pub model_provider: Option<String>,
    /// Complete required provider definitions, using config.toml field names.
    pub model_providers: Option<HashMap<String, JsonValue>>,
    pub additional_developer_instructions: Option<String>,
    pub allowed_approval_policies: Option<Vec<AskForApproval>>,
    pub allowed_approvals_reviewers: Option<Vec<ApprovalsReviewer>>,
    pub allowed_sandbox_modes: Option<Vec<SandboxMode>>,
    pub allowed_windows_sandbox_implementations: Option<Vec<WindowsSandboxImplementation>>,
    pub allowed_permission_profiles: Option<BTreeMap<String, bool>>,
    pub default_permissions: Option<String>,
    pub allowed_web_search_modes: Option<Vec<WebSearchMode>>,
    pub allow_managed_hooks_only: Option<bool>,
    pub allow_browser_and_computer_use: Option<bool>,
    pub allow_appshots: Option<bool>,
    pub allow_remote_control: Option<bool>,
    pub computer_use: Option<ComputerUseRequirements>,
    pub browser_use: Option<BrowserUseRequirements>,
    pub in_app_browser: Option<InAppBrowserRequirements>,
    pub feature_requirements: Option<BTreeMap<String, bool>>,
    pub hooks: Option<ManagedHooksRequirements>,
    pub enforce_residency: Option<ResidencyRequirement>,
    pub network: Option<NetworkRequirements>,
    pub application: Option<ApplicationRequirements>,
    pub auto_review: Option<AutoReviewRequirements>,
    pub models: Option<ModelsRequirements>,
    #[schemars(with = "Option<String>")]
    pub sqlite_home: Option<PathUri>,
    #[schemars(with = "Option<String>")]
    pub log_dir: Option<PathUri>,
    #[schemars(with = "Option<String>")]
    pub model_catalog_json: Option<PathUri>,
    pub check_for_update_on_startup: Option<bool>,
    pub allow_login_shell: Option<bool>,
    pub feedback: Option<FeedbackRequirements>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WindowsSandboxImplementation {
    Elevated,
    Unelevated,
    Mxc,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutoReviewRequirements {
    pub required_on_models: Option<Vec<String>>,
    pub ignore_rules: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelsRequirements {
    pub new_thread: Option<NewThreadModelDefaults>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewThreadModelDefaults {
    pub model: Option<String>,
    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub service_tier: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackRequirements {
    pub enabled: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerUseRequirements {
    pub allow_locked_computer_use: Option<bool>,
    pub allow_persistent_approval: Option<bool>,
    pub default_app_access: Option<AllowDenyRequirement>,
    pub macos: Option<ComputerUseMacosRequirements>,
    pub windows: Option<ComputerUseWindowsRequirements>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerUseMacosRequirements {
    pub bundle_ids: Option<BTreeMap<String, AllowDenyRequirement>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerUseWindowsRequirements {
    pub aumids: Option<BTreeMap<String, AllowDenyRequirement>>,
    pub exes: Option<Vec<ComputerUseWindowsExeRequirement>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerUseWindowsExeRequirement {
    pub publisher_name: String,
    pub product_name: String,
    pub binary_name: Option<String>,
    pub access: AllowDenyRequirement,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserUseRequirements {
    pub extension: Option<BrowserUseExtensionRequirements>,
    pub allow_webmcp: Option<bool>,
    pub allow_history_access: Option<bool>,
    pub disable_auto_review: Option<bool>,
    pub allow_global_persistent_approval: Option<bool>,
    pub default_origin_policy: Option<BrowserUseOriginPolicy>,
    pub origins: Option<BTreeMap<String, BrowserUseOriginPolicy>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserUseExtensionRequirements {
    pub request_headers: Option<Vec<RequestHeader>>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestHeader {
    pub name: String,
    pub value: String,
}

impl std::fmt::Debug for RequestHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestHeader")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserUseOriginPolicy {
    pub access: Option<AllowDenyRequirement>,
    pub downloads: Option<AllowDenyRequirement>,
    pub uploads: Option<AllowDenyRequirement>,
    pub full_cdp_access: Option<AllowDenyRequirement>,
    pub auto_review: Option<AllowDenyRequirement>,
    pub persistent_approval: Option<bool>,
    pub access_approval_lifetime: Option<BrowserUseAccessApprovalLifetime>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BrowserUseAccessApprovalLifetime {
    Turn,
    Thread,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InAppBrowserRequirements {
    pub allow_external_browser_settings_import: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ManagedHooksRequirements {
    pub managed_dir: Option<PathBuf>,
    pub windows_managed_dir: Option<PathBuf>,
    #[serde(rename = "PreToolUse")]
    pub pre_tool_use: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "PermissionRequest")]
    pub permission_request: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "PostToolUse")]
    pub post_tool_use: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "PreCompact")]
    pub pre_compact: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "PostCompact")]
    pub post_compact: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "SessionStart")]
    pub session_start: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "SessionEnd", default)]
    pub session_end: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "UserPromptSubmit")]
    pub user_prompt_submit: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "SubagentStart")]
    pub subagent_start: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "SubagentStop")]
    pub subagent_stop: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "Stop")]
    pub stop: Vec<ConfiguredHookMatcherGroup>,
    #[serde(rename = "Interrupt", default)]
    pub interrupt: Vec<ConfiguredHookMatcherGroup>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfiguredHookMatcherGroup {
    pub matcher: Option<String>,
    pub hooks: Vec<ConfiguredHookHandler>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type")]
pub enum ConfiguredHookHandler {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(rename = "commandWindows")]
        command_windows: Option<String>,
        #[serde(rename = "timeoutSec")]
        timeout_sec: Option<u64>,
        r#async: bool,
        #[serde(rename = "statusMessage")]
        status_message: Option<String>,
        /// Approximate token threshold for spilling this hook's `additionalContext` to disk.
        /// `null` uses 2,500 tokens; `0` disables spilling for this hook. The threshold is
        /// evaluated against the original context; a spilled preview also includes recovery
        /// metadata.
        #[serde(rename = "additionalContextLimit")]
        additional_context_limit: Option<usize>,
    },
    #[serde(rename = "mcp_tool")]
    McpTool {
        server: String,
        tool: String,
        input: serde_json::Map<String, JsonValue>,
        #[serde(rename = "timeoutSec")]
        timeout_sec: Option<u64>,
        #[serde(rename = "statusMessage")]
        status_message: Option<String>,
    },
    #[serde(rename = "prompt")]
    Prompt {},
    #[serde(rename = "agent")]
    Agent {},
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRequirements {
    pub enabled: Option<bool>,
    pub http_port: Option<u16>,
    pub socks_port: Option<u16>,
    pub allow_upstream_proxy: Option<bool>,
    pub dangerously_allow_non_loopback_proxy: Option<bool>,
    pub dangerously_allow_all_unix_sockets: Option<bool>,
    /// Canonical network permission map for `experimental_network`.
    pub domains: Option<BTreeMap<String, NetworkDomainPermission>>,
    /// When true, only managed allowlist entries are respected while managed
    /// network enforcement is active.
    pub managed_allowed_domains_only: Option<bool>,
    /// Legacy compatibility view derived from `domains`.
    pub allowed_domains: Option<Vec<String>>,
    /// Legacy compatibility view derived from `domains`.
    pub denied_domains: Option<Vec<String>>,
    /// Canonical unix socket permission map for `experimental_network`.
    pub unix_sockets: Option<BTreeMap<String, NetworkUnixSocketPermission>>,
    /// Legacy compatibility view derived from `unix_sockets`.
    pub allow_unix_sockets: Option<Vec<String>>,
    pub allow_local_binding: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NetworkDomainPermission {
    Allow,
    Deny,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NetworkUnixSocketPermission {
    Allow,
    Deny,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ResidencyRequirement {
    Us,
}

/// Managed requirements for application traffic, separate from agent permissions.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationRequirements {
    pub network: Option<ApplicationNetworkRequirements>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationNetworkRequirements {
    /// When enabled, only explicitly allowed exact domains may be contacted.
    pub enabled: bool,
    pub domains: BTreeMap<String, NetworkDomainPermission>,
}
