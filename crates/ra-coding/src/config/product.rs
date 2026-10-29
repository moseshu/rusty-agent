// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::struct_excessive_bools,
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::match_same_arms,
    reason = "Preserve the upstream configuration surface, branch order and diagnostic spelling."
)]
//! Effective product configuration constructed from validated source layers.
#![allow(
    missing_docs,
    clippy::too_many_lines,
    reason = "Preserve the upstream product configuration contract."
)]
use super::agent_role_config::AgentRoleConfig;
use super::catalog::ModelsResponse;
use super::network::{
    NetworkConfigInputs, NetworkProxyConfig, NetworkProxySpec, PreparedNetworkConfig,
    build_network_proxy_spec, network_proxy_config_for_profile_selection,
    profile_allows_configured_network_proxy,
};
use super::permission_profile_snapshot::{PermissionProfileSnapshot, ProfileWorkspaceRoot};
use super::providers::{
    LEGACY_OLLAMA_CHAT_PROVIDER_ID, OLLAMA_CHAT_PROVIDER_REMOVED_ERROR, built_in_model_providers,
    merge_configured_model_providers,
};
use super::resolved_permission_profile::PermissionProfileState;
use super::roles_loader::load_agent_roles;
use super::support::MultiAgentVersion;
use super::support::{
    BaseInstructionsProvenance, DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS,
    DEFAULT_OPTIONAL_MCP_STARTUP_GRACE, ExtraConfig, MIN_EMPTY_YIELD_TIME_MS, SqliteConfig,
    WindowsSandboxLevelExt, active_arg0_helper_dir, bundled_token_budget_reminder,
    compatibility_sandbox_policy_for_permission_profile, normalize_for_native_workdir,
    resolve_sqlite_home_env, resolve_windows_sandbox_mode, validate_extra_metadata,
    validate_managed_developer_instructions, windows_mxc_available,
    windows_sandbox_level_for_legacy_checks,
};
use super::windows_sandbox_config::{
    PreparedWindowsSandboxConfig, config_allows_mxc, prepare_windows_sandbox_config,
    resolve_windows_sandbox_type,
};
use super::{otel, requirements};
use ra_config::config_toml::{
    CircuitBreakAction, ConfigToml, DEFAULT_PROJECT_DOC_MAX_BYTES, ProjectConfig,
    RealtimeAudioConfig, RealtimeConfig, ThreadStoreToml, validate_model_providers,
};
use ra_config::features::{
    CodeModeConfigToml, CurrentTimeReminderConfigToml, CurrentTimeReminderDeliveryMode,
    CurrentTimeSource, Feature, FeatureConfigSource, FeatureOverrides, FeatureToml, Features,
    FeaturesToml, MultiAgentV2ConfigToml, NetworkProxyConfigToml, SleepToolMode,
    TokenBudgetConfigToml,
};
use ra_config::loader::load_config_layers_state;
use ra_config::permission_profile_catalog::{
    self, PermissionProfileCatalogEntry, permission_profile_catalog_from_permissions,
    permission_profile_is_allowed, validate_permission_profile_for_deny_read,
};
use ra_config::permission_profiles::{
    CompiledPermissionProfile, WorkspaceWriteSettings, compile_permission_profile,
    default_builtin_permission_profile_name, is_builtin_permission_profile_name,
};
use ra_config::permission_selection::{
    PermissionConfigSyntax, merge_managed_permission_profiles,
    resolve_effective_permission_selection, resolve_permission_config_syntax,
};
use ra_config::types::{
    AltScreenMode, ApprovalsReviewer, History, McpEnterpriseManagedAuthConfig, McpServerConfig,
    MemoriesConfig, ModelAvailabilityNuxConfig, Notice, OAuthCredentialsStoreMode, Personality,
    ResumeCwdMode, ServiceTier, SessionPickerViewMode, ToolSuggestConfig, ToolSuggestDisabledTool,
    ToolSuggestDiscoverable, TuiKeymap, TuiNotificationSettings, TuiPetAnchor, UriBasedFileOpener,
    WebSearchMode, WindowsSandboxModeToml,
};
use ra_config::{
    AskForApproval, AutoCompactTokenLimitScope, CloudConfigBundleLoader, ConfigLayerStack,
    ConfigLoadOptions, ConfigPathContext, ConfigRequirements, ConfigRequirementsToml, Constrained,
    ConstrainedWithSource, ConstraintError, ConstraintResult, LoaderOverrides, ManagedFeatures,
    ModelProviderInfo, PermissionProfileToml, ReasoningEffort, ReasoningSummary,
    ResidencyRequirement, SERVICE_TIER_DEFAULT_REQUEST_VALUE, SandboxMode, SandboxModeRequirement,
    ShellEnvironmentPolicy, Sourced, ThreadConfigLoader, TranscriptFormat, Verbosity,
    WebSearchConfig, WindowsSandboxLevel, find_ra_home, resolve_root_git_project_for_trust,
    sandbox_mode_requirement_for_permission_profile,
};
use ra_executor::{ExecutorFileSystem, LOCAL_FS, ReadFileOptions};
use ra_filesystem::legacy::SandboxPolicy;
use ra_filesystem::models::{
    ActivePermissionProfile, BUILT_IN_PERMISSION_PROFILE_WORKSPACE, PermissionProfile,
    SandboxEnforcement,
};
use ra_filesystem::permissions::{
    DenyReadValidator, DenyReadViolation, FileSystemPath, FileSystemSandboxPolicy,
    FileSystemSandboxPolicyContext, NetworkSandboxPolicy,
};
use ra_filesystem::sandbox::SandboxType;
use ra_paths::{AbsolutePathBuf, AbsolutePathBufGuard, PathConvention, PathUri};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::ErrorKind,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use toml::Value as TomlValue;
const DEFAULT_IGNORE_LARGE_UNTRACKED_DIRS: i64 = 200;
const DEFAULT_IGNORE_LARGE_UNTRACKED_FILES: i64 = 10 * 1024 * 1024;
const DEFAULT_ASYNC_COMPACTION_THRESHOLD_PERCENT: u8 = 65;

/// Signals that a public config selected the retired `untrusted` approval policy.
#[derive(Debug, thiserror::Error)]
#[error("approval_policy = \"untrusted\" is no longer supported; remove this setting")]
pub struct UnsupportedUntrustedApprovalPolicyError;

/// Compatibility-only config retained so legacy `ghost_snapshot` settings
/// continue to load even though snapshots are no longer produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhostSnapshotConfig {
    pub ignore_large_untracked_files: Option<i64>,
    pub ignore_large_untracked_dirs: Option<i64>,
    pub disable_warnings: bool,
}

impl Default for GhostSnapshotConfig {
    fn default() -> Self {
        Self {
            ignore_large_untracked_files: Some(DEFAULT_IGNORE_LARGE_UNTRACKED_FILES),
            ignore_large_untracked_dirs: Some(DEFAULT_IGNORE_LARGE_UNTRACKED_DIRS),
            disable_warnings: false,
        }
    }
}

/// Maximum number of bytes of the documentation that will be embedded. Larger
/// files are *silently truncated* to this size so we do not take up too much of
/// the context window.
pub(crate) const AGENTS_MD_MAX_BYTES: usize = DEFAULT_PROJECT_DOC_MAX_BYTES; // 32 KiB
pub(crate) const DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION: usize = 4;
pub(crate) const DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS: i64 = 10_000;
pub(crate) const DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS: i64 = 3600 * 1000;
pub(crate) const DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS: i64 = 30_000;
const DEFAULT_MULTI_AGENT_V2_TOOL_NAMESPACE: &str = "collaboration";

pub(crate) const HARD_MIN_MULTI_AGENT_V2_TIMEOUT_MS: i64 = 0;
pub(crate) const HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS: i64 =
    DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS;
pub(crate) const DEFAULT_AGENT_MAX_DEPTH: i32 = 1;
#[derive(Debug, Clone, PartialEq)]
pub struct Permissions {
    /// Approval policy for executing commands.
    pub approval_policy: Constrained<AskForApproval>,
    /// Constrained permission profile plus its selected profile identity, if
    /// the profile came from a built-in or named config profile.
    permission_profile_state: PermissionProfileState,
    /// Managed deny-read rules retained independently from user-defined denies.
    managed_deny_read_policy: Option<Arc<FileSystemSandboxPolicy>>,
    /// Thread-scoped runtime workspace roots. Symbolic `:workspace_roots`
    /// entries in the permission profile are materialized against these roots.
    workspace_roots: Vec<AbsolutePathBuf>,
    /// Effective network configuration applied to all spawned processes.
    pub network: Option<NetworkProxySpec>,
    /// Whether the model may request a login shell for shell-based tools.
    /// Default to `true`
    ///
    /// If `true`, the model may request a login shell (`login = true`), and
    /// omitting `login` defaults to using a login shell.
    /// If `false`, the model can never use a login shell: `login = true`
    /// requests are rejected, and omitting `login` defaults to a non-login
    /// shell.
    pub allow_login_shell: bool,
    /// Policy used to build process environments for shell/unified exec.
    pub shell_environment_policy: ShellEnvironmentPolicy,
    /// Effective Windows sandbox mode derived from `[windows].sandbox` or
    /// legacy feature keys.
    pub windows_sandbox_mode: Option<WindowsSandboxModeToml>,
    /// Configured Windows backend; use `Config::effective_local_windows_sandbox_type()` for local selection.
    pub windows_sandbox_type: SandboxType,
}

impl Permissions {
    /// Build permissions from the constrained values required for a minimal
    /// in-process configuration.
    pub fn from_approval_and_profile(
        approval_policy: Constrained<AskForApproval>,
        permission_profile: Constrained<PermissionProfile>,
    ) -> ConstraintResult<Self> {
        Ok(Self {
            approval_policy,
            permission_profile_state: PermissionProfileState::from_constrained_legacy(
                permission_profile,
            )?,
            managed_deny_read_policy: None,
            workspace_roots: Vec::new(),
            network: None,
            allow_login_shell: true,
            shell_environment_policy: ShellEnvironmentPolicy::default(),
            windows_sandbox_mode: None,
            windows_sandbox_type: SandboxType::None,
        })
    }

    #[allow(
        dead_code,
        reason = "Retain the session save, restore and child-inheritance contract."
    )]
    pub(crate) fn permission_profile_state(&self) -> &PermissionProfileState {
        &self.permission_profile_state
    }

    #[allow(
        dead_code,
        reason = "Retain the session save, restore and child-inheritance contract."
    )]
    pub(crate) fn set_permission_profile_state(
        &mut self,
        permission_profile_state: PermissionProfileState,
    ) {
        self.permission_profile_state = permission_profile_state;
    }

    /// Apply a permission profile snapshot emitted by core session state.
    ///
    /// This is a trusted-state bridge for consumers of `SessionConfigured`.
    /// Config loading and app-server selection should resolve named profiles
    /// through config instead of constructing a snapshot directly.
    pub fn set_permission_profile_from_session_snapshot(
        &mut self,
        snapshot: PermissionProfileSnapshot,
    ) -> ConstraintResult<()> {
        self.permission_profile_state
            .set_permission_profile_snapshot(snapshot)
    }

    /// Replace the current permission constraints with a trusted session
    /// snapshot. This is only for clients that must mirror core session state
    /// after their local config constraints reject the snapshot.
    pub fn replace_permission_profile_from_session_snapshot(
        &mut self,
        snapshot: PermissionProfileSnapshot,
    ) -> ConstraintResult<()> {
        let permission_profile = Constrained::allow_only(snapshot.permission_profile().clone());
        self.permission_profile_state =
            PermissionProfileState::from_constrained_snapshot(permission_profile, snapshot)?;
        Ok(())
    }

    /// Borrow the canonical profile before runtime workspace-root
    /// materialization has been applied.
    #[must_use]
    pub fn permission_profile(&self) -> &PermissionProfile {
        self.permission_profile_state.permission_profile()
    }

    pub fn can_set_permission_profile(
        &self,
        permission_profile: &PermissionProfile,
    ) -> ConstraintResult<()> {
        let permission_profile =
            self.permission_profile_preserving_managed_denied_reads(permission_profile.clone());
        self.permission_profile_state
            .can_set_legacy_permission_profile(&permission_profile)
    }

    pub fn set_workspace_roots(&mut self, workspace_roots: Vec<AbsolutePathBuf>) {
        self.workspace_roots = workspace_roots;
    }

    #[must_use]
    pub fn workspace_roots(&self) -> &[AbsolutePathBuf] {
        &self.workspace_roots
    }

    /// Workspace roots that came from user-visible configuration or runtime
    /// selection. Internal Codex-only writable roots are intentionally excluded.
    #[must_use]
    pub fn user_visible_workspace_roots(&self) -> &[AbsolutePathBuf] {
        &self.workspace_roots
    }

    #[must_use]
    pub fn profile_workspace_roots(&self) -> &[ProfileWorkspaceRoot] {
        self.permission_profile_state.profile_workspace_roots()
    }

    /// Effective runtime permissions after config requirements and runtime
    /// workspace-root materialization have been applied.
    #[must_use]
    pub fn effective_permission_profile(&self) -> PermissionProfile {
        self.permission_profile()
            .clone()
            .materialize_project_roots_with_workspace_roots(&self.workspace_roots)
    }

    /// Named profile selected by config, if the current profile has one.
    #[must_use]
    pub fn active_permission_profile(&self) -> Option<ActivePermissionProfile> {
        self.permission_profile_state.active_permission_profile()
    }

    /// Effective filesystem sandbox policy derived from the canonical profile.
    #[must_use]
    pub fn file_system_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        self.effective_permission_profile()
            .file_system_sandbox_policy()
    }

    /// Effective network sandbox policy derived from the canonical profile.
    #[must_use]
    pub fn network_sandbox_policy(&self) -> NetworkSandboxPolicy {
        self.permission_profile().network_sandbox_policy()
    }

    /// Legacy compatibility projection derived from the canonical profile.
    #[must_use]
    pub fn legacy_sandbox_policy(&self, cwd: &Path) -> SandboxPolicy {
        let permission_profile = self.effective_permission_profile();
        compatibility_sandbox_policy_for_permission_profile(&permission_profile, cwd)
    }

    /// Check whether a legacy sandbox policy can be applied to this permission
    /// set after projecting it into the canonical permission profile.
    pub fn can_set_legacy_sandbox_policy(
        &self,
        sandbox_policy: &SandboxPolicy,
        cwd: &Path,
    ) -> ConstraintResult<()> {
        let file_system_sandbox_policy =
            FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(sandbox_policy, cwd);
        let network_sandbox_policy = NetworkSandboxPolicy::from(sandbox_policy);
        let permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
            SandboxEnforcement::from_legacy_sandbox_policy(sandbox_policy),
            &file_system_sandbox_policy,
            network_sandbox_policy,
        );
        self.can_set_permission_profile(&permission_profile)
    }

    /// Set permissions from a legacy sandbox policy and keep every permission
    /// projection in sync.
    pub fn set_legacy_sandbox_policy(
        &mut self,
        sandbox_policy: SandboxPolicy,
        cwd: &Path,
    ) -> ConstraintResult<()> {
        self.can_set_legacy_sandbox_policy(&sandbox_policy, cwd)?;
        let file_system_sandbox_policy =
            FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(&sandbox_policy, cwd);
        let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
        let permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
            SandboxEnforcement::from_legacy_sandbox_policy(&sandbox_policy),
            &file_system_sandbox_policy,
            network_sandbox_policy,
        );
        self.workspace_roots = match &sandbox_policy {
            SandboxPolicy::WorkspaceWrite { writable_roots, .. } => {
                let mut workspace_roots = vec![
                    AbsolutePathBuf::from_absolute_path(cwd)
                        .unwrap_or_else(|_| AbsolutePathBuf::resolve_path_against_base(cwd, "/")),
                ];
                for root in writable_roots {
                    if !workspace_roots
                        .iter()
                        .any(|existing| existing.as_path() == root.as_path())
                    {
                        workspace_roots.push(AbsolutePathBuf::resolve_path_against_base(root, cwd));
                    }
                }
                workspace_roots
            }
            SandboxPolicy::DangerFullAccess
            | SandboxPolicy::ExternalSandbox { .. }
            | SandboxPolicy::ReadOnly { .. } => vec![
                AbsolutePathBuf::from_absolute_path(cwd)
                    .unwrap_or_else(|_| AbsolutePathBuf::resolve_path_against_base(cwd, "/")),
            ],
        };

        self.set_permission_profile(permission_profile)
    }

    /// Set permissions from the canonical profile.
    pub fn set_permission_profile(
        &mut self,
        permission_profile: PermissionProfile,
    ) -> ConstraintResult<()> {
        let permission_profile =
            self.permission_profile_preserving_managed_denied_reads(permission_profile);
        self.permission_profile_state
            .set_legacy_permission_profile(permission_profile)
    }

    fn permission_profile_preserving_managed_denied_reads(
        &self,
        permission_profile: PermissionProfile,
    ) -> PermissionProfile {
        let Some(managed_deny_read_policy) = self.managed_deny_read_policy.as_ref() else {
            return permission_profile;
        };
        if matches!(
            sandbox_mode_requirement_for_permission_profile(&permission_profile),
            SandboxModeRequirement::DangerFullAccess | SandboxModeRequirement::ExternalSandbox
        ) {
            return permission_profile;
        }
        let enforcement = permission_profile.enforcement();
        let (mut file_system_policy, network_policy) = permission_profile.to_runtime_permissions();
        file_system_policy.preserve_deny_read_restrictions_from(managed_deny_read_policy);
        PermissionProfile::from_runtime_permissions_with_enforcement(
            enforcement,
            &file_system_policy,
            network_policy,
        )
    }
}

// A profile override only inherits the selected profile's proxy/allowlist config
// when Codex is still responsible for the network policy. `Disabled` means no
// outer sandbox, so starting the managed proxy would narrow the override.
/// Configured thread persistence backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ThreadStoreConfig {
    /// Persist threads locally using rollout JSONL files and sqlite metadata.
    #[default]
    Local,
    /// In-memory thread store for test and debug configurations.
    InMemory { id: String },
}

/// Application configuration loaded from disk and merged with overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// App-server-owned destination policy; other runtimes remain unmanaged.
    pub application_network_policy: ra_http::NetworkPolicy,
    /// Provenance for how this [`Config`] was derived (merged layers + enforced
    /// requirements).
    pub config_layer_stack: ConfigLayerStack,

    /// Plugin settings resolved with this snapshot, before capability admission.
    pub plugins: ra_config::types::PluginsConfigToml,

    /// Warnings collected during config load that should be shown on startup.
    pub startup_warnings: Vec<String>,

    /// Optional override of model selection.
    pub model: Option<String>,

    /// Default Daybreak preference for new threads and non-interactive turns.
    pub daybreak_enabled: bool,

    /// Effective service tier request id preference for new turns.
    /// `default` means the user explicitly selected standard routing.
    pub service_tier: Option<String>,

    /// Model used specifically for review sessions.
    pub review_model: Option<String>,

    /// Size of the context window for the model, in tokens.
    pub model_context_window: Option<i64>,

    /// Token usage threshold triggering auto-compaction of conversation history.
    pub model_auto_compact_token_limit: Option<i64>,

    /// Controls whether `model_auto_compact_token_limit` applies to the full
    /// active context or only tokens after the carried compaction-window prefix.
    pub model_auto_compact_token_limit_scope: AutoCompactTokenLimitScope,

    /// Percentage of the usable context window that triggers turn-end compaction.
    /// Zero disables turn-end compaction.
    pub model_post_turn_compact_threshold_percent: u8,

    /// Percentage of the model's full context window at which background compaction starts.
    pub async_compaction_threshold_percent: u8,

    /// Key into the `model_providers` map that specifies which provider to use.
    pub model_provider_id: String,

    /// Info needed to make an API request to the model.
    pub model_provider: ModelProviderInfo,

    /// Deprecated: `friendly` and `pragmatic` no longer select a style.
    pub personality: Option<Personality>,

    /// Effective permission configuration for shell tool execution.
    pub permissions: Permissions,

    /// Whether config explicitly selected named permissions profiles instead
    /// of the legacy `sandbox_mode` syntax.
    pub explicit_permission_profile_mode: bool,

    /// User-defined permission profiles available from effective config.
    pub custom_permission_profiles: Vec<PermissionProfileCatalogEntry>,

    /// Configures who approval requests are routed to for review once they have
    /// been escalated. This does not disable separate safety checks such as
    /// ARC.
    pub approvals_reviewer: ApprovalsReviewer,

    /// `enforce_residency` means web traffic cannot be routed outside of a
    /// particular geography. HTTP clients should direct their requests
    /// using backend-specific headers or URLs to enforce this.
    pub enforce_residency: Constrained<Option<ResidencyRequirement>>,

    /// When `true`, `AgentReasoning` events emitted by the backend will be
    /// suppressed from the frontend output. This can reduce visual noise when
    /// users are only interested in the final agent responses.
    pub hide_agent_reasoning: bool,

    /// When set to `true`, `AgentReasoningRawContentEvent` events will be shown in the UI/output.
    /// Defaults to `false`.
    pub show_raw_agent_reasoning: bool,

    /// Base instructions override.
    pub base_instructions: Option<String>,

    /// Origin of the configured base instructions when supplied by another session or lockfile.
    pub base_instructions_provenance: Option<BaseInstructionsProvenance>,

    /// Developer instructions override injected as a separate message.
    pub developer_instructions: Option<String>,

    /// Guardian-specific policy config override from requirements.toml or config.toml.
    /// This is inserted into the fixed guardian prompt template under the
    /// `# Policy Configuration` section rather than replacing the whole
    /// guardian developer prompt.
    pub guardian_policy_config: Option<String>,

    /// Additional Guardian policy from requirements.toml or config.toml.
    /// Rendered into `{{ extra_policy }}` alongside the resolved tenant policy.
    pub guardian_extra_policy: Option<String>,

    /// Guardian prompt template override from config.toml.
    /// The resolved policy config replaces its `{{ tenant_policy_config }}`
    /// placeholder when a review session is built.
    pub guardian_policy_template: Option<String>,
    /// Transcript encoding shared by Guardian review and scoring.
    pub guardian_transcript_mode: TranscriptFormat,

    /// Optional replacement for the gated history-retrieval instructions.
    /// Blank config values are treated as unset, like other Guardian policy overrides.
    pub guardian_conversation_history_prompt: Option<String>,

    /// Optional per-response history-tool budget. Guardian defaults to 4,000 tokens and
    /// preserves stricter parent tool limits.
    pub guardian_conversation_history_max_output_tokens: Option<NonZeroUsize>,

    /// Include a structured error when Guardian's circuit breaker interrupts a turn.
    pub guardian_circuit_break_action: CircuitBreakAction,

    /// Whether to inject the `<permissions instructions>` developer block.
    pub include_permissions_instructions: bool,

    /// Whether to inject the `<apps_instructions>` developer block.
    pub include_apps_instructions: bool,

    /// Whether to inject the `<collaboration_mode>` developer block.
    pub include_collaboration_mode_instructions: bool,

    /// Whether to inject the `<skills_instructions>` developer block.
    pub include_skill_instructions: bool,

    /// Optional token budget override for the available-skills catalog.
    pub skill_max_context_tokens: Option<NonZeroUsize>,

    /// Whether cloud skills are discovered and exposed to the model.
    pub cloud_skill_enabled: bool,

    /// Whether orchestrator-owned MCP tools are exposed to the model.
    pub orchestrator_mcp_enabled: bool,

    /// Whether to inject the `<environment_context>` user block.
    pub include_environment_context: bool,

    /// Whether environment context includes the current date and timezone.
    pub include_environment_context_time: bool,

    /// Compact prompt override.
    pub compact_prompt: Option<String>,

    /// Optional external notifier command. When set, Codex will spawn this
    /// program after each completed *turn* (i.e. when the agent finishes
    /// processing a user submission). The value must be the full command
    /// broken into argv tokens **without** the trailing JSON argument - Codex
    /// appends one extra argument containing a JSON payload describing the
    /// event.
    ///
    /// Example `~/.rusty-agent/config.toml` snippet:
    ///
    /// ```toml
    /// notify = ["notify-send", "Codex"]
    /// ```
    ///
    /// which will be invoked as:
    ///
    /// ```shell
    /// notify-send Codex '{"type":"agent-turn-complete","turn-id":"12345"}'
    /// ```
    ///
    /// If unset the feature is disabled.
    pub notify: Option<Vec<String>>,

    /// TUI notification settings, including enabled events, delivery method, and focus condition.
    pub tui_notifications: TuiNotificationSettings,

    /// Enable ASCII animations and shimmer effects in the TUI.
    pub animations: bool,

    /// Individual TUI effects, subordinate to the animation master switch.
    pub tui_effects: ra_config::types::TuiEffects,

    /// Rich content rendering preferences, independent of animations.
    pub tui_rendering: ra_config::types::TuiRendering,

    /// Show startup tooltips in the TUI welcome screen.
    pub show_tooltips: bool,

    /// Show a TUI notice when the connected app server is an older stable release.
    pub tui_show_server_version_notice: bool,

    /// Generate automatic TUI recaps. Manual `/recap` remains available when disabled.
    pub tui_auto_recap: bool,

    /// Persisted startup availability NUX state for model tooltips.
    pub model_availability_nux: ModelAvailabilityNuxConfig,

    /// Start the composer in Vim mode (`Normal`) by default.
    pub tui_vim_mode_default: bool,
    pub tui_question_esc_back: bool,

    /// Start the TUI in raw scrollback mode for copy-friendly transcript output.
    pub tui_raw_output_mode: bool,

    /// Own the fullscreen transcript when the alternate screen is enabled.
    pub tui_fullscreen_transcript: bool,

    /// Mouse wheel speed multiplier for transcript scrolling; defaults to one row per event.
    pub tui_mouse_scroll_speed: Option<f64>,

    /// Override the terminal default for copying transcript, footer, and composer selections.
    pub tui_copy_on_select: ra_config::types::CopyOnSelect,

    /// Whether to deselect read-only transcript or footer text after confirmed automatic copy.
    pub tui_copy_on_select_clear_selection: bool,

    /// Right-click text paste fallback for the fullscreen TUI.
    pub tui_right_click_paste: ra_config::types::RightClickPaste,

    /// Start the TUI in the specified collaboration mode (plan/default).

    /// Controls whether the TUI uses the terminal's alternate screen buffer.
    ///
    /// This is the same `tui.alternate_screen` value from `config.toml`.
    /// - `auto` (default): Use alternate screen.
    /// - `always`: Always use alternate screen.
    /// - `never`: Never use alternate screen (inline mode, preserves scrollback).
    pub tui_alternate_screen: AltScreenMode,
    /// Ordered list of status line item identifiers for the TUI.
    ///
    /// When unset, the TUI defaults to: `model-with-reasoning` and `current-dir`.
    pub tui_status_line: Option<Vec<String>>,

    /// Whether to color status line items with colors from the active syntax theme.
    pub tui_status_line_use_colors: bool,

    /// Ordered list of terminal title item identifiers for the TUI.
    ///
    /// When unset, the TUI defaults to: `activity` and `project`.
    /// The `activity` item spins while working and shows an action-required
    /// message when blocked on the user.
    pub tui_terminal_title: Option<Vec<String>>,

    /// Syntax highlighting theme override (kebab-case name).
    pub tui_theme: Option<String>,

    /// Pet id preselected by the terminal pet picker.
    pub tui_pet: Option<String>,

    /// Vertical anchor used by terminal pet rendering.
    pub tui_pet_anchor: TuiPetAnchor,

    /// Preferred layout for resume/fork session picker results.
    pub tui_session_picker_view: SessionPickerViewMode,

    /// Last selected grouping in Agent Command Center.
    pub tui_agents_overview_grouping: ra_config::types::AgentsOverviewGrouping,

    /// Working directory to use when resuming or forking a session.
    /// When unset, prompt if the current and session directories differ.
    pub tui_resume_cwd: Option<ResumeCwdMode>,

    /// Terminal resize-reflow tuning knobs.
    pub terminal_resize_reflow: TerminalResizeReflowConfig,

    /// Keybinding overrides for the TUI.
    ///
    /// Precedence is:
    ///
    /// 1. context table (`tui.keymap.chat`, `tui.keymap.composer`, etc.)
    /// 2. `tui.keymap.global`
    /// 3. built-in defaults
    pub tui_keymap: TuiKeymap,

    /// The absolute directory that should be treated as the current working
    /// directory for the session. All relative paths inside the business-logic
    /// layer are resolved against this path.
    pub cwd: AbsolutePathBuf,

    /// Absolute runtime workspace roots for the session. Symbolic
    /// `:workspace_roots` permission entries are materialized against these
    /// roots while profile-defined workspace roots remain encoded directly in
    /// the permission profile.
    pub workspace_roots: Vec<AbsolutePathBuf>,
    /// Whether runtime workspace roots were supplied explicitly by the caller
    /// or legacy config, rather than defaulting to `cwd`.
    pub workspace_roots_explicit: bool,

    /// Preferred store for CLI auth credentials.
    /// file (default): Use a file in the Codex home directory.
    /// keyring: Use an OS-specific keyring service.
    /// auto: Use the OS-specific keyring service if available, otherwise use a file.

    /// Definition for MCP servers that Codex can reach out to for tool calls.
    pub mcp_servers: Constrained<HashMap<String, McpServerConfig>>,

    /// Trusted `IdP` shared by all permitted EMA MCP registrations.
    pub mcp_enterprise_managed_auth: Option<McpEnterpriseManagedAuthConfig>,

    /// When present, only these MCP servers omit the legacy `mcp__` namespace prefix.
    pub non_prefixed_mcp_tool_servers: Option<Vec<String>>,

    /// Preferred store for MCP OAuth credentials.
    /// keyring: Use an OS-specific keyring service.
    ///          Credentials stored in the keyring will only be readable by Codex unless the user explicitly grants access via OS-level keyring access.
    ///          <https://github.com/openai/codex/blob/main/codex-rs/rmcp-client/src/oauth.rs#L2>
    /// file: `CODEX_HOME/.credentials.json`
    ///       This file will be readable to Codex and other applications running as the same user.
    /// auto (default): keyring if available, otherwise file.
    pub mcp_oauth_credentials_store_mode: OAuthCredentialsStoreMode,

    /// Optional fixed port to use for the local HTTP callback server used during MCP OAuth login.
    ///
    /// When unset, Codex will bind to an ephemeral port chosen by the OS.
    pub mcp_oauth_callback_port: Option<u16>,

    /// Optional redirect URI to use during MCP OAuth login.
    ///
    /// When set, this URI is used in the OAuth authorization request instead
    /// of the local listener address. The local callback listener still binds
    /// to 127.0.0.1 (using `mcp_oauth_callback_port` when provided).
    pub mcp_oauth_callback_url: Option<String>,

    /// How long to wait for optional MCP servers while building the initial tool catalog.
    pub mcp_optional_startup_grace: Duration,

    /// Combined provider map (defaults plus user-defined providers).
    pub model_providers: HashMap<String, ModelProviderInfo>,

    /// Maximum total bytes of project instruction content across all selected environments.
    pub project_doc_max_bytes: usize,

    /// Additional filenames to try when looking for project-level docs.
    pub project_doc_fallback_filenames: Vec<String>,

    /// Token budget applied when storing tool/function outputs in the context manager.
    pub tool_output_token_limit: Option<usize>,

    /// Whether multi-agent tools are enabled through `[agents]`.
    pub agents_enabled: bool,

    /// User-configured maximum number of spawned agent threads per session.
    pub agent_max_threads: Option<usize>,

    /// Default model for spawned subagents when the spawn call does not select one.
    pub agent_default_subagent_model: Option<String>,

    /// Default reasoning effort for spawned subagents when the spawn call does not select one.
    pub agent_default_subagent_reasoning_effort: Option<ReasoningEffort>,

    /// Whether to record a model-visible message when an agent turn is interrupted.
    pub agent_interrupt_message_enabled: bool,

    /// Maximum nesting depth for V1 agent threads. Ignored by V2.
    pub agent_max_depth: i32,

    /// User-defined role declarations keyed by role name.
    pub agent_roles: BTreeMap<String, AgentRoleConfig>,

    /// Maximum token budget allowed for a goal and default budget for new goals.
    pub max_goal_token_budget: Option<i64>,

    /// Memories subsystem settings.
    pub memories: MemoriesConfig,

    /// Directory containing all Codex state (defaults to `~/.rusty-agent` but can be
    /// overridden by the `RA_HOME` environment variable).
    pub codex_home: AbsolutePathBuf,

    /// Resolved configuration shared by all Codex `SQLite` databases.
    pub sqlite: SqliteConfig,

    /// Directory where Codex writes log files (defaults to `$CODEX_HOME/log`).
    pub log_dir: PathBuf,

    /// Settings that govern if and what will be written to `~/.rusty-agent/history.jsonl`.
    pub history: History,

    /// When true, session is not persisted on disk. Default to `false`
    pub ephemeral: bool,

    /// Optional extra configuration fields for the thread.
    pub extra_config: Option<ExtraConfig>,

    /// Whether enabled hooks should run without requiring persisted hook trust for this session.
    ///
    /// This is a runtime-only knob populated from invocation overrides, not from config files.
    pub bypass_hook_trust: bool,

    /// Optional URI-based file opener. If set, citations to files in the model
    /// output will be hyperlinked using the specified URI scheme.
    pub file_opener: UriBasedFileOpener,

    /// Path to the current Codex executable. This cannot be set in the config
    /// file: it must be set in code via [`ConfigOverrides`].
    pub codex_self_exe: Option<PathBuf>,

    /// Path to the `codex-linux-sandbox` executable. This must be set if
    /// [`ra_filesystem::sandbox::SandboxType::LinuxSeccomp`] is used. Note that this
    /// cannot be set in the config file: it must be set in code via
    /// [`ConfigOverrides`].
    ///
    /// When this program is invoked, arg0 will be set to `codex-linux-sandbox`.
    pub codex_linux_sandbox_exe: Option<PathBuf>,

    /// Ignored compatibility field for clients that still pass the retired wrapper path.
    pub main_execve_wrapper_exe: Option<PathBuf>,

    /// Ignored compatibility field for clients that still pass a patched zsh path.
    pub zsh_path: Option<PathBuf>,

    /// Value to use for `reasoning.effort` when making a request using the
    /// Responses API.
    pub model_reasoning_effort: Option<ReasoningEffort>,
    /// Optional Plan-mode-specific reasoning effort override used by the TUI.
    ///
    /// When unset, Plan mode uses the built-in Plan preset default (currently
    /// `medium`). When explicitly set (including `none`), this overrides the
    /// Plan preset. The `none` value means "no reasoning" (not "inherit the
    /// global default").
    pub plan_mode_reasoning_effort: Option<ReasoningEffort>,

    /// Optional value to use for `reasoning.summary` when making a request
    /// using the Responses API. When unset, the model catalog default is used.
    pub model_reasoning_summary: Option<ReasoningSummary>,

    /// Optional full model catalog loaded from `model_catalog_json`.
    /// When set, this replaces the bundled catalog for the current process.
    pub model_catalog: Option<ModelsResponse>,

    /// Optional verbosity control for GPT-5 models (Responses API `text.verbosity`).
    pub model_verbosity: Option<Verbosity>,

    /// Base URL for requests to `ChatGPT` (as opposed to the `OpenAI` API).

    /// Whether Codex-owned clients should respect host system proxy settings.
    pub respect_system_proxy: bool,
    /// Backend URL shared by product plugin services.
    pub chatgpt_base_url: String,

    /// Optional product SKU forwarded to the host-owned apps MCP server.
    pub apps_mcp_product_sku: Option<String>,

    /// Bounded, product-owned metadata attached to every Responses API request.
    pub responses_api_metadata: BTreeMap<String, String>,

    /// Machine-local realtime audio device preferences used by realtime voice.
    pub realtime_audio: RealtimeAudioConfig,

    /// Experimental / do not use. Overrides only the realtime conversation
    /// websocket transport base URL (the `Op::RealtimeConversation`
    /// `/v1/realtime`
    /// connection) without changing normal provider HTTP requests.
    pub experimental_realtime_ws_base_url: Option<String>,
    /// Experimental / do not use. Overrides only the WebRTC realtime call
    /// creation base URL.
    pub experimental_realtime_webrtc_call_base_url: Option<String>,
    /// Experimental / do not use. Selects the realtime websocket model/snapshot
    /// used for the `Op::RealtimeConversation` connection.
    pub experimental_realtime_ws_model: Option<String>,
    /// Experimental / do not use. Realtime websocket session selection.
    /// `version` controls v1/v2 and `type` controls conversational/transcription.
    pub realtime: RealtimeConfig,
    /// Experimental / do not use. Overrides only the realtime conversation
    /// websocket transport instructions (the `Op::RealtimeConversation`
    /// `/ws` session.update instructions) without changing normal prompts.
    pub experimental_realtime_ws_backend_prompt: Option<String>,
    /// Experimental / do not use. Replaces the synthesized realtime startup
    /// context appended to websocket session instructions. An empty string
    /// disables startup context injection entirely.
    pub experimental_realtime_ws_startup_context: Option<String>,
    /// Experimental / do not use. Replaces the built-in realtime start
    /// instructions inserted into developer messages when realtime becomes
    /// active.
    pub experimental_realtime_start_instructions: Option<String>,
    /// Experimental / do not use. Selects the thread persistence backend.
    pub experimental_thread_store: ThreadStoreConfig,
    /// When set, restricts `ChatGPT` login to one or more workspace identifiers.

    /// When set, restricts the login mechanism users may use.

    /// Explicit or feature-derived web search mode.
    pub web_search_mode: Constrained<WebSearchMode>,

    /// Additional parameters for the web search tool when it is enabled.
    pub web_search_config: Option<WebSearchConfig>,

    /// Whether to register the experimental `request_user_input` tool.
    pub experimental_request_user_input_enabled: bool,

    /// Whether to register the `update_plan` tool.
    pub update_plan_enabled: bool,

    /// Policy for collecting and validating tool runtimes.
    pub tool_registry: ToolRegistryConfig,

    /// Configuration for the experimental code-mode tool surface.
    pub code_mode: CodeModeConfig,

    /// Maximum poll window for background terminal output (`write_stdin`), in milliseconds.
    /// Default: `300000` (5 minutes).
    pub background_terminal_max_timeout: u64,

    /// Idle timeout for unsubscribed app-server threads, resolved at server startup.
    pub thread_unload_delay: Duration,

    /// Compatibility-only settings retained for legacy `ghost_snapshot`
    /// config loading.
    pub ghost_snapshot: GhostSnapshotConfig,

    /// Settings specific to the task-path-based multi-agent tool surface.
    pub multi_agent_v2: MultiAgentV2Config,

    /// Context-window token budget configuration, when enabled.
    pub token_budget: Option<TokenBudgetConfig>,
    /// Runtime snapshot of configured token-budget preferences before startup activation.
    pub token_budget_startup_config: Option<super::token_budget_startup::TokenBudgetStartupConfig>,
    /// Shared token budget for the root thread and its sub-agents.
    pub rollout_budget: Option<RolloutBudgetConfig>,
    /// Current-time reminder and clock tool configuration, when enabled.
    pub current_time_reminder: Option<CurrentTimeReminderConfig>,
    /// How the sleep tool is selected when its feature gate is enabled.
    pub sleep_tool_mode: SleepToolMode,

    /// Centralized feature flags; source of truth for feature gating.
    pub features: ManagedFeatures,

    /// Local rollout preference after checking network restrictions and native availability.
    pub prefer_mxc: bool,

    /// Host feature defaults retained beneath explicit configuration overrides.
    pub runtime_feature_defaults: BTreeMap<Feature, bool>,

    /// When `true`, suppress warnings about unstable (under development) features.
    pub suppress_unstable_features_warning: bool,

    /// The currently active project config, resolved by checking if cwd:
    /// is (1) part of a git repo, (2) a git worktree, or (3) just using the cwd
    pub active_project: ProjectConfig,

    /// Collection of various notices we show the user
    pub notices: Notice,

    /// When `true`, checks for Codex updates on startup and surfaces update prompts.
    /// Set to `false` only if your Codex updates are centrally managed.
    /// Defaults to `true`.
    pub check_for_update_on_startup: bool,

    /// When true, disables burst-paste detection for typed input entirely.
    /// All characters are inserted as they are received, and no buffering
    /// or placeholder replacement will occur for fast keypress bursts.
    pub disable_paste_burst: bool,

    /// When `false`, disables `OpenAI` analytics across Codex product surfaces on this machine.
    /// Custom OTLP metrics exporters are controlled by `otel.metrics_exporter`.
    /// Voluntarily left as Optional because the default value might depend on the client.
    pub analytics_enabled: Option<bool>,

    /// When `false`, disables feedback collection across Codex product surfaces.
    /// Defaults to `true`.
    pub feedback_enabled: bool,

    /// Configured discoverable tools for tool suggestions.
    pub tool_suggest: ToolSuggestConfig,

    /// OTEL configuration (exporter type, endpoint, headers, etc.).
    pub otel: ra_config::types::OtelConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolRegistryConfig {
    /// Fail the turn when multiple tools share the same effective name.
    pub error_on_tool_collisions: bool,
    /// Include authoritative tool information in per-turn request metadata.
    pub turn_metadata_includes_tool_info: bool,
}

const DEFAULT_CODE_MODE_EXEC_YIELD_TIME_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeModeConfig {
    pub default_exec_yield_time_ms: u64,
    /// Show handler duration, code-mode host duration, and harness overhead
    /// in each code-mode cell response.
    /// Experimental: this option and the response format may change or be removed.
    pub experimental_show_cell_overhead: bool,
    pub tool_input_schema_max_bytes: Option<usize>,
    pub excluded_tool_namespaces: Vec<String>,
    pub direct_only_tool_namespaces: Vec<String>,
    /// Keep code mode fail-closed when the standalone host is unavailable.
    pub disable_in_process_fallback: bool,
}

impl Default for CodeModeConfig {
    fn default() -> Self {
        Self {
            default_exec_yield_time_ms: DEFAULT_CODE_MODE_EXEC_YIELD_TIME_MS,
            experimental_show_cell_overhead: false,
            tool_input_schema_max_bytes: None,
            excluded_tool_namespaces: Vec::new(),
            direct_only_tool_namespaces: Vec::new(),
            disable_in_process_fallback: false,
        }
    }
}

const TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES: usize = 2000;
const TOKEN_BUDGET_GUIDANCE_MESSAGE_MAX_BYTES: usize = 2000;
const AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenBudgetConfig {
    pub use_history_notes_extension: bool,
    pub reminder_threshold_tokens: Option<i64>,
    pub reminder_message_template: String,
    pub guidance_message: Option<String>,
    pub auto_compact_fallback_prompt: Option<String>,
    pub auto_compact_fallback_buffer_tokens: Option<i64>,
}

impl TokenBudgetConfig {
    pub(crate) fn validate(&self) -> std::io::Result<()> {
        if self
            .reminder_threshold_tokens
            .is_some_and(|tokens| tokens <= 0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.reminder_threshold_tokens must be positive",
            ));
        }

        if self.reminder_message_template.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.reminder_message_template must not be empty",
            ));
        }
        if self.reminder_message_template.len() > TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.token_budget.reminder_message_template must not exceed {TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES} bytes"
                ),
            ));
        }

        if self
            .guidance_message
            .as_ref()
            .is_some_and(|message| message.len() > TOKEN_BUDGET_GUIDANCE_MESSAGE_MAX_BYTES)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.token_budget.guidance_message must not exceed {TOKEN_BUDGET_GUIDANCE_MESSAGE_MAX_BYTES} bytes"
                ),
            ));
        }

        if self
            .auto_compact_fallback_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.len() > AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.token_budget.auto_compact_fallback_prompt must not exceed {AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES} bytes"
                ),
            ));
        }
        if self.auto_compact_fallback_prompt.is_some()
            && self.auto_compact_fallback_buffer_tokens.is_none()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.auto_compact_fallback_buffer_tokens is required when auto_compact_fallback_prompt is set",
            ));
        }
        if self
            .auto_compact_fallback_buffer_tokens
            .is_some_and(|tokens| tokens <= 0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.auto_compact_fallback_buffer_tokens must be positive",
            ));
        }

        Ok(())
    }

    #[must_use]
    pub fn fallback_buffer_tokens(&self) -> i64 {
        if self.auto_compact_fallback_prompt.is_some() {
            self.auto_compact_fallback_buffer_tokens.unwrap_or(0)
        } else {
            0
        }
    }
}

impl Default for TokenBudgetConfig {
    fn default() -> Self {
        Self {
            use_history_notes_extension: false,
            reminder_threshold_tokens: None,
            reminder_message_template: bundled_token_budget_reminder().to_owned(),
            guidance_message: None,
            auto_compact_fallback_prompt: None,
            auto_compact_fallback_buffer_tokens: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RolloutBudgetConfig {
    pub limit_tokens: i64,
    pub reminder_at_remaining_tokens: Vec<i64>,
    pub sampling_token_weight: f64,
    pub prefill_token_weight: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CurrentTimeReminderConfig {
    pub reminder_interval_seconds: u64,
    pub clock_source: CurrentTimeSource,
    pub delivery_mode: CurrentTimeReminderDeliveryMode,
    /// Whether to expose the input-interruptible `clock.sleep` tool.
    pub sleep_tool: bool,
}

impl Default for CurrentTimeReminderConfig {
    fn default() -> Self {
        Self {
            reminder_interval_seconds: 1,
            clock_source: CurrentTimeSource::System,
            delivery_mode: CurrentTimeReminderDeliveryMode::AnyInference,
            sleep_tool: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MultiAgentV2Config {
    pub max_concurrent_threads_per_session: usize,
    pub min_wait_timeout_ms: i64,
    pub max_wait_timeout_ms: i64,
    pub default_wait_timeout_ms: i64,
    pub usage_hint_text: Option<String>,
    pub root_agent_usage_hint_text: Option<String>,
    pub subagent_usage_hint_text: Option<String>,
    pub subagent_developer_instructions: Option<String>,
    pub preserve_fork_prefix: bool,
    pub multi_agent_mode_hint_text: Option<String>,
    pub tool_namespace: Option<String>,
    pub hide_spawn_agent_metadata: bool,
    pub expose_spawn_agent_model_overrides: bool,
    pub wait_agent_enabled: bool,
    pub disable_native_tools: bool,
    pub disable_direct_message: bool,
    pub message_board_in_memory: bool,
    pub message_board_remote: Option<ra_config::features::RemoteMessageBoardConfigToml>,
    pub non_code_mode_only: bool,
}

impl MultiAgentV2Config {
    fn defaults_for_max_concurrency(max_concurrent_threads_per_session: usize) -> Self {
        Self {
            max_concurrent_threads_per_session,
            min_wait_timeout_ms: DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS,
            max_wait_timeout_ms: DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS,
            default_wait_timeout_ms: DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS,
            usage_hint_text: None,
            root_agent_usage_hint_text: None,
            subagent_usage_hint_text: None,
            subagent_developer_instructions: None,
            preserve_fork_prefix: false,
            multi_agent_mode_hint_text: None,
            tool_namespace: Some(DEFAULT_MULTI_AGENT_V2_TOOL_NAMESPACE.to_string()),
            hide_spawn_agent_metadata: true,
            expose_spawn_agent_model_overrides: true,
            wait_agent_enabled: true,
            disable_native_tools: false,
            disable_direct_message: false,
            message_board_in_memory: false,
            message_board_remote: None,
            non_code_mode_only: true,
        }
    }
}

impl Default for MultiAgentV2Config {
    fn default() -> Self {
        Self::defaults_for_max_concurrency(
            DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalResizeReflowMaxRows {
    /// Use the runtime terminal detector to choose a scrollback-sized cap.
    #[default]
    Auto,
    /// Keep all rendered transcript rows during resize reflow.
    Disabled,
    /// Keep at most this many rendered transcript rows during resize reflow.
    Limit(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalResizeReflowConfig {
    pub max_rows: TerminalResizeReflowMaxRows,
}

#[derive(Clone, Default)]
pub struct ConfigBuilder {
    codex_home: Option<PathBuf>,
    cli_overrides: Option<Vec<(String, TomlValue)>>,
    harness_overrides: Option<ConfigOverrides>,
    loader_overrides: Option<LoaderOverrides>,
    strict_config: bool,
    cloud_config_bundle: CloudConfigBundleLoader,
    thread_config_loader: Option<Arc<dyn ThreadConfigLoader>>,
    fallback_cwd: Option<PathBuf>,
    without_project_context: bool,
}

impl ConfigBuilder {
    #[must_use]
    pub fn codex_home(mut self, codex_home: PathBuf) -> Self {
        self.codex_home = Some(codex_home);
        self
    }

    #[must_use]
    pub fn cli_overrides(mut self, cli_overrides: Vec<(String, TomlValue)>) -> Self {
        self.cli_overrides = Some(cli_overrides);
        self
    }

    #[must_use]
    pub fn harness_overrides(mut self, harness_overrides: ConfigOverrides) -> Self {
        self.harness_overrides = Some(harness_overrides);
        self
    }

    /// The harness overrides this builder applies, for a caller that adjusts some of them.
    pub(crate) fn harness_overrides_mut(&mut self) -> &mut ConfigOverrides {
        self.harness_overrides
            .get_or_insert_with(ConfigOverrides::default)
    }

    #[must_use]
    pub fn loader_overrides(mut self, loader_overrides: LoaderOverrides) -> Self {
        self.loader_overrides = Some(loader_overrides);
        self
    }

    #[must_use]
    pub fn strict_config(mut self, strict_config: bool) -> Self {
        self.strict_config = strict_config;
        self
    }

    #[must_use]
    pub fn cloud_config_bundle(mut self, cloud_config_bundle: CloudConfigBundleLoader) -> Self {
        self.cloud_config_bundle = cloud_config_bundle;
        self
    }

    #[must_use]
    pub fn thread_config_loader(
        mut self,
        thread_config_loader: Arc<dyn ThreadConfigLoader>,
    ) -> Self {
        self.thread_config_loader = Some(thread_config_loader);
        self
    }

    #[must_use]
    pub fn fallback_cwd(mut self, fallback_cwd: Option<PathBuf>) -> Self {
        self.fallback_cwd = fallback_cwd;
        self
    }

    /// Materializes `Config.cwd` without using it as config-layer context.
    #[must_use]
    pub fn without_project_context(mut self) -> Self {
        self.without_project_context = true;
        self
    }

    pub async fn build(self) -> std::io::Result<Config> {
        // Keep the large config-loading future off small runtime thread stacks.
        Box::pin(self.build_inner()).await
    }

    async fn build_inner(self) -> std::io::Result<Config> {
        let Self {
            codex_home,
            cli_overrides,
            harness_overrides,
            loader_overrides,
            strict_config,
            cloud_config_bundle,
            thread_config_loader,
            fallback_cwd,
            without_project_context,
        } = self;
        let codex_home = match codex_home {
            Some(codex_home) => AbsolutePathBuf::from_absolute_path(codex_home)?,
            None => find_ra_home()?,
        };
        let cli_overrides = cli_overrides.unwrap_or_default();
        let mut harness_overrides = harness_overrides.unwrap_or_default();
        let loader_overrides = loader_overrides.unwrap_or_default();
        let cwd_override = harness_overrides.cwd.as_deref().or(fallback_cwd.as_deref());
        let cwd = match cwd_override {
            Some(path) => AbsolutePathBuf::relative_to_current_dir(path)?,
            None => AbsolutePathBuf::current_dir()?,
        };
        harness_overrides.cwd = Some(cwd.to_path_buf());
        let config_layer_stack = load_config_layers_state(
            LOCAL_FS.as_ref(),
            &codex_home,
            (!without_project_context).then_some(cwd),
            &cli_overrides,
            ConfigLoadOptions {
                loader_overrides,
                strict_config,
                cloud_config_bundle,
            },
            thread_config_loader
                .as_deref()
                .unwrap_or(&ra_config::NoopThreadConfigLoader),
        )
        .await?;
        let config_toml = config_toml_from_layers(&config_layer_stack).await?;
        Config::load_config_with_layer_stack(
            LOCAL_FS.as_ref(),
            config_toml,
            harness_overrides,
            codex_home,
            config_layer_stack,
        )
        .await
    }

    #[cfg(feature = "test-api")]
    #[must_use]
    pub fn without_managed_config_for_tests() -> Self {
        Self::default().loader_overrides(LoaderOverrides::without_managed_config_for_tests())
    }
}

/// Optional overrides for user configuration (e.g., from CLI flags).
#[derive(Default, Debug, Clone)]
pub struct ConfigOverrides {
    pub model: Option<String>,
    pub review_model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox_mode: Option<SandboxMode>,
    pub permission_profile: Option<PermissionProfile>,
    pub default_permissions: Option<String>,
    /// Permission profile ID recovered from persisted thread state. Explicit
    /// permission overrides take precedence, and stale profile IDs fall back
    /// to the configured default.
    pub persisted_permission_profile_id: Option<String>,
    /// Unnamed permissions from a thread-owned rollout snapshot. These are
    /// fallback state: explicit overrides and current requirements take priority.
    pub persisted_permission_profile: Option<PermissionProfile>,
    pub model_provider: Option<String>,
    pub service_tier: Option<Option<String>>,
    pub codex_self_exe: Option<PathBuf>,
    pub codex_linux_sandbox_exe: Option<PathBuf>,
    /// Ignored compatibility field for clients that still pass the retired wrapper path.
    pub main_execve_wrapper_exe: Option<PathBuf>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    /// Deprecated: `friendly` and `pragmatic` no longer select a style.
    pub personality: Option<Personality>,
    pub compact_prompt: Option<String>,
    pub show_raw_agent_reasoning: Option<bool>,
    pub tools_web_search_request: Option<bool>,
    pub ephemeral: Option<bool>,
    pub bypass_hook_trust: Option<bool>,
    /// Additional directories that should be treated as writable roots for this session.
    pub additional_writable_roots: Vec<PathBuf>,
    /// Explicit absolute runtime workspace roots for this session. When set,
    /// this is the full runtime root list rather than an additive override.
    pub workspace_roots: Option<Vec<AbsolutePathBuf>>,
}

fn dedupe_absolute_paths(paths: &mut Vec<AbsolutePathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

/// Resolves the OSS provider from CLI override or global config.
/// Returns `None` if no provider is configured at any level.
fn resolve_web_search_mode(config_toml: &ConfigToml, features: &Features) -> Option<WebSearchMode> {
    if let Some(mode) = config_toml.web_search {
        return Some(mode);
    }
    if features.enabled(Feature::WebSearchCached) {
        return Some(WebSearchMode::Cached);
    }
    if features.enabled(Feature::WebSearchRequest) {
        return Some(WebSearchMode::Live);
    }
    None
}

fn resolve_web_search_config(config_toml: &ConfigToml) -> Option<WebSearchConfig> {
    config_toml
        .tools
        .as_ref()
        .and_then(|tools| tools.web_search.as_ref())
        .cloned()
        .map(Into::into)
}

fn resolve_experimental_request_user_input_enabled(config_toml: &ConfigToml) -> bool {
    config_toml
        .tools
        .as_ref()
        .and_then(|tools| tools.experimental_request_user_input.as_ref())
        .is_none_or(|config| config.enabled)
}

fn resolve_update_plan_enabled(config_toml: &ConfigToml) -> bool {
    config_toml
        .tools
        .as_ref()
        .and_then(|tools| tools.update_plan.as_ref())
        .is_some_and(|config| config.enabled)
}

fn resolve_feature_enabled(feature: Option<&ra_config::config_toml::FeatureToggleToml>) -> bool {
    feature.and_then(|feature| feature.enabled).unwrap_or(true)
}

fn resolve_code_mode_config(config_toml: &ConfigToml) -> CodeModeConfig {
    let base = code_mode_toml_config(config_toml.features.as_ref());
    let host = config_toml
        .features
        .as_ref()
        .and_then(|features| features.code_mode_host.as_ref())
        .and_then(|feature| match feature {
            FeatureToml::Enabled(_) => None,
            FeatureToml::Config(config) => Some(config),
        });

    CodeModeConfig {
        default_exec_yield_time_ms: base
            .and_then(|config| config.default_exec_yield_time_ms)
            .unwrap_or(DEFAULT_CODE_MODE_EXEC_YIELD_TIME_MS),
        experimental_show_cell_overhead: base
            .and_then(|config| config.experimental_show_cell_overhead)
            .unwrap_or_default(),
        tool_input_schema_max_bytes: base
            .and_then(|config| config.tool_input_schema_max_bytes)
            .map(NonZeroUsize::get),
        excluded_tool_namespaces: base
            .and_then(|config| config.excluded_tool_namespaces.as_ref())
            .cloned()
            .unwrap_or_default(),
        direct_only_tool_namespaces: base
            .and_then(|config| config.direct_only_tool_namespaces.as_ref())
            .cloned()
            .unwrap_or_default(),
        disable_in_process_fallback: host
            .and_then(|config| config.disable_in_process_fallback)
            .unwrap_or_default(),
    }
}

fn resolve_multi_agent_v2_config(config_toml: &ConfigToml) -> MultiAgentV2Config {
    let base = multi_agent_v2_toml_config(config_toml.features.as_ref());
    let max_concurrent_threads_per_session = base
        .and_then(|config| config.max_concurrent_threads_per_session)
        .or_else(|| {
            config_toml
                .agents
                .as_ref()
                .and_then(|agents| agents.max_concurrent_threads_per_session)
                .map(|max_threads| max_threads.saturating_add(1))
        })
        .unwrap_or(DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION);
    let default =
        MultiAgentV2Config::defaults_for_max_concurrency(max_concurrent_threads_per_session);
    let min_wait_timeout_ms = base
        .and_then(|config| config.min_wait_timeout_ms)
        .unwrap_or(default.min_wait_timeout_ms);
    let max_wait_timeout_ms = base
        .and_then(|config| config.max_wait_timeout_ms)
        .unwrap_or(default.max_wait_timeout_ms);
    let default_wait_timeout_ms = base
        .and_then(|config| config.default_wait_timeout_ms)
        .unwrap_or(default.default_wait_timeout_ms);
    let usage_hint_text = base
        .and_then(|config| config.usage_hint_text.as_ref())
        .cloned()
        .or(default.usage_hint_text);
    let hide_spawn_agent_metadata = base
        .and_then(|config| config.hide_spawn_agent_metadata)
        .unwrap_or(default.hide_spawn_agent_metadata);
    let expose_spawn_agent_model_overrides = base
        .and_then(|config| config.expose_spawn_agent_model_overrides)
        .unwrap_or(default.expose_spawn_agent_model_overrides);
    let root_agent_usage_hint_text = base
        .and_then(|config| config.root_agent_usage_hint_text.as_ref())
        .cloned();
    let subagent_usage_hint_text = base
        .and_then(|config| config.subagent_usage_hint_text.as_ref())
        .cloned();
    let wait_agent_enabled = base
        .and_then(|config| config.wait_agent_enabled)
        .unwrap_or(default.wait_agent_enabled);
    let disable_native_tools = base
        .and_then(|config| config.disable_native_tools)
        .unwrap_or(default.disable_native_tools);
    let disable_direct_message = base
        .and_then(|config| config.disable_direct_message)
        .unwrap_or(default.disable_direct_message);
    let message_board_in_memory = base
        .and_then(|config| config.message_board_in_memory)
        .unwrap_or(default.message_board_in_memory);
    let subagent_developer_instructions = base
        .and_then(|config| config.subagent_developer_instructions.as_ref())
        .map(|instructions| instructions.trim().to_string());
    let preserve_fork_prefix = base
        .and_then(|config| config.preserve_fork_prefix)
        .unwrap_or(default.preserve_fork_prefix);
    let multi_agent_mode_hint_text = base
        .and_then(|config| config.multi_agent_mode_hint_text.as_ref())
        .cloned()
        .or(default.multi_agent_mode_hint_text);
    let tool_namespace = base
        .and_then(|config| config.tool_namespace.as_ref())
        .cloned()
        .or(default.tool_namespace);
    let non_code_mode_only = base
        .and_then(|config| config.non_code_mode_only)
        .unwrap_or(default.non_code_mode_only);

    MultiAgentV2Config {
        max_concurrent_threads_per_session,
        min_wait_timeout_ms,
        max_wait_timeout_ms,
        default_wait_timeout_ms,
        usage_hint_text,
        root_agent_usage_hint_text,
        subagent_usage_hint_text,
        subagent_developer_instructions,
        preserve_fork_prefix,
        multi_agent_mode_hint_text,
        tool_namespace,
        hide_spawn_agent_metadata,
        expose_spawn_agent_model_overrides,
        wait_agent_enabled,
        disable_native_tools,
        disable_direct_message,
        message_board_in_memory,
        message_board_remote: base.and_then(|config| config.message_board_remote.clone()),
        non_code_mode_only,
    }
}

pub(crate) fn resolve_token_budget_config(
    config_toml: &ConfigToml,
    features: &ManagedFeatures,
) -> std::io::Result<Option<TokenBudgetConfig>> {
    if !features.enabled(Feature::TokenBudget) {
        return Ok(None);
    }

    let token_budget_config = token_budget_toml_config(config_toml.features.as_ref());
    let use_history_notes_extension = token_budget_config
        .and_then(|config| config.use_history_notes_extension)
        .unwrap_or_default();
    let reminder_threshold_tokens =
        token_budget_config.and_then(|config| config.reminder_threshold_tokens);
    let reminder_message_template = token_budget_config
        .and_then(|config| config.reminder_message_template.clone())
        .unwrap_or_else(|| bundled_token_budget_reminder().to_owned());
    let guidance_message = token_budget_config
        .and_then(|config| config.guidance_message.clone())
        .filter(|message| !message.trim().is_empty());
    let auto_compact_fallback_prompt = token_budget_config
        .and_then(|config| config.auto_compact_fallback_prompt.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let auto_compact_fallback_buffer_tokens =
        token_budget_config.and_then(|config| config.auto_compact_fallback_buffer_tokens);

    let token_budget = TokenBudgetConfig {
        use_history_notes_extension,
        reminder_threshold_tokens,
        reminder_message_template,
        guidance_message,
        auto_compact_fallback_prompt,
        auto_compact_fallback_buffer_tokens,
    };
    token_budget.validate()?;
    Ok(Some(token_budget))
}

fn resolve_rollout_budget_config(
    config_toml: &ConfigToml,
    features: &ManagedFeatures,
) -> std::io::Result<Option<RolloutBudgetConfig>> {
    if !features.enabled(Feature::RolloutBudget) {
        return Ok(None);
    }
    let missing_limit_error = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "features.rollout_budget.limit_tokens is required when rollout_budget is enabled",
        )
    };
    let Some(FeatureToml::Config(config)) = config_toml
        .features
        .as_ref()
        .and_then(|features| features.rollout_budget.as_ref())
    else {
        return Err(missing_limit_error());
    };
    let Some(limit_tokens) = config.limit_tokens else {
        return Err(missing_limit_error());
    };
    if limit_tokens <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "features.rollout_budget.limit_tokens must be positive",
        ));
    }
    let reminder_at_remaining_tokens =
        config
            .reminder_at_remaining_tokens
            .clone()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "features.rollout_budget.reminder_at_remaining_tokens is required when rollout_budget is enabled",
                )
            })?;
    if reminder_at_remaining_tokens
        .iter()
        .any(|&tokens| tokens <= 0 || tokens >= limit_tokens)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "features.rollout_budget.reminder_at_remaining_tokens must contain only positive values below limit_tokens",
        ));
    }
    let sampling_token_weight = config.sampling_token_weight.unwrap_or(1.0);
    let prefill_token_weight = config.prefill_token_weight.unwrap_or(1.0);
    for (field, weight) in [
        ("sampling_token_weight", sampling_token_weight),
        ("prefill_token_weight", prefill_token_weight),
    ] {
        if !weight.is_finite() || weight < 0.0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("features.rollout_budget.{field} must be finite and non-negative"),
            ));
        }
    }
    Ok(Some(RolloutBudgetConfig {
        limit_tokens,
        reminder_at_remaining_tokens,
        sampling_token_weight,
        prefill_token_weight,
    }))
}

fn resolve_current_time_reminder_config(
    config_toml: &ConfigToml,
    features: &ManagedFeatures,
) -> std::io::Result<Option<CurrentTimeReminderConfig>> {
    if !features.enabled(Feature::CurrentTimeReminder) {
        return Ok(None);
    }

    let base = current_time_reminder_toml_config(config_toml.features.as_ref());
    let default = CurrentTimeReminderConfig::default();
    let reminder_interval_seconds = base
        .and_then(|config| config.reminder_interval_seconds)
        .unwrap_or(default.reminder_interval_seconds);

    Ok(Some(CurrentTimeReminderConfig {
        reminder_interval_seconds,
        clock_source: base
            .and_then(|config| config.clock_source)
            .unwrap_or(default.clock_source),
        delivery_mode: base
            .and_then(|config| config.delivery_mode)
            .unwrap_or(default.delivery_mode),
        sleep_tool: base
            .and_then(|config| config.sleep_tool)
            .unwrap_or(default.sleep_tool),
    }))
}

fn resolve_terminal_resize_reflow_config(config_toml: &ConfigToml) -> TerminalResizeReflowConfig {
    let Some(tui) = config_toml.tui.as_ref() else {
        return TerminalResizeReflowConfig::default();
    };

    TerminalResizeReflowConfig {
        max_rows: match tui.terminal_resize_reflow_max_rows {
            Some(0) => TerminalResizeReflowMaxRows::Disabled,
            Some(rows) => TerminalResizeReflowMaxRows::Limit(rows),
            None => TerminalResizeReflowMaxRows::Auto,
        },
    }
}

fn code_mode_toml_config(features: Option<&FeaturesToml>) -> Option<&CodeModeConfigToml> {
    match features?.code_mode.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => Some(config),
    }
}

fn multi_agent_v2_toml_config(features: Option<&FeaturesToml>) -> Option<&MultiAgentV2ConfigToml> {
    match features?.multi_agent_v2.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => Some(config),
    }
}

fn token_budget_toml_config(features: Option<&FeaturesToml>) -> Option<&TokenBudgetConfigToml> {
    match features?.token_budget.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => Some(config),
    }
}

fn current_time_reminder_toml_config(
    features: Option<&FeaturesToml>,
) -> Option<&CurrentTimeReminderConfigToml> {
    match features?.current_time_reminder.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => Some(config),
    }
}

pub(super) fn network_proxy_toml_config(
    features: Option<&FeaturesToml>,
) -> Option<&NetworkProxyConfigToml> {
    match features?.network_proxy.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => Some(config),
    }
}

/// Bootstrap-only resolver for the cloud-config fetch.
///
/// Call before a cloud-config bundle is available. Final [`Config`] loading
/// resolves the effective feature value after all layers are available.
fn validate_multi_agent_v2_wait_timeout(label: &str, value: i64) -> std::io::Result<()> {
    if value < HARD_MIN_MULTI_AGENT_V2_TIMEOUT_MS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{label} must be at least {HARD_MIN_MULTI_AGENT_V2_TIMEOUT_MS}"),
        ));
    }
    if value > HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{label} must be at most {HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS}"),
        ));
    }
    Ok(())
}

fn validate_multi_agent_v2_tool_namespace(namespace: Option<&str>) -> std::io::Result<()> {
    const LABEL: &str = "features.multi_agent_v2.tool_namespace";
    const MAX_LEN: usize = 64;
    const RESERVED_RESPONSES_NAMESPACES: &[&str] = &[
        "api_tool",
        "browser",
        "computer",
        "container",
        "file_search",
        "functions",
        "image_gen",
        "multi_tool_use",
        "python",
        "python_user_visible",
        "submodel_delegator",
        "terminal",
        "tool_search",
        "web",
    ];

    let Some(namespace) = namespace else {
        return Ok(());
    };
    if namespace.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} must not be empty"),
        ));
    }
    if namespace.trim() != namespace {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} must not have leading or trailing whitespace"),
        ));
    }
    if !namespace
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} must match ^[a-zA-Z0-9_-]+$"),
        ));
    }
    if namespace.chars().count() > MAX_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} must be at most {MAX_LEN} characters"),
        ));
    }
    if namespace == "mcp"
        || namespace.starts_with("mcp__")
        || RESERVED_RESPONSES_NAMESPACES.contains(&namespace)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} uses a reserved namespace: {namespace}"),
        ));
    }

    Ok(())
}

impl Config {
    #[cfg(feature = "test-api")]
    pub async fn load_from_base_config_with_overrides(
        cfg: ConfigToml,
        overrides: ConfigOverrides,
        codex_home: AbsolutePathBuf,
    ) -> std::io::Result<Self> {
        // Note this ignores requirements.toml enforcement for tests.
        let config_layer_stack = ConfigLayerStack::default();
        Self::load_config_with_layer_stack(
            LOCAL_FS.as_ref(),
            cfg,
            overrides,
            codex_home,
            config_layer_stack,
        )
        .await
    }

    pub async fn load_config_with_layer_stack(
        fs: &dyn ExecutorFileSystem,
        mut cfg: ConfigToml,
        overrides: ConfigOverrides,
        codex_home: AbsolutePathBuf,
        config_layer_stack: ConfigLayerStack,
    ) -> std::io::Result<Self> {
        // Keep the large config-construction future off small test thread stacks.
        Box::pin(async move {
        if cfg.experimental_thread_store_endpoint.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "`experimental_thread_store_endpoint` is no longer supported; remove it from config.toml",
            ));
        }

        validate_model_providers(&cfg.model_providers)
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        if cfg.model_post_turn_compact_threshold_percent.is_some_and(|percent| percent > 100) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "model_post_turn_compact_threshold_percent must be between 0 and 100",
            ));
        }
        let async_compaction_threshold_percent = cfg.features.as_ref()
            .and_then(|features| features.async_compaction.as_ref())
            .and_then(|feature| match feature {
                FeatureToml::Enabled(_) => None,
                FeatureToml::Config(config) => config.model_async_compact_threshold_percent,
            })
            .unwrap_or(DEFAULT_ASYNC_COMPACTION_THRESHOLD_PERCENT);
        if !(1..=100).contains(&async_compaction_threshold_percent) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "model_async_compact_threshold_percent must be between 1 and 100",
            ));
        }
        if let Some(responses_api_metadata) = cfg.responses_api_metadata.as_ref() {
            validate_extra_metadata(responses_api_metadata.iter()).map_err(|message| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
            })?;
        }
        validate_managed_developer_instructions(
            config_layer_stack
                .requirements()
                .additional_developer_instructions
                .as_ref(),
        )?;
        let orchestrator = cfg.orchestrator.as_ref();
        let cloud_skill_enabled = cfg
            .cloud
            .as_ref()
            .and_then(|cloud| cloud.skills.as_ref())
            .and_then(|skills| skills.enabled)
            .unwrap_or(true);
        let orchestrator_mcp_enabled =
            resolve_feature_enabled(orchestrator.and_then(|value| value.mcp.as_ref()));
        let mut startup_warnings = config_layer_stack
            .startup_warnings()
            .unwrap_or_default()
            .to_vec();
        let configured_sqlite_home = cfg.sqlite_home.clone();
        requirements::apply_to_config(
            &mut cfg,
            config_layer_stack.requirements(),
            &mut startup_warnings,
        );
        // Destructure every field to ensure ConfigRequirements additions are
        // either applied above or handled while constructing the final Config.
        let ConfigRequirements {
            chatgpt_base_url: _,
            sqlite_home: _,
            log_dir: _,
            model_catalog_json: _,
            model_provider: _,
            model_providers: _,
            check_for_update_on_startup: _,
            allow_login_shell: _,
            feedback: _,
            approval_policy: mut constrained_approval_policy,
            approvals_reviewer: mut constrained_approvals_reviewer,
            auto_review_required_models: _,
            permission_profile: mut constrained_permission_profile,
            windows_sandbox_mode: mut constrained_windows_sandbox_mode,
            web_search_mode: mut constrained_web_search_mode,
            allow_managed_hooks_only: _,
            allow_appshots: _,
            allow_remote_control: _,
            computer_use: _,
            feature_requirements,
            managed_hooks: _,
            mcp_servers,
            plugins: _,
            marketplaces: _,
            exec_policy: _,
            enforce_residency,
            network: network_requirements,
            application: _,
            filesystem: filesystem_requirements,
            additional_developer_instructions: _,
            guardian_policy_config_source: _,
            guardian_extra_policy_source: _,
        } = config_layer_stack.requirements().clone();

        // Destructure ConfigOverrides fully to ensure all overrides are applied.
        let ConfigOverrides {
            model,
            review_model: override_review_model,
            cwd,
            approval_policy: approval_policy_override,
            approvals_reviewer: approvals_reviewer_override,
            sandbox_mode,
            permission_profile,
            default_permissions: default_permissions_override,
            persisted_permission_profile_id,
            persisted_permission_profile,
            model_provider,
            service_tier: service_tier_override,
            codex_self_exe,
            codex_linux_sandbox_exe,
            main_execve_wrapper_exe: _,
            base_instructions,
            developer_instructions,
            personality,
            compact_prompt,
            show_raw_agent_reasoning,
            tools_web_search_request: override_tools_web_search_request,
            ephemeral,
            bypass_hook_trust,
            additional_writable_roots,
            workspace_roots: workspace_roots_override,
        } = overrides;
        let bypass_hook_trust = bypass_hook_trust.unwrap_or_default();

        if bypass_hook_trust {
            startup_warnings.push(
                "`--dangerously-bypass-hook-trust` is enabled. Enabled hooks may run without review for this invocation."
                    .to_string(),
            );
        }

        if sandbox_mode.is_some() && permission_profile.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "`sandbox_mode` and `permission_profile` overrides cannot both be set",
            ));
        }
        if sandbox_mode.is_some() && default_permissions_override.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "`sandbox_mode` and `default_permissions` overrides cannot both be set",
            ));
        }
        if permission_profile.is_some() && default_permissions_override.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "`permission_profile` and `default_permissions` overrides cannot both be set",
            ));
        }
        if let Some(profile) = cfg.profile.as_deref() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "legacy `profile = \"{profile}\"` config is no longer supported; use `--profile {profile}` with `{profile}.config.toml` instead"
                ),
            ));
        }

        let tool_suggest = resolve_tool_suggest_config(&cfg, &config_layer_stack);
        let feature_overrides = FeatureOverrides {
            web_search_request: override_tools_web_search_request,
        };

        let configured_features = Features::from_sources(
            FeatureConfigSource {
                features: cfg.features.as_ref(),
                experimental_use_unified_exec_tool: cfg.experimental_use_unified_exec_tool,
            },
            FeatureConfigSource {
                ..Default::default()
            },
            feature_overrides,
        );
        let features = ManagedFeatures::from_configured_with_warnings(
            configured_features,
            feature_requirements,
            &mut startup_warnings,
        )?;
        let mcp_enterprise_managed_auth = McpEnterpriseManagedAuthConfig::resolve(
            &config_layer_stack,
            cfg.mcp_enterprise_managed_auth.as_ref(),
            &cfg.mcp_servers,
            features.enabled(Feature::UseXaa),
        )?;
        let non_prefixed_mcp_tool_servers = if features.enabled(Feature::NonPrefixedMcpToolNames) {
            cfg.features
                .as_ref()
                .and_then(|features| features.non_prefixed_mcp_tool_names.as_ref())
                .and_then(|feature| match feature {
                    FeatureToml::Enabled(_) => None,
                    FeatureToml::Config(config) => config.server_names.clone(),
                })
        } else {
            None
        };
        let respect_system_proxy = features.enabled(Feature::RespectSystemProxy);
        let enable_network_proxy = features.enabled(Feature::NetworkProxy);
        let allow_mxc =
            cfg.windows.as_ref().and_then(|windows| windows.allow_mxc) != Some(false);
        if !allow_mxc && resolve_windows_sandbox_mode(&cfg) == Some(WindowsSandboxModeToml::Mxc) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "windows.sandbox = \"mxc\" is not allowed when windows.allow_mxc = false",
            ));
        }
        let PreparedWindowsSandboxConfig {
            mode: windows_sandbox_mode,
            sandbox_type: windows_sandbox_type,
            level: windows_sandbox_level,
        } = prepare_windows_sandbox_config(
            resolve_windows_sandbox_mode(&cfg),
            WindowsSandboxLevel::from_features(&features),
            &mut constrained_windows_sandbox_mode,
            &mut startup_warnings,
        )?;
        let resolved_cwd = AbsolutePathBuf::try_from(normalize_for_native_workdir({
            use std::env;

            match cwd {
                None => {
                    tracing::info!("cwd not set, using current dir");
                    env::current_dir()?
                }
                Some(p) if p.is_absolute() => p,
                Some(p) => {
                    // Resolve relative path against the current working directory.
                    tracing::info!("cwd is relative, resolving against current dir");
                    let mut current = env::current_dir()?;
                    current.push(p);
                    current
                }
            }
        }))?;
        let requested_additional_writable_roots: Vec<AbsolutePathBuf> = additional_writable_roots
            .into_iter()
            .map(|path| AbsolutePathBuf::resolve_path_against_base(path, resolved_cwd.as_path()))
            .collect();
        let repo_root = resolve_root_git_project_for_trust(fs, &resolved_cwd).await;
        let active_project = cfg
            .get_active_project(
                resolved_cwd.as_path(),
                repo_root.as_ref().map(AbsolutePathBuf::as_path),
            )
            .unwrap_or(ProjectConfig { trust_level: None });
        let permission_config_syntax = resolve_permission_config_syntax(
            &config_layer_stack,
            &cfg,
            sandbox_mode,
        );
        let requirements_toml = config_layer_stack.requirements_toml();
        let persisted_permission_profile = persisted_permission_profile.filter(|profile| {
            sandbox_mode.is_none()
                && permission_profile.is_none()
                && default_permissions_override.is_none()
                && persisted_permission_profile_id.is_none()
                && requirements_toml.allowed_permission_profiles.is_none()
                // Executor-owned enforcement and materialized grants are not portable
                // thread defaults. Reject the original mode before preserving denials.
                && !matches!(profile, PermissionProfile::External { .. })
                && !profile.file_system_sandbox_policy().entries.iter().any(|entry| {
                    entry.access.can_read()
                        && matches!(entry.path, FileSystemPath::Path { .. } | FileSystemPath::GlobPattern { .. })
                })
                && permission_profile_catalog::permission_profile_satisfies_requirements(
                    &config_layer_stack,
                    profile,
                )
        });
        let persisted_permission_profile_id = if sandbox_mode.is_some()
            || permission_profile.is_some()
            || default_permissions_override.is_some()
        {
            None
        } else {
            persisted_permission_profile_id.as_deref()
        };
        let permission_path_context = ConfigPathContext::new(
            PathConvention::native(),
            Some(PathUri::from_abs_path(&resolved_cwd)),
            AbsolutePathBufGuard::home_directory()
                .and_then(|home| PathUri::from_host_native_path(home).ok()),
        );
        let effective_permission_selection = resolve_effective_permission_selection(
            cfg.permissions.as_ref(),
            default_permissions_override.as_deref(),
            persisted_permission_profile_id.map(|profile_id| (profile_id, &permission_path_context)),
            cfg.default_permissions.as_deref(),
            requirements_toml,
            &mut startup_warnings,
        )?;
        if effective_permission_selection.has_profiles()
            && !matches!(
                permission_config_syntax,
                Some(PermissionConfigSyntax::Legacy)
            )
            && effective_permission_selection.selected_profile_id.is_none()
            && !effective_permission_selection.requirements_force_profile_selection
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "config defines `[permissions]` profiles but does not set `default_permissions`",
            ));
        }

        let memories_config: MemoriesConfig = cfg.memories.clone().unwrap_or_default().into();
        let memories_root = codex_home.join(memories_config.version.directory_name());

        let profiles_are_active = effective_permission_selection.profiles_are_active(
            default_permissions_override.as_deref(),
            permission_config_syntax,
        );
        let prefer_mxc = features.enabled(Feature::PreferMxc)
            && config_allows_mxc(
                &constrained_windows_sandbox_mode,
                &effective_permission_selection,
                profiles_are_active,
                permission_profile.as_ref(),
                network_requirements.as_ref(),
                &cfg,
                enable_network_proxy,
            )?
            && windows_mxc_available();
        let local_windows_sandbox_type =
            resolve_windows_sandbox_type(windows_sandbox_type, prefer_mxc);
        let legacy_windows_sandbox_level = windows_sandbox_level_for_legacy_checks(
            local_windows_sandbox_type,
            windows_sandbox_level,
        );
        let explicit_permission_profile_mode = effective_permission_selection
            .persisted_profile_id_was_provided
            || default_permissions_override.is_some()
            || matches!(
                permission_config_syntax,
                Some(PermissionConfigSyntax::Profiles)
            );
        let custom_permission_profiles = permission_profile_catalog_from_permissions(
            &config_layer_stack,
            effective_permission_selection.profiles.as_ref(),
            &permission_path_context,
        )?
        .into_iter()
        .filter(|profile| !is_builtin_permission_profile_name(&profile.id))
        .collect();
        let using_implicit_builtin_profile = !effective_permission_selection
            .persisted_profile_id_was_provided
            && permission_config_syntax.is_none()
            && effective_permission_selection.selected_profile_id.is_none();
        let should_seed_legacy_workspace_roots = effective_permission_selection
            .selected_profile_id
            .is_none()
            && matches!(
                permission_config_syntax,
                None | Some(PermissionConfigSyntax::Legacy)
            );
        let legacy_workspace_roots_explicit = should_seed_legacy_workspace_roots
            && cfg
                .sandbox_workspace_write
                .as_ref()
                .is_some_and(|sandbox_workspace_write| {
                    !sandbox_workspace_write.writable_roots.is_empty()
                });
        let workspace_roots_explicit = workspace_roots_override.is_some()
            || !requested_additional_writable_roots.is_empty()
            || legacy_workspace_roots_explicit;
        let mut workspace_roots = if let Some(workspace_roots) = workspace_roots_override { workspace_roots } else {
            let mut workspace_roots = vec![resolved_cwd.clone()];
            workspace_roots.extend(requested_additional_writable_roots.clone());
            if should_seed_legacy_workspace_roots
                && let Some(sandbox_workspace_write) = cfg.sandbox_workspace_write.as_ref()
            {
                workspace_roots.extend(sandbox_workspace_write.writable_roots.clone());
            }
            workspace_roots
        };
        dedupe_absolute_paths(&mut workspace_roots);
        let (
            configured_network_proxy_config,
            mut permission_profile,
            mut file_system_sandbox_policy,
            mut active_permission_profile,
            mut profile_workspace_roots,
        ) = if let Some(permission_profile) = permission_profile {
            let (file_system_sandbox_policy, _network_sandbox_policy) =
                permission_profile.to_runtime_permissions();
            let configured_network_proxy_config =
                if profile_allows_configured_network_proxy(&permission_profile)
                    && profiles_are_active
                {
                    // PermissionProfile carries the active network sandbox bit, not the configured
                    // proxy/allowlist policy. Keep that config so active profiles can round-trip
                    // without broadening network behavior.
                    let default_permissions = effective_permission_selection
                        .selected_profile_id
                        .unwrap_or_else(|| {
                            default_builtin_permission_profile_name(
                                &active_project,
                                legacy_windows_sandbox_level,
                            )
                        });
                    network_proxy_config_for_profile_selection(
                        effective_permission_selection.profiles.as_ref(),
                        default_permissions,
                    )?
                } else {
                    NetworkProxyConfig::default()
                };
            (
                configured_network_proxy_config,
                permission_profile,
                file_system_sandbox_policy,
                None,
                Vec::new(),
            )
        } else if profiles_are_active {
            let default_permissions = effective_permission_selection
                .selected_profile_id
                .unwrap_or_else(|| {
                    default_builtin_permission_profile_name(
                        &active_project,
                        legacy_windows_sandbox_level,
                    )
                });
            let builtin_workspace_write_settings = if using_implicit_builtin_profile {
                cfg.sandbox_workspace_write.as_ref().map(|settings| WorkspaceWriteSettings {
                    writable_roots: settings.writable_roots.iter().map(PathUri::from_abs_path).collect(),
                    network_access: settings.network_access,
                    exclude_tmpdir_env_var: settings.exclude_tmpdir_env_var,
                    exclude_slash_tmp: settings.exclude_slash_tmp,
                })
            } else {
                None
            };
            let configured_network_proxy_config = network_proxy_config_for_profile_selection(
                effective_permission_selection.profiles.as_ref(),
                default_permissions,
            )?;
            let CompiledPermissionProfile {
                permission_profile,
                workspace_roots: configured_workspace_roots,
            } = compile_permission_profile(
                effective_permission_selection.profiles.as_ref(),
                default_permissions,
                &permission_path_context,
                builtin_workspace_write_settings.as_ref(),
                &mut startup_warnings,
            )?;
            let file_system_sandbox_policy = permission_profile.file_system_sandbox_policy();
            let active_permission_profile = if using_implicit_builtin_profile
                && default_permissions == BUILT_IN_PERMISSION_PROFILE_WORKSPACE
                && cfg.sandbox_workspace_write.is_some()
            {
                // The implicit built-in profile preserves legacy
                // `[sandbox_workspace_write]` customizations, but explicitly
                // selecting `:workspace` intentionally ignores those legacy
                // settings. Do not advertise a re-selectable active profile
                // when doing so would lose roots, network, or tmp settings.
                None
            } else {
                let selected_profile_extends = cfg
                    .permissions
                    .as_ref()
                    .and_then(|permissions| permissions.entries.get(default_permissions))
                    .and_then(|profile| profile.extends.clone());
                Some(ActivePermissionProfile {
                    id: default_permissions.to_string(),
                    extends: selected_profile_extends,
                })
            };
            (
                configured_network_proxy_config,
                permission_profile,
                file_system_sandbox_policy,
                active_permission_profile,
                configured_workspace_roots,
            )
        } else {
            let configured_network_proxy_config = NetworkProxyConfig::default();
            // No named `[permissions]` profile is active, but permissions
            // should still flow through the canonical profile representation.
            // Derive the old `sandbox_mode` defaults as a profile first, then
            // keep a legacy-compatible projection only for the remaining code
            // paths that still speak `SandboxPolicy`.
            let mut permission_profile = cfg
                .derive_permission_profile(
                    sandbox_mode,
                    legacy_windows_sandbox_level,
                    Some(&active_project),
                    Some(&constrained_permission_profile),
                )
                .await;
            // The legacy-derived profiles above are expected to be
            // representable as `SandboxPolicy`. This guard keeps the old safe
            // fallback behavior if future changes make this branch derive a
            // profile with split-only filesystem semantics, such as root write
            // with carveouts or writes that are not expressible as
            // workspace-write roots.
            if let Err(err) = permission_profile.to_legacy_sandbox_policy(resolved_cwd.as_path()) {
                tracing::warn!(
                    error = %err,
                    "derived permission profile cannot be represented as a legacy sandbox policy; falling back to read-only"
                );
                permission_profile = PermissionProfile::read_only();
            }
            let (file_system_sandbox_policy, _network_sandbox_policy) =
                permission_profile.to_runtime_permissions();
            (
                configured_network_proxy_config,
                permission_profile,
                file_system_sandbox_policy,
                None,
                Vec::new(),
            )
        };
        if let Some(saved) = persisted_permission_profile {
            let (mut saved_file_system, saved_network) = saved.to_runtime_permissions();
            saved_file_system.preserve_deny_read_restrictions_from(&file_system_sandbox_policy);
            let restored = PermissionProfile::from_runtime_permissions_with_enforcement(
                saved.enforcement(),
                &saved_file_system,
                saved_network,
            );
            if permission_profile_catalog::permission_profile_satisfies_requirements(
                &config_layer_stack,
                &restored,
            ) {
                permission_profile = restored;
                file_system_sandbox_policy = saved_file_system;
                active_permission_profile = None;
                profile_workspace_roots.clear();
            }
        }
        let prepared_network = PreparedNetworkConfig::from_inputs(NetworkConfigInputs {
            configured_proxy: configured_network_proxy_config,
            feature_enabled: enable_network_proxy,
            features: cfg.features.as_ref(),
            candidate_permission_profile: &permission_profile,
            credential_broker_base_url: cfg.openai_base_url.as_deref(),
        });
        if cfg.approval_policy == Some(AskForApproval::UnlessTrusted) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                UnsupportedUntrustedApprovalPolicyError,
            ));
        }
        let approval_policy_was_explicit =
            approval_policy_override.is_some() || cfg.approval_policy.is_some();
        let mut approval_policy = approval_policy_override
            .or(cfg.approval_policy)
            .unwrap_or_else(|| {
                if active_project.is_trusted() {
                    AskForApproval::OnRequest
                } else if active_project.is_untrusted() {
                    AskForApproval::UnlessTrusted
                } else {
                    AskForApproval::default()
                }
            });
        if !approval_policy_was_explicit
            && let Err(err) = constrained_approval_policy.can_set(&approval_policy)
        {
            tracing::warn!(
                error = %err,
                "default approval policy is disallowed by requirements; falling back to required default"
            );
            approval_policy = constrained_approval_policy.value();
        }
        let approvals_reviewer_was_explicit =
            approvals_reviewer_override.is_some() || cfg.approvals_reviewer.is_some();
        let mut approvals_reviewer = approvals_reviewer_override
            .or(cfg.approvals_reviewer)
            .unwrap_or(ApprovalsReviewer::User);
        if !approvals_reviewer_was_explicit
            && let Err(err) = constrained_approvals_reviewer.can_set(&approvals_reviewer)
        {
            tracing::warn!(
                error = %err,
                "default approvals reviewer is disallowed by requirements; falling back to required default"
            );
            approvals_reviewer = constrained_approvals_reviewer.value();
        }
        let web_search_mode =
            resolve_web_search_mode(&cfg, &features).unwrap_or(WebSearchMode::Cached);
        let web_search_config = resolve_web_search_config(&cfg);
        let experimental_request_user_input_enabled =
            resolve_experimental_request_user_input_enabled(&cfg);
        let update_plan_enabled = resolve_update_plan_enabled(&cfg);
        let tool_registry = ToolRegistryConfig {
            error_on_tool_collisions: cfg
                .features
                .as_ref()
                .and_then(|features| features.tool_registry.as_ref())
                .and_then(|config| config.error_on_tool_collisions)
                .unwrap_or_default(),
            turn_metadata_includes_tool_info: cfg
                .features
                .as_ref()
                .and_then(|features| features.tool_registry.as_ref())
                .and_then(|config| config.turn_metadata_includes_tool_info)
                .unwrap_or_default(),
        };
        let code_mode = resolve_code_mode_config(&cfg);
        let multi_agent_v2 = resolve_multi_agent_v2_config(&cfg);
        let token_budget = resolve_token_budget_config(&cfg, &features)?;
        let rollout_budget = resolve_rollout_budget_config(&cfg, &features)?;
        let current_time_reminder = resolve_current_time_reminder_config(&cfg, &features)?;
        let sleep_tool_mode = cfg
            .features
            .as_ref()
            .and_then(|features| features.sleep_tool.as_ref())
            .and_then(|feature| match feature {
                FeatureToml::Enabled(_) => None,
                FeatureToml::Config(config) => config.mode,
            })
            .unwrap_or_default();
        let terminal_resize_reflow = resolve_terminal_resize_reflow_config(&cfg);

        let agent_roles =
            load_agent_roles(fs, &cfg, &config_layer_stack, &mut startup_warnings).await?;

        let openai_base_url = cfg
            .openai_base_url
            .clone()
            .filter(|value| !value.is_empty());

        let model_providers =
            merge_configured_model_providers(built_in_model_providers(openai_base_url), cfg.model_providers)
                .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;

        let model_provider_id = config_layer_stack.required_model_provider().map(str::to_string)
            .or(model_provider)
            .or(cfg.model_provider)
            .unwrap_or_else(|| "openai".to_string());
        let model_provider = model_providers
            .get(&model_provider_id)
            .ok_or_else(|| {
                let message = if model_provider_id == LEGACY_OLLAMA_CHAT_PROVIDER_ID {
                    OLLAMA_CHAT_PROVIDER_REMOVED_ERROR.to_string()
                } else {
                    format!("Model provider `{model_provider_id}` not found")
                };
                std::io::Error::new(std::io::ErrorKind::NotFound, message)
            })?
            .clone();

        let shell_environment_policy = ShellEnvironmentPolicy::from(cfg.shell_environment_policy);
        let allow_login_shell = cfg.allow_login_shell.unwrap_or(true);

        let history = cfg.history.unwrap_or_default();

        if multi_agent_v2.max_concurrent_threads_per_session == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.multi_agent_v2.max_concurrent_threads_per_session must be at least 1",
            ));
        }
        validate_multi_agent_v2_wait_timeout(
            "features.multi_agent_v2.min_wait_timeout_ms",
            multi_agent_v2.min_wait_timeout_ms,
        )?;
        validate_multi_agent_v2_wait_timeout(
            "features.multi_agent_v2.max_wait_timeout_ms",
            multi_agent_v2.max_wait_timeout_ms,
        )?;
        validate_multi_agent_v2_wait_timeout(
            "features.multi_agent_v2.default_wait_timeout_ms",
            multi_agent_v2.default_wait_timeout_ms,
        )?;
        if multi_agent_v2.min_wait_timeout_ms > multi_agent_v2.max_wait_timeout_ms {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.multi_agent_v2.min_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms",
            ));
        }
        if multi_agent_v2.default_wait_timeout_ms < multi_agent_v2.min_wait_timeout_ms {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.multi_agent_v2.default_wait_timeout_ms must be at least features.multi_agent_v2.min_wait_timeout_ms",
            ));
        }
        if multi_agent_v2.default_wait_timeout_ms > multi_agent_v2.max_wait_timeout_ms {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.multi_agent_v2.default_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms",
            ));
        }
        validate_multi_agent_v2_tool_namespace(multi_agent_v2.tool_namespace.as_deref())?;
        let agents_enabled = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.enabled)
            .unwrap_or(true);
        let agent_max_threads = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.max_concurrent_threads_per_session);
        if agent_max_threads == Some(0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "agents.max_concurrent_threads_per_session must be at least 1",
            ));
        }
        let agent_max_depth = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.max_depth)
            .unwrap_or(DEFAULT_AGENT_MAX_DEPTH);
        let agent_default_subagent_model = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.default_subagent_model.clone());
        let agent_default_subagent_reasoning_effort = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.default_subagent_reasoning_effort.clone());
        let agent_interrupt_message_enabled = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.interrupt_message)
            .unwrap_or(true);
        let background_terminal_max_timeout = cfg
            .background_terminal_max_timeout
            .unwrap_or(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
            .max(MIN_EMPTY_YIELD_TIME_MS);
        let thread_unload_delay =
            Duration::from_secs(cfg.thread_unload_delay_secs.unwrap_or(/*default*/ 1800));
        if std::time::Instant::now()
            .checked_add(thread_unload_delay)
            .is_none()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "thread_unload_delay_secs is too large",
            ));
        }

        let ghost_snapshot = {
            let mut config = GhostSnapshotConfig::default();
            if let Some(ghost_snapshot) = cfg.ghost_snapshot.as_ref()
                && let Some(ignore_over_bytes) = ghost_snapshot.ignore_large_untracked_files
            {
                config.ignore_large_untracked_files = if ignore_over_bytes > 0 {
                    Some(ignore_over_bytes)
                } else {
                    None
                };
            }
            if let Some(ghost_snapshot) = cfg.ghost_snapshot.as_ref()
                && let Some(threshold) = ghost_snapshot.ignore_large_untracked_dirs
            {
                config.ignore_large_untracked_dirs =
                    if threshold > 0 { Some(threshold) } else { None };
            }
            if let Some(ghost_snapshot) = cfg.ghost_snapshot.as_ref()
                && let Some(disable_warnings) = ghost_snapshot.disable_warnings
            {
                config.disable_warnings = disable_warnings;
            }
            config
        };

        let model = model.or(cfg.model);
        let notices = cfg.notice.unwrap_or_default();
        let service_tier = match service_tier_override {
            Some(Some(service_tier)) => Some(service_tier),
            Some(None) => Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string()),
            None => cfg.service_tier,
        };
        let service_tier = service_tier.and_then(|service_tier| {
            match ServiceTier::from_request_value(&service_tier) {
                Some(ServiceTier::Fast) => features
                    .enabled(Feature::FastMode)
                    .then(|| ServiceTier::Fast.request_value().to_string()),
                Some(ServiceTier::Flex) => Some(ServiceTier::Flex.request_value().to_string()),
                None if service_tier == "ultrafast" => features
                    .enabled(Feature::UltrafastMode)
                    .then_some(service_tier),
                None => Some(service_tier),
            }
        });

        let compact_prompt = compact_prompt.or(cfg.compact_prompt).and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        // Load base instructions override from a file if specified. If the
        // path is relative, resolve it against the effective cwd so the
        // behaviour matches other path-like config values.
        let model_instructions_path = cfg.model_instructions_file.as_ref();
        let file_base_instructions = Self::try_read_non_empty_file(
            fs,
            model_instructions_path,
            "model instructions file",
        )
        .await?;
        let base_instructions = base_instructions
            .or(file_base_instructions)
            .or(cfg.instructions.clone());
        let base_instructions_provenance = base_instructions
            .as_ref()
            .map(|_| BaseInstructionsProvenance::Custom);
        let developer_instructions = developer_instructions.or(cfg.developer_instructions);
        let include_permissions_instructions = cfg.include_permissions_instructions.unwrap_or(true);
        let include_apps_instructions = cfg.include_apps_instructions.unwrap_or(true);
        let include_collaboration_mode_instructions =
            cfg.include_collaboration_mode_instructions.unwrap_or(true);
        let include_skill_instructions = cfg
            .skills
            .as_ref()
            .and_then(|skills| skills.include_instructions)
            .unwrap_or(true);
        let skill_max_context_tokens = cfg
            .skills
            .as_ref()
            .and_then(|skills| skills.max_context_tokens);
        let include_environment_context = cfg.include_environment_context.unwrap_or(true);
        let include_environment_context_time = cfg.include_environment_context_time.unwrap_or(true);
        let guardian_policy_config =
            guardian_policy_config_from_requirements(config_layer_stack.requirements_toml())
                .or_else(|| {
                    cfg.auto_review
                        .as_ref()
                        .and_then(|auto_review| normalize_guardian_policy_config(
                            auto_review.policy.as_deref(),
                        ))
                });
        let guardian_extra_policy = normalize_guardian_policy_config(
            config_layer_stack
                .requirements_toml()
                .guardian_extra_policy
                .as_deref(),
        )
        .or_else(|| {
            cfg.auto_review.as_ref().and_then(|auto_review| {
                normalize_guardian_policy_config(auto_review.extra_policy.as_deref())
            })
        });
        let guardian_transcript_mode = cfg
            .features
            .as_ref()
            .and_then(|features| features.guardianv2.as_ref())
            .and_then(|feature| match feature {
                FeatureToml::Config(config) => config.transcript_mode,
                FeatureToml::Enabled(_) => None,
            })
            .unwrap_or_default();
        let guardian_policy_template = cfg
            .auto_review
            .as_ref()
            .and_then(|auto_review| {
                normalize_guardian_policy_config(
                    auto_review.experimental_policy_template.as_deref(),
                )
            });
        let guardian_conversation_history_prompt = cfg.auto_review.as_ref().and_then(|auto_review| {
            normalize_guardian_policy_config(
                auto_review.experimental_conversation_history_prompt.as_deref(),
            )
        });
        let guardian_conversation_history_max_output_tokens = cfg
            .auto_review
            .as_ref()
            .and_then(|auto_review| auto_review.conversation_history_max_output_tokens);
        let personality = personality.or(cfg.personality);

        let experimental_compact_prompt_path = cfg.experimental_compact_prompt_file.as_ref();
        let file_compact_prompt = Self::try_read_non_empty_file(
            fs,
            experimental_compact_prompt_path,
            "experimental compact prompt file",
        )
        .await?;
        let compact_prompt = compact_prompt.or(file_compact_prompt);
        let review_model = override_review_model.or(cfg.review_model);

        let check_for_update_on_startup = cfg.check_for_update_on_startup.unwrap_or(true);
        let model_catalog = load_model_catalog(cfg.model_catalog_json.clone())?;

        let log_dir = cfg
            .log_dir
            .as_ref().map_or_else(|| codex_home.join("log").to_path_buf(), AbsolutePathBuf::to_path_buf);
        let sqlite_home_env = resolve_sqlite_home_env(&resolved_cwd);
        requirements::push_sqlite_home_env_override_warning(
            configured_sqlite_home.as_ref(),
            sqlite_home_env.as_deref(),
            config_layer_stack.requirements().sqlite_home.as_ref(),
            &mut startup_warnings,
        );
        let sqlite_home = cfg
            .sqlite_home
            .clone()
            .or(sqlite_home_env)
            .unwrap_or_else(|| codex_home.clone());
        let original_permission_profile = permission_profile.clone();
        apply_requirement_constrained_value(
            "approval_policy",
            approval_policy,
            &mut constrained_approval_policy,
            &mut startup_warnings,
        )?;
        if let Some(Sourced {
            value: filesystem_requirements,
            source: filesystem_requirements_source,
        }) = filesystem_requirements.as_ref()
            && !filesystem_requirements.deny_read.is_empty()
        {
            let requirement_source = filesystem_requirements_source.clone();
            constrained_permission_profile
                .value
                .add_validator(move |permission_profile| {
                    validate_permission_profile_for_deny_read(
                        permission_profile,
                        &requirement_source,
                    )
                })
                .map_err(std::io::Error::from)?;
        }
        apply_requirement_constrained_value(
            "approvals_reviewer",
            approvals_reviewer,
            &mut constrained_approvals_reviewer,
            &mut startup_warnings,
        )?;
        let permission_profile_was_constrained = apply_requirement_constrained_value(
            "permission_profile",
            permission_profile,
            &mut constrained_permission_profile,
            &mut startup_warnings,
        )?;
        if permission_profile_was_constrained
            && sandbox_mode_requirement_for_permission_profile(&original_permission_profile)
                == SandboxModeRequirement::DangerFullAccess
            && constrained_permission_profile.get() == &PermissionProfile::read_only()
            && constrained_approval_policy.value() == AskForApproval::Never
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "`approval_policy = \"never\"` cannot be used because requirements do not allow `sandbox_mode = \"danger-full-access\"`; Codex would fall back to read-only permissions with approvals disabled. Choose an `approval_policy` based on what you need, such as `on-request`, or choose an allowed sandbox mode.",
            ));
        }
        if permission_profile_was_constrained {
            // The selected profile no longer describes the effective
            // permissions after requirements forced a fallback.
            active_permission_profile = None;
            profile_workspace_roots.clear();
        }
        apply_requirement_constrained_value(
            "web_search_mode",
            web_search_mode,
            &mut constrained_web_search_mode,
            &mut startup_warnings,
        )?;

        let mcp_servers = crate::plugins::runtime_config::constrain_mcp_servers(cfg.mcp_servers.clone(), mcp_servers.as_ref())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;

        let network_permission_profile = constrained_permission_profile.get().clone();
        let network = build_network_proxy_spec(
            prepared_network.configured_proxy,
            network_requirements,
            &network_permission_profile,
            &shell_environment_policy.r#set,
            &features,
            cfg.features.as_ref(),
        )?;
        let mut helper_readable_roots: Vec<_> = std::env::var_os("PATH")
            .as_deref()
            .and_then(|path| active_arg0_helper_dir(&codex_home, path))
            .into_iter()
            .collect();
        if features.enabled(Feature::MemoryTool) && memories_config.use_memories {
            helper_readable_roots.push(memories_root);
        }
        let effective_permission_profile = constrained_permission_profile.value.get().clone();
        let (mut effective_file_system_sandbox_policy, effective_network_sandbox_policy) =
            effective_permission_profile.to_runtime_permissions();
        if effective_permission_profile != original_permission_profile {
            effective_file_system_sandbox_policy
                .preserve_deny_read_restrictions_from(&file_system_sandbox_policy);
        }
        let managed_deny_read_policy = filesystem_requirements
            .as_ref()
            .filter(|Sourced { value, .. }| !value.deny_read.is_empty())
            .map(|Sourced { value, .. }| -> std::io::Result<_> {
                let mut policy = FileSystemSandboxPolicy::restricted(Vec::new());
                value.apply_to_policy(&mut policy, ra_paths::PathConvention::native())?;
                Ok(Arc::new(policy))
            })
            .transpose()?;
        if let Some(managed_deny_read_policy) = managed_deny_read_policy.as_ref() {
            effective_file_system_sandbox_policy
                .preserve_deny_read_restrictions_from(managed_deny_read_policy);
        }
        let effective_file_system_sandbox_policy = effective_file_system_sandbox_policy
            .with_additional_readable_roots(resolved_cwd.as_path(), &helper_readable_roots);
        let effective_permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
            effective_permission_profile.enforcement(),
            &effective_file_system_sandbox_policy,
            effective_network_sandbox_policy,
        );
        constrained_permission_profile
            .value
            .set(effective_permission_profile)
            .map_err(std::io::Error::from)?;
        if let Some(Sourced {
            source: requirement_source,
            ..
        }) = filesystem_requirements.as_ref()
            && let Some(managed_file_system_policy) = managed_deny_read_policy.as_ref()
        {
            let cwd = PathUri::from_abs_path(&resolved_cwd);
            let user_home_dir = PathUri::from_host_native_path("~").ok();
            let temporary_directories = std::env::var_os("TMPDIR")
                .filter(|path| !path.is_empty())
                .and_then(|path| AbsolutePathBuf::from_absolute_path(PathBuf::from(path)).ok())
                .map(PathUri::from)
                .into_iter()
                .collect::<Vec<_>>();
            let context = FileSystemSandboxPolicyContext {
                cwd: &cwd,
                workspace_roots: std::slice::from_ref(&cwd),
                user_home_dir: user_home_dir.as_ref(),
                temporary_directories: Some(&temporary_directories),
            };
            let validator = DenyReadValidator::new(managed_file_system_policy, &context)
                .map_err(std::io::Error::other)?;
            let requirement_source = requirement_source.clone();
            constrained_permission_profile
                .value
                .add_validator(move |permission_profile| {
                    let mut file_system_policy = permission_profile.file_system_sandbox_policy();
                    // Preserve the native conversion boundary before the shared URI checks.
                    // Mandatory entries are Deny entries, so their identity stays unchanged.
                    file_system_policy.entries.retain_mut(|entry| {
                        if entry.access.can_read()
                            && let FileSystemPath::Path { path } = &mut entry.path
                        {
                            let Ok(native_path) = path.to_abs_path() else {
                                return false;
                            };
                            *path = PathUri::from(native_path);
                        }
                        true
                    });
                    let context = FileSystemSandboxPolicyContext {
                        cwd: &cwd,
                        workspace_roots: std::slice::from_ref(&cwd),
                        user_home_dir: user_home_dir.as_ref(),
                        temporary_directories: Some(&temporary_directories),
                    };
                    validator.validate(&file_system_policy, &context).map_err(|violation| {
                        let candidate = match violation {
                            DenyReadViolation::MissingRequiredDeny => "missing managed deny".to_string(),
                            DenyReadViolation::ReadablePath(path) => path.to_abs_path().map_or_else(
                                |_| path.to_string(),
                                |path| path.to_string_lossy().into_owned(),
                            ),
                        };
                        ConstraintError::InvalidValue {
                            field_name: "permissions.filesystem",
                            candidate,
                            allowed: "all managed deny_read restrictions".to_string(),
                            requirement_source: requirement_source.clone(),
                        }
                    })
                })
                .map_err(std::io::Error::from)?;
        }

        let permission_profile_state = PermissionProfileState::from_constrained_active_profile(
            constrained_permission_profile.value,
            active_permission_profile,
            profile_workspace_roots,
        )
        .map_err(std::io::Error::from)?;
        let otel = otel::resolve_config(cfg.otel.unwrap_or_default(), &mut startup_warnings);
        let mut config = Self {
            plugins: cfg.plugins,
            prefer_mxc,
            model,
            daybreak_enabled: cfg.daybreak.unwrap_or(false),
            service_tier,
            review_model,
            model_context_window: cfg.model_context_window,
            model_auto_compact_token_limit: cfg.model_auto_compact_token_limit,
            model_auto_compact_token_limit_scope: cfg
                .model_auto_compact_token_limit_scope
                .unwrap_or_default(),
            model_post_turn_compact_threshold_percent: cfg
                .model_post_turn_compact_threshold_percent
                .unwrap_or_default(),
            async_compaction_threshold_percent,
            model_provider_id,
            model_provider,
            cwd: resolved_cwd,
            workspace_roots: workspace_roots.clone(),
            workspace_roots_explicit,
            startup_warnings,
            permissions: Permissions {
                approval_policy: constrained_approval_policy.value,
                permission_profile_state,
                managed_deny_read_policy,
                workspace_roots,
                network,
                allow_login_shell,
                shell_environment_policy,
                windows_sandbox_mode,
                windows_sandbox_type,
            },
            explicit_permission_profile_mode,
            custom_permission_profiles,
            approvals_reviewer: constrained_approvals_reviewer.value(),
            enforce_residency: enforce_residency.value,
            notify: cfg.notify,
            base_instructions,
            base_instructions_provenance,
            personality,
            developer_instructions,
            compact_prompt,
            include_permissions_instructions,
            include_apps_instructions,
            include_collaboration_mode_instructions,
            include_skill_instructions,
            skill_max_context_tokens,
            cloud_skill_enabled,
            orchestrator_mcp_enabled,
            include_environment_context,
            include_environment_context_time,
            mcp_servers,
            non_prefixed_mcp_tool_servers,
            mcp_enterprise_managed_auth,
            // The config.toml omits "_mode" because it's a config file. However, "_mode"
            // is important in code to differentiate the mode from the store implementation.
            mcp_oauth_credentials_store_mode: resolve_mcp_oauth_credentials_store_mode(
                cfg.mcp_oauth_credentials_store.unwrap_or_default(), env!("CARGO_PKG_VERSION"),
            ),
            mcp_oauth_callback_port: cfg.mcp_oauth_callback_port,
            mcp_oauth_callback_url: cfg.mcp_oauth_callback_url.clone(),
            mcp_optional_startup_grace: cfg
                .mcp_optional_startup_grace_ms
                .map_or(DEFAULT_OPTIONAL_MCP_STARTUP_GRACE, Duration::from_millis),
            model_providers,
            project_doc_max_bytes: cfg.project_doc_max_bytes.unwrap_or(AGENTS_MD_MAX_BYTES),
            project_doc_fallback_filenames: cfg
                .project_doc_fallback_filenames
                .unwrap_or_default()
                .into_iter()
                .filter_map(|name| {
                    let trimmed = name.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                })
                .collect(),
            tool_output_token_limit: cfg.tool_output_token_limit,
            agents_enabled,
            agent_max_threads,
            agent_default_subagent_model,
            agent_default_subagent_reasoning_effort,
            agent_max_depth,
            agent_roles,
            max_goal_token_budget: cfg
                .goals
                .as_ref()
                .and_then(|goals| goals.max_goal_token_budget)
                .map(|max_goal_token_budget| {
                    i64::try_from(max_goal_token_budget.get()).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "goals.max_goal_token_budget exceeds the maximum supported token budget",
                        )
                    })
                })
                .transpose()?,
            memories: memories_config,
            agent_interrupt_message_enabled,
            codex_home,
            sqlite: SqliteConfig::from_sqlite_home(sqlite_home),
            log_dir,
            config_layer_stack,
            application_network_policy: ra_http::NetworkPolicy::default(),
            history,
            ephemeral: ephemeral.unwrap_or_default(),
            extra_config: None,
            bypass_hook_trust,
            file_opener: cfg.file_opener.unwrap_or(UriBasedFileOpener::VsCode),
            codex_self_exe,
            codex_linux_sandbox_exe,
            main_execve_wrapper_exe: None,
            zsh_path: None,

            hide_agent_reasoning: cfg.hide_agent_reasoning.unwrap_or(false),
            show_raw_agent_reasoning: cfg
                .show_raw_agent_reasoning
                .or(show_raw_agent_reasoning)
                .unwrap_or(false),
            guardian_policy_config,
            guardian_extra_policy,
            guardian_policy_template,
            guardian_transcript_mode,
            guardian_conversation_history_prompt,
            guardian_conversation_history_max_output_tokens,
            guardian_circuit_break_action: cfg
                .auto_review
                .as_ref()
                .and_then(|auto_review| auto_review.circuit_break_action)
                .unwrap_or_default(),
            model_reasoning_effort: cfg.model_reasoning_effort,
            plan_mode_reasoning_effort: cfg.plan_mode_reasoning_effort,
            model_reasoning_summary: cfg.model_reasoning_summary,
            model_catalog,
            model_verbosity: cfg.model_verbosity,
            respect_system_proxy,
            chatgpt_base_url: cfg.chatgpt_base_url.unwrap_or_else(|| "https://chatgpt.com/backend-api/".to_string()),
            apps_mcp_product_sku: cfg.apps_mcp_product_sku.clone(),
            responses_api_metadata: cfg.responses_api_metadata.unwrap_or_default(),
            realtime_audio: cfg
                .audio
                .map_or_else(RealtimeAudioConfig::default, |audio| RealtimeAudioConfig {
                    microphone: audio.microphone,
                    microphone_channel: audio.microphone_channel,
                    speaker: audio.speaker,
                }),
            experimental_realtime_ws_base_url: cfg.experimental_realtime_ws_base_url,
            experimental_realtime_webrtc_call_base_url: cfg
                .experimental_realtime_webrtc_call_base_url,
            experimental_realtime_ws_model: cfg.experimental_realtime_ws_model,
            realtime: cfg
                .realtime
                .map_or_else(RealtimeConfig::default, |realtime| {
                    let defaults = RealtimeConfig::default();
                    RealtimeConfig {
                        version: realtime.version.unwrap_or(defaults.version),
                        session_type: realtime.session_type.unwrap_or(defaults.session_type),
                        transport: realtime.transport.unwrap_or(defaults.transport),
                        voice: realtime.voice,
                    }
                }),
            experimental_realtime_ws_backend_prompt: cfg.experimental_realtime_ws_backend_prompt,
            experimental_realtime_ws_startup_context: cfg.experimental_realtime_ws_startup_context,
            experimental_realtime_start_instructions: cfg.experimental_realtime_start_instructions,
            experimental_thread_store: thread_store_config(cfg.experimental_thread_store),
            web_search_mode: constrained_web_search_mode.value,
            web_search_config,
            experimental_request_user_input_enabled,
            update_plan_enabled,
            tool_registry,
            code_mode,
            background_terminal_max_timeout,
            thread_unload_delay,
            ghost_snapshot,
            multi_agent_v2,
            token_budget,
            token_budget_startup_config: None,
            rollout_budget,
            current_time_reminder,
            sleep_tool_mode,
            features,
            runtime_feature_defaults: BTreeMap::new(),
            suppress_unstable_features_warning: cfg
                .suppress_unstable_features_warning
                .unwrap_or(false),
            active_project,
            notices,
            check_for_update_on_startup,
            disable_paste_burst: cfg
                .tui
                .as_ref()
                .and_then(|tui| tui.disable_paste_burst)
                .or(cfg.disable_paste_burst)
                .unwrap_or(false),
            analytics_enabled: cfg.analytics.as_ref().and_then(|a| a.enabled),
            feedback_enabled: cfg
                .feedback
                .as_ref()
                .and_then(|feedback| feedback.enabled)
                .unwrap_or(true),
            tool_suggest,
            tui_notifications: cfg
                .tui
                .as_ref()
                .map(|t| t.notification_settings.clone())
                .unwrap_or_default(),
            animations: cfg.tui.as_ref().is_none_or(|t| t.animations),
            tui_effects: cfg.tui.as_ref().map(|t| t.effects).unwrap_or_default(),
            tui_rendering: cfg.tui.as_ref().map(|t| t.rendering).unwrap_or_default(),
            show_tooltips: cfg.tui.as_ref().is_none_or(|t| t.show_tooltips),
            tui_show_server_version_notice: cfg
                .tui
                .as_ref()
                .is_none_or(|t| t.show_server_version_notice),
            tui_auto_recap: cfg.tui.as_ref().is_none_or(|t| t.auto_recap),
            model_availability_nux: cfg
                .tui
                .as_ref()
                .map(|t| t.model_availability_nux.clone())
                .unwrap_or_default(),
            tui_question_esc_back: cfg.tui.as_ref().is_none_or(|t| t.question_esc_back),
            tui_vim_mode_default: cfg
                .tui
                .as_ref()
                .is_some_and(|t| t.vim_mode_default),
            tui_raw_output_mode: cfg
                .tui
                .as_ref()
                .is_some_and(|t| t.raw_output_mode),
            tui_fullscreen_transcript: cfg
                .tui
                .as_ref()
                .is_none_or(|tui| tui.fullscreen_transcript),
            tui_mouse_scroll_speed: cfg.tui.as_ref().and_then(|tui| tui.mouse_scroll_speed),
            tui_copy_on_select: cfg
                .tui
                .as_ref()
                .map(|tui| tui.copy_on_select)
                .unwrap_or_default(),
            tui_copy_on_select_clear_selection: cfg
                .tui
                .as_ref()
                .is_some_and(|tui| tui.copy_on_select_clear_selection),
            tui_right_click_paste: cfg
                .tui
                .as_ref()
                .map(|tui| tui.right_click_paste)
                .unwrap_or_default(),
            tui_alternate_screen: cfg
                .tui
                .as_ref()
                .map(|t| t.alternate_screen)
                .unwrap_or_default(),
            tui_status_line: cfg.tui.as_ref().and_then(|t| t.status_line.clone()),
            tui_status_line_use_colors: cfg
                .tui
                .as_ref()
                .is_none_or(|t| t.status_line_use_colors),
            tui_terminal_title: cfg.tui.as_ref().and_then(|t| t.terminal_title.clone()),
            tui_theme: cfg.tui.as_ref().and_then(|t| t.theme.clone()),
            tui_pet: cfg.tui.as_ref().and_then(|t| t.pet.clone()),
            tui_pet_anchor: cfg
                .tui
                .as_ref()
                .map(|t| t.pet_anchor)
                .unwrap_or_default(),
            tui_session_picker_view: cfg
                .tui
                .as_ref()
                .and_then(|t| t.session_picker_view)
                .unwrap_or_default(),
            tui_agents_overview_grouping: cfg.tui.as_ref().map(|t| t.agents_overview_grouping).unwrap_or_default(),
            tui_resume_cwd: cfg.tui.as_ref().and_then(|t| t.resume_cwd),
            terminal_resize_reflow,
            tui_keymap: cfg
                .tui
                .as_ref()
                .map(|t| t.keymap.clone())
                .unwrap_or_default(),
            otel,
        };
        if let Some(message) = config.credential_masking_warning(config.permissions.permission_profile()) {
            tracing::warn!("{message}");
            config.startup_warnings.push(message.to_string());
        }
        Ok(config)
        })
        .await
    }

    /// If `path` is `Some`, attempts to read the file at the given path and
    /// returns its contents as a trimmed `String`. If the file is empty, or
    /// is `Some` but cannot be read, returns an `Err`.
    async fn try_read_non_empty_file(
        fs: &dyn ExecutorFileSystem,
        path: Option<&AbsolutePathBuf>,
        context: &str,
    ) -> std::io::Result<Option<String>> {
        let Some(path) = path else {
            return Ok(None);
        };

        let path_uri = PathUri::from_abs_path(path);
        let contents = fs
            .read_file_text(&path_uri, ReadFileOptions::default(), /*sandbox*/ None)
            .await
            .map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("failed to read {context} {}: {e}", path.display()),
                )
            })?;

        let s = contents.trim().to_string();
        if s.is_empty() {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{context} is empty: {}", path.display()),
            ))
        } else {
            Ok(Some(s))
        }
    }

    pub fn set_windows_sandbox_enabled(&mut self, value: bool) {
        self.permissions.windows_sandbox_mode = if value {
            Some(WindowsSandboxModeToml::Unelevated)
        } else if matches!(
            self.permissions.windows_sandbox_mode,
            Some(WindowsSandboxModeToml::Unelevated)
        ) {
            None
        } else {
            self.permissions.windows_sandbox_mode
        };
    }

    pub fn set_windows_elevated_sandbox_enabled(&mut self, value: bool) {
        self.permissions.windows_sandbox_mode = if value {
            Some(WindowsSandboxModeToml::Elevated)
        } else if matches!(
            self.permissions.windows_sandbox_mode,
            Some(WindowsSandboxModeToml::Elevated)
        ) {
            None
        } else {
            self.permissions.windows_sandbox_mode
        };
    }

    #[must_use]
    pub fn managed_network_requirements_enabled(&self) -> bool {
        !matches!(
            self.permissions.permission_profile(),
            PermissionProfile::Disabled
        ) && self
            .config_layer_stack
            .requirements_toml()
            .network
            .is_some()
    }

    /// Resolves a named permission profile from effective config and managed requirements.
    pub fn resolve_permission_profile(
        &self,
        profile_name: &str,
    ) -> std::io::Result<PermissionProfileToml> {
        let cfg: ConfigToml = self
            .config_layer_stack
            .effective_config()
            .try_into()
            .map_err(|err| {
                std::io::Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "failed to read effective config for selected permission profile: {err}"
                    ),
                )
            })?;
        let permissions = merge_managed_permission_profiles(
            cfg.permissions.as_ref(),
            self.config_layer_stack.requirements_toml(),
        )?
        .unwrap_or_default();

        ra_config::permission_profiles::resolve_permission_profile(&permissions, profile_name)
    }

    pub fn network_proxy_spec_for_active_permission_profile(
        &self,
        active_permission_profile: &ActivePermissionProfile,
        permission_profile: &PermissionProfile,
    ) -> std::io::Result<Option<NetworkProxySpec>> {
        let profile_allows_network_proxy =
            profile_allows_configured_network_proxy(permission_profile);
        let cfg: ConfigToml = self
            .config_layer_stack
            .effective_config()
            .try_into()
            .map_err(|err| {
                std::io::Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "failed to read effective config for selected permission profile: {err}"
                    ),
                )
            })?;
        let configured_network_proxy_config = if profile_allows_network_proxy {
            let permissions = merge_managed_permission_profiles(
                cfg.permissions.as_ref(),
                self.config_layer_stack.requirements_toml(),
            )?;
            network_proxy_config_for_profile_selection(
                permissions.as_ref(),
                active_permission_profile.id.as_str(),
            )?
        } else {
            NetworkProxyConfig::default()
        };
        let prepared_network = PreparedNetworkConfig::from_inputs(NetworkConfigInputs {
            configured_proxy: configured_network_proxy_config,
            feature_enabled: profile_allows_network_proxy
                && self.features.enabled(Feature::NetworkProxy),
            features: cfg.features.as_ref(),
            candidate_permission_profile: permission_profile,
            credential_broker_base_url: cfg.openai_base_url.as_deref(),
        });
        build_network_proxy_spec(
            prepared_network.configured_proxy,
            self.config_layer_stack.requirements().network.clone(),
            permission_profile,
            &self.permissions.shell_environment_policy.r#set,
            &self.features,
            cfg.features.as_ref(),
        )
    }

    pub(crate) fn credential_masking_warning(
        &self,
        permission_profile: &PermissionProfile,
    ) -> Option<&'static str> {
        if !self.features.enabled(Feature::CredentialMasking) {
            return None;
        }
        // The session bypasses the configured proxy for unsandboxed execution,
        // even when managed requirements keep the proxy spec enabled.
        if matches!(permission_profile, PermissionProfile::Disabled) {
            return Some(
                "Credential masking is inactive because sandboxing is disabled. Commands will receive unmasked credentials. Select a sandboxed permission profile to use credential masking.",
            );
        }
        if self
            .permissions
            .network
            .as_ref()
            .is_some_and(NetworkProxySpec::credential_broker_enabled)
        {
            return None;
        }
        if self
            .permissions
            .network
            .as_ref()
            .is_some_and(NetworkProxySpec::enabled)
        {
            Some(
                "Credential masking is inactive because shell environment overrides contain conflicting case-insensitive provider keys. Commands will receive unmasked credentials. Remove the conflicting entries from `shell_environment_policy.set`.",
            )
        } else {
            Some(
                "Credential masking is inactive because the network proxy is disabled. Commands will receive unmasked credentials. Enable `features.network_proxy` or configure managed network requirements to use credential masking.",
            )
        }
    }

    #[must_use]
    pub fn bundled_skills_enabled(&self) -> bool {
        ra_config::bundled_skills_enabled_from_stack(&self.config_layer_stack)
    }

    /// Returns whether effective requirements allow selecting a concrete profile.
    #[must_use]
    pub fn is_permission_profile_allowed(
        &self,
        profile_id: &str,
        permission_profile: &PermissionProfile,
    ) -> bool {
        permission_profile_is_allowed(&self.config_layer_stack, profile_id, permission_profile)
    }
}

fn guardian_policy_config_from_requirements(
    requirements_toml: &ConfigRequirementsToml,
) -> Option<String> {
    normalize_guardian_policy_config(requirements_toml.guardian_policy_config.as_deref())
}

fn normalize_guardian_policy_config(value: Option<&str>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

/// Returns the path to the Codex configuration directory, which can be
/// specified by the `RA_HOME` environment variable. If not set, defaults to
/// `~/.rusty-agent`.
///
/// - If `RA_HOME` is set, the value must exist and be a directory. The
///   value will be canonicalized and this function will Err otherwise.
/// - If `RA_HOME` is not set, this function does not verify that the
pub(super) fn apply_requirement_constrained_value<T>(
    field_name: &'static str,
    configured_value: T,
    constrained_value: &mut ConstrainedWithSource<T>,
    startup_warnings: &mut Vec<String>,
) -> std::io::Result<bool>
where
    T: Clone + std::fmt::Debug + Send + Sync,
{
    if let Err(err) = constrained_value.set(configured_value) {
        let fallback_value = constrained_value.get().clone();
        tracing::warn!(
            error = %err,
            ?fallback_value,
            requirement_source = ?constrained_value.source,
            "configured value is disallowed by requirements; falling back to required value for {field_name}"
        );
        let message = format!(
            "Configured value for `{field_name}` is disallowed by requirements; falling back to required value {fallback_value:?}. Details: {err}"
        );
        startup_warnings.push(message);

        constrained_value.set(fallback_value).map_err(|fallback_err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "configured value for `{field_name}` is disallowed by requirements ({err}); fallback to a requirement-compliant value also failed ({fallback_err})"
                ),
            )
        })?;
        return Ok(true);
    }

    Ok(false)
}

fn resolve_tool_suggest_config(
    config_toml: &ConfigToml,
    config_layer_stack: &ConfigLayerStack,
) -> ToolSuggestConfig {
    resolve_tool_suggest_config_from_config(config_toml.tool_suggest.as_ref(), config_layer_stack)
}

fn resolve_tool_suggest_config_from_config(
    tool_suggest: Option<&ToolSuggestConfig>,
    config_layer_stack: &ConfigLayerStack,
) -> ToolSuggestConfig {
    let discoverables = tool_suggest
        .into_iter()
        .flat_map(|tool_suggest| tool_suggest.discoverables.iter())
        .filter_map(|discoverable| {
            let trimmed = discoverable.id.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(ToolSuggestDiscoverable {
                    kind: discoverable.kind,
                    id: trimmed.to_string(),
                })
            }
        })
        .collect();
    let mut seen_disabled_tools = HashSet::new();
    let mut disabled_tools = Vec::new();
    let mut add_disabled_tool = |disabled_tool: ToolSuggestDisabledTool| {
        if let Some(disabled_tool) = disabled_tool.normalized()
            && seen_disabled_tools.insert(disabled_tool.clone())
        {
            disabled_tools.push(disabled_tool);
        }
    };

    let mut layers = config_layer_stack.layers_low_to_high().peekable();
    if layers.peek().is_none() {
        for disabled_tool in tool_suggest
            .into_iter()
            .flat_map(|tool_suggest| tool_suggest.disabled_tools.iter().cloned())
        {
            add_disabled_tool(disabled_tool);
        }
    } else {
        for layer in layers {
            let Some(tool_suggest) = layer
                .config
                .get("tool_suggest")
                .cloned()
                .and_then(|value| value.try_into::<ToolSuggestConfig>().ok())
            else {
                continue;
            };
            for disabled_tool in tool_suggest.disabled_tools {
                add_disabled_tool(disabled_tool);
            }
        }
    }

    ToolSuggestConfig {
        discoverables,
        disabled_tools,
    }
}

fn thread_store_config(thread_store: Option<ThreadStoreToml>) -> ThreadStoreConfig {
    match thread_store {
        Some(ThreadStoreToml::Local {}) => ThreadStoreConfig::Local,
        Some(ThreadStoreToml::InMemory { id }) => ThreadStoreConfig::InMemory { id },
        None => ThreadStoreConfig::Local,
    }
}

fn load_catalog_json(path: &AbsolutePathBuf) -> std::io::Result<ModelsResponse> {
    let file_contents = std::fs::read_to_string(path)?;
    let catalog = serde_json::from_str::<ModelsResponse>(&file_contents).map_err(|err| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "failed to parse model_catalog_json path `{}` as JSON: {err}",
                path.display()
            ),
        )
    })?;
    if catalog.models.is_empty() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "model_catalog_json path `{}` must contain at least one model",
                path.display()
            ),
        ));
    }
    Ok(catalog)
}

fn load_model_catalog(
    model_catalog_json: Option<AbsolutePathBuf>,
) -> std::io::Result<Option<ModelsResponse>> {
    model_catalog_json
        .map(|path| load_catalog_json(&path))
        .transpose()
}

impl Config {
    #[must_use]
    pub fn legacy_sandbox_policy(&self) -> SandboxPolicy {
        self.permissions.legacy_sandbox_policy(self.cwd.as_path())
    }
}

async fn config_toml_from_layers(layers: &ConfigLayerStack) -> std::io::Result<ConfigToml> {
    // The loader resolves paths relative to each layer's file before deserialization.
    match layers.effective_config().try_into() {
        Ok(config_toml) => Ok(config_toml),
        Err(err) => {
            if let Some(config_error) =
                ra_config::diagnostics::first_layer_config_error::<ConfigToml>(
                    layers,
                    ra_config::CONFIG_TOML_FILE,
                )
                .await
            {
                return Err(ra_config::diagnostics::io_error_from_config_error(
                    std::io::ErrorKind::InvalidData,
                    config_error,
                    Some(err),
                ));
            }
            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err))
        }
    }
}

impl Config {
    #[must_use]
    pub fn prefix_mcp_tool_names(&self) -> bool {
        !self.features.enabled(Feature::NonPrefixedMcpToolNames)
            || self.non_prefixed_mcp_tool_servers.is_some()
    }
    #[must_use]
    pub fn sqlite_config(&self) -> &SqliteConfig {
        &self.sqlite
    }
    #[must_use]
    pub fn http_client_factory(&self) -> ra_http::HttpClientFactory {
        let policy = if self.respect_system_proxy {
            ra_http::OutboundProxyPolicy::RespectSystemProxy
        } else {
            ra_http::OutboundProxyPolicy::ReqwestDefault
        };
        let factory = ra_http::HttpClientFactory::new(policy)
            .with_network_policy(self.application_network_policy.clone());
        if !self.respect_system_proxy && self.features.enabled(Feature::SystemProxyFallback) {
            factory.with_system_proxy_fallback()
        } else {
            factory
        }
    }
    #[must_use]
    pub fn plugins_config_input(&self) -> crate::plugins::PluginsConfigInput {
        let mut input = crate::plugins::PluginsConfigInput::new(
            self.config_layer_stack.clone(),
            self.plugins.clone(),
            self.model_provider_id.clone(),
            self.features.enabled(Feature::Plugins),
            self.http_client_factory(),
        );
        input.chatgpt_base_url.clone_from(&self.chatgpt_base_url);
        input
    }
    #[must_use]
    pub fn plugin_runtime_config(&self) -> crate::plugins::PluginRuntimeConfig {
        crate::plugins::PluginRuntimeConfig {
            config_layer_stack: self.config_layer_stack.clone(),
            features: self.features.clone(),
            plugins: self.plugins.clone(),
            model_provider_id: self.model_provider_id.clone(),
            chatgpt_base_url: self.chatgpt_base_url.clone(),
            mcp_servers: self.mcp_servers.clone(),
            mcp_enterprise_managed_auth: self.mcp_enterprise_managed_auth.clone(),
            startup_warnings: self.startup_warnings.clone(),
            legacy_notify_argv: self.notify.clone(),
            bypass_hook_trust: self.bypass_hook_trust,
            cwd: self.cwd.clone(),
            home: self.codex_home.clone(),
            http_client_factory: self.http_client_factory(),
        }
    }
}

const LOCAL_DEV_BUILD_VERSION: &str = "0.0.0";
fn resolve_mcp_oauth_credentials_store_mode(
    configured: OAuthCredentialsStoreMode,
    package_version: &str,
) -> OAuthCredentialsStoreMode {
    match (package_version, configured) {
        (
            LOCAL_DEV_BUILD_VERSION,
            OAuthCredentialsStoreMode::Keyring | OAuthCredentialsStoreMode::Auto,
        ) => OAuthCredentialsStoreMode::File,
        (_, mode) => mode,
    }
}

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use super::{OAuthCredentialsStoreMode, Permissions};
    #[must_use]
    pub fn resolve_mcp_oauth_credentials_store_mode(
        configured: OAuthCredentialsStoreMode,
        package_version: &str,
    ) -> OAuthCredentialsStoreMode {
        super::resolve_mcp_oauth_credentials_store_mode(configured, package_version)
    }
    #[must_use]
    pub fn resolve_multi_agent_v2_config(
        config: &ra_config::ConfigToml,
    ) -> super::MultiAgentV2Config {
        super::resolve_multi_agent_v2_config(config)
    }
    #[must_use]
    pub fn config_package_version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    #[must_use]
    pub fn native_mxc_available() -> bool {
        super::windows_mxc_available()
    }
    #[must_use]
    pub fn saved_permission_profile_snapshot(
        permissions: &Permissions,
    ) -> super::PermissionProfileSnapshot {
        permissions.permission_profile_state().snapshot()
    }
    pub fn inherit_permission_profile_state(target: &mut Permissions, source: &Permissions) {
        target.set_permission_profile_state(source.permission_profile_state().clone());
    }
}

impl Config {
    #[must_use]
    pub fn effective_agent_max_threads(
        &self,
        multi_agent_version: MultiAgentVersion,
    ) -> Option<usize> {
        match multi_agent_version {
            MultiAgentVersion::V2 => Some(
                self.multi_agent_v2
                    .max_concurrent_threads_per_session
                    .saturating_sub(1),
            ),
            MultiAgentVersion::Disabled | MultiAgentVersion::V1 => {
                self.agent_max_threads.or(DEFAULT_AGENT_MAX_THREADS)
            }
        }
    }
    #[must_use]
    pub fn effective_workspace_roots(&self) -> Vec<PathUri> {
        let mut workspace_roots = self
            .workspace_roots
            .iter()
            .map(PathUri::from_abs_path)
            .collect::<Vec<_>>();
        workspace_roots.extend(
            self.permissions
                .profile_workspace_roots()
                .iter()
                .map(|root| root.as_uri().clone()),
        );
        // Preserve spelling changes even when Windows path comparison folds case.
        let mut seen = HashSet::new();
        workspace_roots.retain(|root| seen.insert(root.to_string()));
        workspace_roots
    }
}
const DEFAULT_AGENT_MAX_THREADS: Option<usize> = Some(6);

use ra_http::{HttpClientFactory, OutboundProxyPolicy};
pub fn resolve_bootstrap_http_client_factory(
    cfg: &ConfigToml,
    feature_requirements: Option<&Sourced<ra_config::FeatureRequirementsToml>>,
) -> std::io::Result<HttpClientFactory> {
    let configured_features = Features::from_sources(
        FeatureConfigSource {
            features: cfg.features.as_ref(),
            experimental_use_unified_exec_tool: cfg.experimental_use_unified_exec_tool,
        },
        FeatureConfigSource::default(),
        FeatureOverrides::default(),
    );
    let features =
        ManagedFeatures::from_configured(configured_features, feature_requirements.cloned())?;
    let outbound_proxy_policy = if features.enabled(Feature::RespectSystemProxy) {
        OutboundProxyPolicy::RespectSystemProxy
    } else {
        OutboundProxyPolicy::ReqwestDefault
    };
    let mut factory = HttpClientFactory::new(outbound_proxy_policy);
    if outbound_proxy_policy == OutboundProxyPolicy::ReqwestDefault
        && features.enabled(Feature::SystemProxyFallback)
    {
        factory = factory.with_system_proxy_fallback();
    }
    Ok(factory)
}
