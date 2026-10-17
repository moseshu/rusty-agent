// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The methods that load threads and read them: Codex's `thread/start`, `thread/resume`,
//! `thread/fork`, `thread/read` and `thread/loaded/list` (`protocol/v2/thread.rs`).
//!
//! Every member Codex's parameters have is read, so a client sending one is told what happens to
//! it rather than having it dropped. Those this framework acts on are the model and service tier,
//! the approval and sandbox policy and named permissions, the thread's ephemerality and source,
//! the tools its client provides, the fork boundaries and the history to include. Codex's
//! deprecated `personality` and
//! `multiAgentMode`, which it ignores, and its test-only `mockExperimentalField` are ignored here
//! too. Any other member, when sent, is refused as `<method>.<member> is not supported`: a working
//! directory, workspace roots, environments, instructions, configuration overrides, projects,
//! raw events and the like belong to Codex's product configuration, which a host here does not
//! have.
//!
//! A thread's effective settings are an open record in the answers, as in
//! `thread/settings/updated`: the model, its provider, service tier and reasoning effort come from
//! the thread, and the product's fields — working directory, approval and sandbox policy, named
//! permissions — from the host's settings adapter, when it has one.

#![allow(
    clippy::option_option,
    reason = "Codex distinguishes omitted, null and supplied service tiers"
)]

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use super::{
    ApprovalsReviewer, AskForApproval, DynamicToolSpec, SandboxPolicy, SortDirection, Thread,
    ThreadHistoryMode, Turn, TurnItemsView, settings::double_option,
};

/// Codex's coarse sandbox selection for a new, resumed or forked thread: `SandboxMode`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Reads anywhere, writes nowhere, no network.
    ReadOnly,
    /// Writes in the workspace, no network.
    WorkspaceWrite,
    /// Unrestricted.
    DangerFullAccess,
}

impl SandboxMode {
    /// The policy the mode selects, with Codex's defaults for what the mode leaves open: no
    /// network and no writable roots beyond the workspace.
    #[must_use]
    pub fn to_policy(self) -> SandboxPolicy {
        match self {
            Self::ReadOnly => SandboxPolicy::ReadOnly {
                network_access: false,
            },
            Self::WorkspaceWrite => SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            Self::DangerFullAccess => SandboxPolicy::DangerFullAccess,
        }
    }
}

/// Why a thread was started: Codex's `ThreadStartSource`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadStartSource {
    /// A new conversation.
    Startup,
    /// A conversation the user cleared; it starts empty, as a new one does.
    Clear,
}

/// The policy members `thread/start`, `thread/resume` and `thread/fork` share.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadPolicyFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) approval_policy: Option<AskForApproval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) approvals_reviewer: Option<ApprovalsReviewer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sandbox: Option<SandboxMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permissions: Option<String>,
}

impl ThreadPolicyFields {
    fn granular_approval_policy(&self) -> bool {
        matches!(self.approval_policy, Some(AskForApproval::Granular { .. }))
    }
}

/// The parameters of `thread/start`: Codex's `ThreadStartParams`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model_provider: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) allow_provider_model_fallback: bool,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) service_tier: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_workspace_roots: Option<Value>,
    #[serde(flatten)]
    pub(crate) policy: ThreadPolicyFields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) service_name: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base_instructions: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) developer_instructions: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) personality: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) multi_agent_mode: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ephemeral: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) history_mode: Option<ThreadHistoryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_start_source: Option<ThreadStartSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thread_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) daybreak_enabled: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) environments: Option<Value>,
    #[serde(
        default,
        deserialize_with = "super::dynamic_tools::deserialize_dynamic_tool_specs",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) dynamic_tools: Option<Vec<DynamicToolSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) selected_capability_roots: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mock_experimental_field: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) experimental_raw_events: bool,
}

impl ThreadStartParams {
    /// Starts a thread with the host's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs its turns on `model`.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Keeps no rollout of it, or keeps one.
    #[must_use]
    pub const fn with_ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = Some(ephemeral);
        self
    }

    /// Classifies where the thread came from, such as `user`.
    #[must_use]
    pub fn with_thread_source(mut self, source: impl Into<String>) -> Self {
        self.thread_source = Some(source.into());
        self
    }

    /// Prompts for approval as `policy` says.
    #[must_use]
    pub const fn with_approval_policy(mut self, policy: AskForApproval) -> Self {
        self.policy.approval_policy = Some(policy);
        self
    }

    /// Confines its tools as `mode` says.
    #[must_use]
    pub const fn with_sandbox(mut self, mode: SandboxMode) -> Self {
        self.policy.sandbox = Some(mode);
        self
    }

    /// Confines its tools as the named permissions profile says.
    #[must_use]
    pub fn with_permissions(mut self, profile: impl Into<String>) -> Self {
        self.policy.permissions = Some(profile.into());
        self
    }

    /// Gives the thread the tools the client provides.
    #[must_use]
    pub fn with_dynamic_tools(mut self, tools: impl IntoIterator<Item = DynamicToolSpec>) -> Self {
        self.dynamic_tools = Some(tools.into_iter().collect());
        self
    }

    /// The tools the client provides, read into the canonical shape.
    #[must_use]
    pub fn dynamic_tools(&self) -> Option<&[DynamicToolSpec]> {
        self.dynamic_tools.as_deref()
    }

    /// The first member sent that needs the experimental API, in the order Codex declares them.
    #[must_use]
    pub fn experimental_reason(&self) -> Option<&'static str> {
        [
            (
                "thread/start.allowProviderModelFallback",
                self.allow_provider_model_fallback,
            ),
            (
                "thread/start.runtimeWorkspaceRoots",
                self.runtime_workspace_roots.is_some(),
            ),
            (
                "askForApproval.granular",
                self.policy.granular_approval_policy(),
            ),
            (
                "thread/start.permissions",
                self.policy.permissions.is_some(),
            ),
            (
                "thread/start.multiAgentMode",
                self.multi_agent_mode.is_some(),
            ),
            ("thread/start.historyMode", self.history_mode.is_some()),
            ("thread/start.projectId", self.project_id.is_some()),
            (
                "thread/start.daybreakEnabled",
                self.daybreak_enabled.is_some(),
            ),
            ("thread/start.environments", self.environments.is_some()),
            ("thread/start.dynamicTools", self.dynamic_tools.is_some()),
            (
                "thread/start.selectedCapabilityRoots",
                self.selected_capability_roots.is_some(),
            ),
            (
                "thread/start.mockExperimentalField",
                self.mock_experimental_field.is_some(),
            ),
            (
                "thread/start.experimentalRawEvents",
                self.experimental_raw_events,
            ),
        ]
        .into_iter()
        .find_map(|(reason, sent)| sent.then_some(reason))
    }

    /// The first member sent that sets something this framework does not have, by its wire name.
    #[must_use]
    pub fn unsupported_field(&self) -> Option<&'static str> {
        [
            ("modelProvider", self.model_provider.is_some()),
            (
                "allowProviderModelFallback",
                self.allow_provider_model_fallback,
            ),
            ("cwd", self.cwd.is_some()),
            (
                "runtimeWorkspaceRoots",
                self.runtime_workspace_roots.is_some(),
            ),
            ("config", self.config.is_some()),
            ("serviceName", self.service_name.is_some()),
            ("baseInstructions", self.base_instructions.is_some()),
            (
                "developerInstructions",
                self.developer_instructions.is_some(),
            ),
            ("projectId", self.project_id.is_some()),
            ("daybreakEnabled", self.daybreak_enabled.is_some()),
            ("environments", self.environments.is_some()),
            (
                "selectedCapabilityRoots",
                self.selected_capability_roots.is_some(),
            ),
            ("experimentalRawEvents", self.experimental_raw_events),
        ]
        .into_iter()
        .find_map(|(name, sent)| sent.then_some(name))
    }
}

/// The answer to `thread/start`, `thread/resume` and `thread/fork` besides the thread: the
/// thread's effective settings, an open record (see the module's documentation).
pub type ThreadSettingsRecord = Map<String, Value>;

/// The answer to `thread/start`: Codex's `ThreadStartResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
    thread: Thread,
    #[serde(flatten)]
    settings: ThreadSettingsRecord,
}

impl ThreadStartResponse {
    pub(crate) const fn new(thread: Thread, settings: ThreadSettingsRecord) -> Self {
        Self { thread, settings }
    }

    /// The thread started.
    #[must_use]
    pub const fn thread(&self) -> &Thread {
        &self.thread
    }

    /// Its effective settings.
    #[must_use]
    pub const fn settings(&self) -> &ThreadSettingsRecord {
        &self.settings
    }
}

/// Reads an empty path as no path: Codex's `deserialize_empty_path_as_none`.
fn empty_path_as_none<'de, D: Deserializer<'de>>(de: D) -> Result<Option<PathBuf>, D::Error> {
    Ok(Option::<PathBuf>::deserialize(de)?.filter(|path| !path.as_os_str().is_empty()))
}

/// The page of turns `thread/resume` includes when asked: Codex's
/// `ThreadResumeInitialTurnsPageParams`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeInitialTurnsPageParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sort_direction: Option<SortDirection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) items_view: Option<TurnItemsView>,
}

impl ThreadResumeInitialTurnsPageParams {
    /// The newest page, of the default size, with summary items.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Holds at most `limit` turns.
    #[must_use]
    pub const fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Pages in `sort_direction`.
    #[must_use]
    pub const fn with_sort_direction(mut self, sort_direction: SortDirection) -> Self {
        self.sort_direction = Some(sort_direction);
        self
    }

    /// Includes `items_view` of each turn's items.
    #[must_use]
    pub const fn with_items_view(mut self, items_view: TurnItemsView) -> Self {
        self.items_view = Some(items_view);
        self
    }
}

/// A page of a thread's turns: Codex's `TurnsPage`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnsPage {
    data: Vec<Turn>,
    next_cursor: Option<String>,
    backwards_cursor: Option<String>,
}

impl TurnsPage {
    pub(crate) const fn new(
        data: Vec<Turn>,
        next_cursor: Option<String>,
        backwards_cursor: Option<String>,
    ) -> Self {
        Self {
            data,
            next_cursor,
            backwards_cursor,
        }
    }

    /// The turns.
    #[must_use]
    pub fn data(&self) -> &[Turn] {
        &self.data
    }

    /// Where the next page starts, if there is one.
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }

    /// Where a page in the other direction starts.
    #[must_use]
    pub fn backwards_cursor(&self) -> Option<&str> {
        self.backwards_cursor.as_deref()
    }
}

/// The parameters of `thread/resume`: Codex's `ThreadResumeParams`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub(crate) thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) history: Option<Value>,
    #[serde(
        default,
        deserialize_with = "empty_path_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model_provider: Option<String>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) service_tier: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_workspace_roots: Option<Value>,
    #[serde(flatten)]
    pub(crate) policy: ThreadPolicyFields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base_instructions: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) developer_instructions: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) personality: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) exclude_turns: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) initial_turns_page: Option<ThreadResumeInitialTurnsPageParams>,
}

impl ThreadResumeParams {
    /// Resumes `thread_id`, including its turns.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            ..Self::default()
        }
    }

    /// Runs its turns on `model` from now on.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Leaves the turns out of the answer.
    #[must_use]
    pub const fn excluding_turns(mut self) -> Self {
        self.exclude_turns = true;
        self
    }

    /// Includes a page of turns in the answer.
    #[must_use]
    pub fn with_initial_turns_page(mut self, page: ThreadResumeInitialTurnsPageParams) -> Self {
        self.initial_turns_page = Some(page);
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The first member sent that needs the experimental API, in the order Codex declares them.
    #[must_use]
    pub fn experimental_reason(&self) -> Option<&'static str> {
        [
            ("thread/resume.history", self.history.is_some()),
            ("thread/resume.path", self.path.is_some()),
            (
                "thread/resume.runtimeWorkspaceRoots",
                self.runtime_workspace_roots.is_some(),
            ),
            (
                "askForApproval.granular",
                self.policy.granular_approval_policy(),
            ),
            (
                "thread/resume.permissions",
                self.policy.permissions.is_some(),
            ),
            (
                "thread/resume.initialTurnsPage",
                self.initial_turns_page.is_some(),
            ),
        ]
        .into_iter()
        .find_map(|(reason, sent)| sent.then_some(reason))
    }

    /// The first member sent that sets something this framework does not have, by its wire name.
    #[must_use]
    pub fn unsupported_field(&self) -> Option<&'static str> {
        [
            ("history", self.history.is_some()),
            ("path", self.path.is_some()),
            ("modelProvider", self.model_provider.is_some()),
            ("cwd", self.cwd.is_some()),
            (
                "runtimeWorkspaceRoots",
                self.runtime_workspace_roots.is_some(),
            ),
            ("config", self.config.is_some()),
            ("baseInstructions", self.base_instructions.is_some()),
            (
                "developerInstructions",
                self.developer_instructions.is_some(),
            ),
        ]
        .into_iter()
        .find_map(|(name, sent)| sent.then_some(name))
    }
}

/// The answer to `thread/resume`: Codex's `ThreadResumeResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeResponse {
    thread: Thread,
    #[serde(default)]
    initial_turns_page: Option<TurnsPage>,
    #[serde(default)]
    turns_backwards_cursor: Option<String>,
    #[serde(default)]
    items_backwards_cursor: Option<String>,
    #[serde(flatten)]
    settings: ThreadSettingsRecord,
}

impl ThreadResumeResponse {
    pub(crate) const fn new(
        thread: Thread,
        initial_turns_page: Option<TurnsPage>,
        settings: ThreadSettingsRecord,
    ) -> Self {
        Self {
            thread,
            initial_turns_page,
            turns_backwards_cursor: None,
            items_backwards_cursor: None,
            settings,
        }
    }

    /// The thread resumed.
    #[must_use]
    pub const fn thread(&self) -> &Thread {
        &self.thread
    }

    /// The page of turns asked for, if one was.
    #[must_use]
    pub const fn initial_turns_page(&self) -> Option<&TurnsPage> {
        self.initial_turns_page.as_ref()
    }

    /// Its effective settings.
    #[must_use]
    pub const fn settings(&self) -> &ThreadSettingsRecord {
        &self.settings
    }
}

/// The parameters of `thread/fork`: Codex's `ThreadForkParams`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadForkParams {
    pub(crate) thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) before_turn_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "empty_path_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model_provider: Option<String>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) service_tier: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_workspace_roots: Option<Value>,
    #[serde(flatten)]
    pub(crate) policy: ThreadPolicyFields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base_instructions: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) developer_instructions: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) ephemeral: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thread_source: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) exclude_turns: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) defer_goal_continuation: bool,
}

impl ThreadForkParams {
    /// Forks `thread_id` as it stands, including the turns in the answer.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            ..Self::default()
        }
    }

    /// Keeps the turns up to and including `turn_id`.
    #[must_use]
    pub fn through_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.last_turn_id = Some(turn_id.into());
        self
    }

    /// Keeps the turns before `turn_id`.
    #[must_use]
    pub fn before_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.before_turn_id = Some(turn_id.into());
        self
    }

    /// Keeps no rollout of the fork.
    #[must_use]
    pub const fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    /// Classifies where the fork came from, such as `user`.
    #[must_use]
    pub fn with_thread_source(mut self, source: impl Into<String>) -> Self {
        self.thread_source = Some(source.into());
        self
    }

    /// Leaves the turns out of the answer.
    #[must_use]
    pub const fn excluding_turns(mut self) -> Self {
        self.exclude_turns = true;
        self
    }

    /// The thread forked.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The first member sent that needs the experimental API, in the order Codex declares them.
    #[must_use]
    pub fn experimental_reason(&self) -> Option<&'static str> {
        [
            ("thread/fork.beforeTurnId", self.before_turn_id.is_some()),
            ("thread/fork.path", self.path.is_some()),
            (
                "thread/fork.runtimeWorkspaceRoots",
                self.runtime_workspace_roots.is_some(),
            ),
            (
                "askForApproval.granular",
                self.policy.granular_approval_policy(),
            ),
            ("thread/fork.permissions", self.policy.permissions.is_some()),
            (
                "thread/fork.deferGoalContinuation",
                self.defer_goal_continuation,
            ),
        ]
        .into_iter()
        .find_map(|(reason, sent)| sent.then_some(reason))
    }

    /// The first member sent that sets something this framework does not have, by its wire name.
    #[must_use]
    pub fn unsupported_field(&self) -> Option<&'static str> {
        [
            ("path", self.path.is_some()),
            ("modelProvider", self.model_provider.is_some()),
            ("cwd", self.cwd.is_some()),
            (
                "runtimeWorkspaceRoots",
                self.runtime_workspace_roots.is_some(),
            ),
            ("config", self.config.is_some()),
            ("baseInstructions", self.base_instructions.is_some()),
            (
                "developerInstructions",
                self.developer_instructions.is_some(),
            ),
            ("deferGoalContinuation", self.defer_goal_continuation),
        ]
        .into_iter()
        .find_map(|(name, sent)| sent.then_some(name))
    }
}

/// The answer to `thread/fork`: Codex's `ThreadForkResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadForkResponse {
    thread: Thread,
    #[serde(flatten)]
    settings: ThreadSettingsRecord,
}

impl ThreadForkResponse {
    pub(crate) const fn new(thread: Thread, settings: ThreadSettingsRecord) -> Self {
        Self { thread, settings }
    }

    /// The new thread.
    #[must_use]
    pub const fn thread(&self) -> &Thread {
        &self.thread
    }

    /// Its effective settings.
    #[must_use]
    pub const fn settings(&self) -> &ThreadSettingsRecord {
        &self.settings
    }
}

/// The parameters of `thread/read`: Codex's `ThreadReadParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadParams {
    pub(crate) thread_id: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) include_turns: bool,
}

impl ThreadReadParams {
    /// Reads `thread_id` without its turns.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            include_turns: false,
        }
    }

    /// Includes its turns, with all their items.
    #[must_use]
    pub const fn including_turns(mut self) -> Self {
        self.include_turns = true;
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

/// The answer to `thread/read`: Codex's `ThreadReadResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadResponse {
    thread: Thread,
    /// Local receipt. Null means this thread's durable read state is unavailable.
    read_state: Option<super::ThreadReadState>,
}

impl ThreadReadResponse {
    pub(crate) const fn new(thread: Thread, read_state: Option<super::ThreadReadState>) -> Self {
        Self { thread, read_state }
    }

    /// The durable local receipt, or none when unavailable for this thread.
    #[must_use]
    pub const fn read_state(&self) -> Option<&super::ThreadReadState> {
        self.read_state.as_ref()
    }

    /// The thread.
    #[must_use]
    pub const fn thread(&self) -> &Thread {
        &self.thread
    }
}

/// The parameters of `thread/loaded/list`: Codex's `ThreadLoadedListParams`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadLoadedListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<u32>,
}

impl ThreadLoadedListParams {
    /// Every loaded thread.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts after `cursor`.
    #[must_use]
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// Holds at most `limit` ids.
    #[must_use]
    pub const fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// The answer to `thread/loaded/list`: Codex's `ThreadLoadedListResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadLoadedListResponse {
    data: Vec<String>,
    next_cursor: Option<String>,
}

impl ThreadLoadedListResponse {
    pub(crate) const fn new(data: Vec<String>, next_cursor: Option<String>) -> Self {
        Self { data, next_cursor }
    }

    /// The ids of the loaded threads on this page.
    #[must_use]
    pub fn data(&self) -> &[String] {
        &self.data
    }

    /// Where the next page starts, if there is one.
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }
}
