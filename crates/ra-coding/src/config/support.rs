// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_debug_formatting,
    clippy::items_after_statements,
    reason = "Preserve the upstream configuration surface, branch order and diagnostic spelling."
)]
#![allow(missing_docs, reason = "Preserve upstream configuration records.")]
use ra_config::{ConfigToml, Feature, Sourced, WindowsSandboxLevel, WindowsSandboxModeToml};
use ra_filesystem::legacy::SandboxPolicy;
use ra_filesystem::{
    models::PermissionProfile,
    permissions::{FileSystemSandboxPolicy, NetworkSandboxPolicy},
};
use ra_paths::AbsolutePathBuf;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseInstructionsProvenance {
    Custom,
    Model { model: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtraConfig {}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteConfig {
    sqlite_home: AbsolutePathBuf,
}
impl SqliteConfig {
    #[must_use]
    pub fn from_sqlite_home(sqlite_home: AbsolutePathBuf) -> Self {
        Self { sqlite_home }
    }
    #[must_use]
    pub fn home(&self) -> &Path {
        self.sqlite_home.as_path()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiAgentVersion {
    Disabled,
    #[default]
    V1,
    V2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CyberAccessProgram {
    Standard,
    DaybreakBlue,
    DaybreakRed,
}
pub(super) const DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS: u64 = 300_000;
pub(super) const MIN_EMPTY_YIELD_TIME_MS: u64 = 5_000;
pub(super) const DEFAULT_OPTIONAL_MCP_STARTUP_GRACE: std::time::Duration =
    std::time::Duration::from_secs(1);
pub(super) fn resolve_sqlite_home_env(resolved_cwd: &Path) -> Option<AbsolutePathBuf> {
    let raw = std::env::var("RA_SQLITE_HOME").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(AbsolutePathBuf::resolve_path_against_base(
        trimmed,
        resolved_cwd,
    ))
}
pub(super) fn active_arg0_helper_dir(
    codex_home: &Path,
    path_env: &OsStr,
) -> Option<AbsolutePathBuf> {
    let arg0_root = codex_home
        .join("tmp")
        .join(format!("arg0-{}", std::env::consts::OS));
    let legacy_root = codex_home.join("tmp").join("arg0");
    std::env::split_paths(path_env)
        .find(|path| {
            (path.parent() == Some(arg0_root.as_path())
                || path.parent() == Some(legacy_root.as_path()))
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("codex-arg0"))
        })
        .and_then(|path| AbsolutePathBuf::from_absolute_path(path).ok())
}

pub(super) fn bundled_token_budget_reminder() -> &'static str {
    concat!(
        "Your context window is nearly exhausted (only {n_remaining} tokens remaining) and will be automatically reset for you soon. ",
        "Once reset, message items in current context window will be cleared in the new window, but notes and history items will be persistent across windows."
    )
}

use ra_config::features::{Features, FeaturesToml};
use ra_filesystem::sandbox::SandboxType;
use std::ffi::OsStr;
pub(super) fn windows_sandbox_level_for_legacy_checks(
    sandbox_type: SandboxType,
    sandbox_level: WindowsSandboxLevel,
) -> WindowsSandboxLevel {
    if sandbox_type == SandboxType::WindowsMxc {
        WindowsSandboxLevel::RestrictedToken
    } else {
        sandbox_level
    }
}

pub(super) trait WindowsSandboxLevelExt {
    fn from_features(features: &Features) -> WindowsSandboxLevel;
}

impl WindowsSandboxLevelExt for WindowsSandboxLevel {
    fn from_features(features: &Features) -> WindowsSandboxLevel {
        if features.enabled(Feature::WindowsSandboxElevated) {
            return WindowsSandboxLevel::Elevated;
        }
        if features.enabled(Feature::WindowsSandbox) {
            WindowsSandboxLevel::RestrictedToken
        } else {
            WindowsSandboxLevel::Disabled
        }
    }
}

pub(super) fn resolve_windows_sandbox_mode(cfg: &ConfigToml) -> Option<WindowsSandboxModeToml> {
    cfg.windows
        .as_ref()
        .and_then(|windows| windows.sandbox)
        .or_else(|| legacy_windows_sandbox_mode(cfg.features.as_ref()))
}

pub(super) fn legacy_windows_sandbox_mode(
    features: Option<&FeaturesToml>,
) -> Option<WindowsSandboxModeToml> {
    let entries = features.map(FeaturesToml::entries)?;
    legacy_windows_sandbox_mode_from_entries(&entries)
}

pub(super) fn legacy_windows_sandbox_mode_from_entries(
    entries: &BTreeMap<String, bool>,
) -> Option<WindowsSandboxModeToml> {
    if entries
        .get(Feature::WindowsSandboxElevated.key())
        .copied()
        .unwrap_or(false)
    {
        return Some(WindowsSandboxModeToml::Elevated);
    }
    if entries
        .get(Feature::WindowsSandbox.key())
        .copied()
        .unwrap_or(false)
        || entries
            .get("enable_experimental_windows_sandbox")
            .copied()
            .unwrap_or(false)
    {
        Some(WindowsSandboxModeToml::Unelevated)
    } else {
        None
    }
}

pub(super) fn normalize_for_native_workdir(path: impl AsRef<Path>) -> PathBuf {
    normalize_for_native_workdir_with_flag(path.as_ref().to_path_buf(), cfg!(windows))
}
fn normalize_for_native_workdir_with_flag(path: PathBuf, is_windows: bool) -> PathBuf {
    if is_windows {
        dunce::simplified(&path).to_path_buf()
    } else {
        path
    }
}
pub(crate) const INSTALLATION_ID_KEY: &str = "installation_id";
pub(crate) const SESSION_ID_KEY: &str = "session_id";
pub(crate) const THREAD_ID_KEY: &str = "thread_id";
pub(crate) const AGENT_NAME_KEY: &str = "agent_name";
pub(crate) const TURN_ID_KEY: &str = "turn_id";
pub(crate) const WINDOW_ID_KEY: &str = "window_id";
pub(crate) const WINDOW_NUMBER_KEY: &str = "window_number";
pub(crate) const CONTEXT_WINDOW_ID_KEY: &str = "context_window_id";
pub(crate) const REQUEST_KIND_KEY: &str = "request_kind";
pub(crate) const COMPACTION_KEY: &str = "compaction";
pub(crate) const LEGACY_CODE_MODE_TOOL_NAMES_KEY: &str = "code_mode_tool_names";
pub(crate) const TOOL_NAMESPACES_INFO_KEY: &str = "tool_namespaces_info";
pub(crate) const TURN_STARTED_AT_UNIX_MS_KEY: &str = "turn_started_at_unix_ms";
pub(crate) const HISTORY_INGEST_REQUESTED_KEY: &str = "history_ingest_requested";
pub(crate) const ANALYTICS_ENABLED_KEY: &str = "analytics_enabled";
pub(crate) const HISTORY_INITIALIZATION_KEY: &str = "history_initialization";
pub(crate) const MCP_ATTRIBUTION_CLIENT_METADATA_KEY: &str = "mcp_attribution";
pub(crate) const FORKED_FROM_THREAD_ID_KEY: &str = "forked_from_thread_id";
pub(crate) const FORKED_FROM_ORDINAL_EXCLUSIVE_KEY: &str = "forked_from_ordinal_exclusive";
pub(crate) const PARENT_THREAD_ID_KEY: &str = "parent_thread_id";
pub(crate) const PARENT_TURN_ID_KEY: &str = "parent_turn_id";
pub(crate) const ROOT_TURN_ID_KEY: &str = "root_turn_id";
pub(crate) const SUBAGENT_KIND_KEY: &str = "subagent_kind";
pub(crate) const THREAD_SOURCE_KEY: &str = "thread_source";
pub(crate) const TURN_TRIGGER_KEY: &str = "turn_trigger";
pub(crate) const SANDBOX_KEY: &str = "sandbox";
pub(crate) const SANDBOX_MODE_KEY: &str = "sandbox_mode";
pub(crate) const AUTO_REVIEW_ENABLED_KEY: &str = "auto_review_enabled";
pub(crate) const NODE_REPL_AUTO_REVIEW_REQUIRED_KEY: &str = "node_repl_auto_review_required";
pub(crate) const NODE_REPL_DISABLED_KEY: &str = "node_repl_disabled";
pub(crate) const WORKSPACES_KEY: &str = "workspaces";
const MAX_EXTRA_METADATA_ENTRIES: usize = 16;
const MAX_EXTRA_METADATA_KEY_BYTES: usize = 64;
pub(crate) const MAX_EXTRA_METADATA_VALUE_BYTES: usize = 128;
const RESERVED_METADATA_KEYS: &[&str] = &[
    "guardian_credits_requested",
    MCP_ATTRIBUTION_CLIENT_METADATA_KEY,
    INSTALLATION_ID_KEY,
    X_CODEX_INSTALLATION_ID_HEADER,
    SESSION_ID_KEY,
    THREAD_ID_KEY,
    AGENT_NAME_KEY,
    TURN_ID_KEY,
    WINDOW_ID_KEY,
    WINDOW_NUMBER_KEY,
    CONTEXT_WINDOW_ID_KEY,
    X_CODEX_WINDOW_ID_HEADER,
    X_CODEX_TURN_METADATA_HEADER,
    X_CODEX_PARENT_THREAD_ID_HEADER,
    X_OPENAI_SUBAGENT_HEADER,
    REQUEST_KIND_KEY,
    COMPACTION_KEY,
    LEGACY_CODE_MODE_TOOL_NAMES_KEY,
    TOOL_NAMESPACES_INFO_KEY,
    TURN_STARTED_AT_UNIX_MS_KEY,
    HISTORY_INGEST_REQUESTED_KEY,
    ANALYTICS_ENABLED_KEY,
    HISTORY_INITIALIZATION_KEY,
    FORKED_FROM_THREAD_ID_KEY,
    FORKED_FROM_ORDINAL_EXCLUSIVE_KEY,
    PARENT_THREAD_ID_KEY,
    PARENT_TURN_ID_KEY,
    ROOT_TURN_ID_KEY,
    SUBAGENT_KIND_KEY,
    THREAD_SOURCE_KEY,
    TURN_TRIGGER_KEY,
    SANDBOX_KEY,
    SANDBOX_MODE_KEY,
    AUTO_REVIEW_ENABLED_KEY,
    NODE_REPL_AUTO_REVIEW_REQUIRED_KEY,
    NODE_REPL_DISABLED_KEY,
    WORKSPACES_KEY,
];
const BACKWARD_COMPATIBLE_RESERVED_METADATA_KEYS: &[&str] = &[
    WINDOW_NUMBER_KEY,
    FORKED_FROM_ORDINAL_EXCLUSIVE_KEY,
    ANALYTICS_ENABLED_KEY,
];
pub(super) fn validate_extra_metadata<'a>(
    extra: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Result<(), &'static str> {
    let mut count = 0;
    for (key, value) in extra {
        count += 1;
        if count > MAX_EXTRA_METADATA_ENTRIES {
            return Err("responses_api_metadata may contain at most 16 entries");
        }
        if key.len() > MAX_EXTRA_METADATA_KEY_BYTES || !valid_extra_metadata_key(key) {
            return Err("responses_api_metadata keys must be short ASCII identifiers");
        }
        if RESERVED_METADATA_KEYS.contains(&key.as_str())
            && !BACKWARD_COMPATIBLE_RESERVED_METADATA_KEYS.contains(&key.as_str())
        {
            return Err("responses_api_metadata contains a reserved key");
        }
        if value.len() > MAX_EXTRA_METADATA_VALUE_BYTES {
            return Err("responses_api_metadata values may contain at most 128 bytes");
        }
    }
    Ok(())
}
fn valid_extra_metadata_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}
pub(super) fn compatibility_sandbox_policy_for_permission_profile(
    permissions: &PermissionProfile,
    cwd: &Path,
) -> SandboxPolicy {
    permissions
        .to_legacy_sandbox_policy(cwd)
        .unwrap_or_else(|_| {
            let (file_system_policy, network_policy) = permissions.to_runtime_permissions();
            compatibility_workspace_write_policy(file_system_policy, network_policy, cwd)
        })
}
fn compatibility_workspace_write_policy(
    file_system_policy: FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    cwd: &Path,
) -> SandboxPolicy {
    let cwd_abs = AbsolutePathBuf::from_absolute_path(cwd).ok();
    let writable_roots = file_system_policy
        .get_writable_roots_with_cwd(cwd)
        .into_iter()
        .map(|root| root.root)
        .filter(|root| cwd_abs.as_ref().map(AbsolutePathBuf::as_path) != Some(root.as_path()))
        .collect();
    let tmpdir_writable = std::env::var_os("TMPDIR")
        .filter(|tmpdir| !tmpdir.is_empty())
        .and_then(|tmpdir| {
            AbsolutePathBuf::from_absolute_path(std::path::PathBuf::from(tmpdir)).ok()
        })
        .is_some_and(|tmpdir| {
            file_system_policy.can_write_local_path_with_cwd(tmpdir.as_path(), cwd)
        });
    let slash_tmp = Path::new("/tmp");
    let slash_tmp_writable = slash_tmp.is_absolute()
        && slash_tmp.is_dir()
        && file_system_policy.can_write_local_path_with_cwd(slash_tmp, cwd);

    SandboxPolicy::WorkspaceWrite {
        writable_roots,
        network_access: network_policy.is_enabled(),
        exclude_tmpdir_env_var: !tmpdir_writable,
        exclude_slash_tmp: !slash_tmp_writable,
    }
}

pub(super) fn validate_managed_developer_instructions(
    requirement: Option<&Sourced<String>>,
) -> std::io::Result<()> {
    let Some(requirement) = requirement else {
        return Ok(());
    };
    const NOTICE: &str = "These managed developer instructions replace all previously provided managed developer instructions.";
    let rendered_bytes = requirement.value.len().saturating_add(
        format!(
            "<managed_developer_instructions>\n{NOTICE}\n\n\n</managed_developer_instructions>"
        )
        .len(),
    );
    if rendered_bytes > 40_000 {
        let estimated_tokens = rendered_bytes.div_ceil(4);
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "`additional_developer_instructions` from {} exceeds the model-context limit of 10000 estimated tokens ({estimated_tokens} including context markers)",
                requirement.source
            ),
        ));
    }
    Ok(())
}
// Native MXC availability is supplied by the sandbox implementation when that backend is ported.
pub(super) fn windows_mxc_available() -> bool {
    false
}

const X_CODEX_INSTALLATION_ID_HEADER: &str = "x-codex-installation-id";
const X_CODEX_WINDOW_ID_HEADER: &str = "x-codex-window-id";
const X_CODEX_TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";
const X_CODEX_PARENT_THREAD_ID_HEADER: &str = "x-codex-parent-thread-id";
const X_OPENAI_SUBAGENT_HEADER: &str = "x-openai-subagent";
