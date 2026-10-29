//! What a thread's turns are made of, and the notifications that report them as they happen:
//! Codex's `ThreadItem`, `item/started` and `item/completed` (`protocol/v2/item.rs`).
//!
//! The server builds the items a run here produces: the user's input, the agent's messages and
//! reasoning, context compaction, and — through the host's [`ThreadToolItems`] — the tool calls a
//! product shows, such as command executions and file changes. The other items of Codex's union
//! belong to machinery not ported, such as its hooks, plans, review mode, image generation and
//! multi-agent tools, and are added as that machinery is.
//!
//! [`ThreadToolItems`]: crate::server::ThreadToolItems

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{CommandAction, DynamicToolCallOutputContentItem, UserInput};

/// The method of the notification that an item started.
pub const ITEM_STARTED_METHOD: &str = "item/started";

/// The method of the notification that an item completed.
pub const ITEM_COMPLETED_METHOD: &str = "item/completed";

/// Whether an agent message is commentary or the turn's answer: Codex's `MessagePhase`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    /// Text around the work, which more may follow.
    Commentary,
    /// The turn's answer.
    FinalAnswer,
}

/// Where a command execution stands: Codex's `CommandExecutionStatus`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionStatus {
    /// It is running.
    InProgress,
    /// It finished successfully.
    Completed,
    /// It failed.
    Failed,
    /// It was not approved.
    Declined,
}

/// What started a command execution: Codex's `CommandExecutionSource`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionSource {
    /// The agent's tool call.
    #[default]
    Agent,
    /// The user's own shell command.
    UserShell,
    /// The start of a unified-exec session.
    UnifiedExecStartup,
    /// Input to a running unified-exec session.
    UnifiedExecInteraction,
}

/// Where applying a patch stands: Codex's `PatchApplyStatus`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PatchApplyStatus {
    /// It is being applied.
    InProgress,
    /// It was applied.
    Completed,
    /// It failed.
    Failed,
    /// It was not approved.
    Declined,
}

/// Where a call of a client's tool stands: Codex's `DynamicToolCallStatus`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DynamicToolCallStatus {
    /// The client is running it.
    InProgress,
    /// The client said it succeeded.
    Completed,
    /// The client said it failed, or no answer came.
    Failed,
}

/// What a patch does to one file: Codex's `PatchChangeKind`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PatchChangeKind {
    /// It creates the file.
    Add,
    /// It deletes the file.
    Delete,
    /// It changes the file, and moves it if `move_path` is set. The member keeps Codex's
    /// snake-case name.
    #[non_exhaustive]
    Update {
        /// Where the file moves to, if it does.
        move_path: Option<PathBuf>,
    },
}

impl PatchChangeKind {
    /// Changes the file in place, or moves it to `move_path` as well.
    #[must_use]
    pub const fn update(move_path: Option<PathBuf>) -> Self {
        Self::Update { move_path }
    }
}

/// A file a patch changes: Codex's `FileUpdateChange`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileUpdateChange {
    path: String,
    kind: PatchChangeKind,
    diff: String,
}

impl FileUpdateChange {
    /// The change `kind` to the file at `path`, shown as `diff`: the content of a file created or
    /// deleted, and the hunks of one changed.
    #[must_use]
    pub fn new(path: impl Into<String>, kind: PatchChangeKind, diff: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind,
            diff: diff.into(),
        }
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// What the patch does to it.
    #[must_use]
    pub const fn kind(&self) -> &PatchChangeKind {
        &self.kind
    }

    /// The change, for display.
    #[must_use]
    pub fn diff(&self) -> &str {
        &self.diff
    }
}

/// One thing a turn is made of: Codex's `ThreadItem`, for the items a run here produces.
///
/// Codex's agent message also carries memory citations, a delivery mode and questions for the
/// user, which its memory and asynchronous-input machinery fill; they are carried as the JSON a
/// newer server sends, and this server sends `null`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadItem {
    /// Input the user gave.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    UserMessage {
        /// Its id.
        id: String,
        /// The id the client gave the input, if it gave one.
        client_id: Option<String>,
        /// The input.
        content: Vec<UserInput>,
    },
    /// Continuation requested by a stop hook, attributed to each handler run.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    HookPrompt {
        /// History item identity.
        id: String,
        /// The handler prompts in execution order.
        fragments: Vec<super::hooks::HookPromptFragment>,
    },
    /// A message from the agent.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    AgentMessage {
        /// Its id.
        id: String,
        /// Its text.
        text: String,
        /// Whether it is commentary or the answer, if the model said.
        #[serde(default)]
        phase: Option<MessagePhase>,
        /// Codex's memory citation, as sent.
        #[serde(default)]
        memory_citation: Option<Value>,
        /// Codex's delivery mode, as sent.
        #[serde(default)]
        delivery: Option<Value>,
        /// Codex's questions for the user, as sent.
        #[serde(default)]
        questions: Option<Value>,
    },
    /// The model's reasoning.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    Reasoning {
        /// Its id.
        id: String,
        /// The summary the provider gave.
        #[serde(default)]
        summary: Vec<String>,
        /// The reasoning text, where the provider gives it.
        #[serde(default)]
        content: Vec<String>,
    },
    /// A command the agent ran.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    CommandExecution {
        /// Its id: the tool call's.
        id: String,
        /// The plugin the command belongs to, for Codex's plugin scripts.
        #[serde(default)]
        plugin_id: Option<String>,
        /// The plugin script it runs, for Codex's plugin scripts.
        #[serde(default)]
        script_path: Option<String>,
        /// The command line.
        command: String,
        /// The directory it runs in.
        cwd: String,
        /// The process session it runs in, while it can be reached.
        process_id: Option<String>,
        /// What started it.
        #[serde(default)]
        source: CommandExecutionSource,
        /// Where it stands.
        status: CommandExecutionStatus,
        /// What it does, parsed for display.
        command_actions: Vec<CommandAction>,
        /// Its output.
        aggregated_output: Option<String>,
        /// How it exited.
        exit_code: Option<i32>,
        /// How long it ran, in milliseconds.
        duration_ms: Option<i64>,
    },
    /// File changes the agent made.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    FileChange {
        /// Its id: the tool call's.
        id: String,
        /// The files changed.
        changes: Vec<FileUpdateChange>,
        /// Where applying them stands.
        status: PatchApplyStatus,
    },
    /// A call of a tool the client provides.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    DynamicToolCall {
        /// Its id: the tool call's.
        id: String,
        /// The namespace of the tool, if it has one.
        namespace: Option<String>,
        /// The tool.
        tool: String,
        /// The arguments, as the model wrote them.
        arguments: Value,
        /// Where it stands.
        status: DynamicToolCallStatus,
        /// What the tool returned, once it answered.
        content_items: Option<Vec<DynamicToolCallOutputContentItem>>,
        /// Whether the tool succeeded, once it answered.
        success: Option<bool>,
        /// How long the call took, in milliseconds.
        duration_ms: Option<i64>,
    },
    /// The thread's context was compacted.
    #[non_exhaustive]
    #[serde(rename_all = "camelCase")]
    ContextCompaction {
        /// Its id.
        id: String,
    },
}

impl ThreadItem {
    /// The user's input `content`.
    #[must_use]
    pub fn user_message(id: impl Into<String>, content: Vec<UserInput>) -> Self {
        Self::UserMessage {
            id: id.into(),
            client_id: None,
            content,
        }
    }

    /// The agent's message `text`, in `phase` if the model said.
    #[must_use]
    pub fn agent_message(
        id: impl Into<String>,
        text: impl Into<String>,
        phase: Option<MessagePhase>,
    ) -> Self {
        Self::AgentMessage {
            id: id.into(),
            text: text.into(),
            phase,
            memory_citation: None,
            delivery: None,
            questions: None,
        }
    }

    /// The model's reasoning: its `summary` and its `content`.
    #[must_use]
    pub fn reasoning(id: impl Into<String>, summary: Vec<String>, content: Vec<String>) -> Self {
        Self::Reasoning {
            id: id.into(),
            summary,
            content,
        }
    }

    /// The file `changes` of a patch, applied as far as `status` says.
    #[must_use]
    pub fn file_change(
        id: impl Into<String>,
        changes: Vec<FileUpdateChange>,
        status: PatchApplyStatus,
    ) -> Self {
        Self::FileChange {
            id: id.into(),
            changes,
            status,
        }
    }

    /// The thread's context was compacted.
    #[must_use]
    pub fn context_compaction(id: impl Into<String>) -> Self {
        Self::ContextCompaction { id: id.into() }
    }

    /// Its id: Codex's `ThreadItem::id`.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::UserMessage { id, .. }
            | Self::HookPrompt { id, .. }
            | Self::AgentMessage { id, .. }
            | Self::Reasoning { id, .. }
            | Self::CommandExecution { id, .. }
            | Self::FileChange { id, .. }
            | Self::DynamicToolCall { id, .. }
            | Self::ContextCompaction { id } => id,
        }
    }
}

/// A command execution, built up before it becomes a [`ThreadItem::CommandExecution`].
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandExecutionItem {
    id: String,
    command: String,
    cwd: String,
    process_id: Option<String>,
    source: CommandExecutionSource,
    status: CommandExecutionStatus,
    command_actions: Vec<CommandAction>,
    aggregated_output: Option<String>,
    exit_code: Option<i32>,
    duration_ms: Option<i64>,
}

impl CommandExecutionItem {
    /// The command line `command`, run by the tool call `id` in `cwd`, where `status` says.
    pub fn new(
        id: impl Into<String>,
        command: impl Into<String>,
        cwd: impl Into<String>,
        status: CommandExecutionStatus,
    ) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            cwd: cwd.into(),
            process_id: None,
            source: CommandExecutionSource::Agent,
            status,
            command_actions: Vec::new(),
            aggregated_output: None,
            exit_code: None,
            duration_ms: None,
        }
    }

    /// The process session it runs in.
    pub fn with_process_id(mut self, process_id: impl Into<String>) -> Self {
        self.process_id = Some(process_id.into());
        self
    }

    /// What started it.
    pub const fn with_source(mut self, source: CommandExecutionSource) -> Self {
        self.source = source;
        self
    }

    /// What it does, parsed for display.
    pub fn with_command_actions(mut self, command_actions: Vec<CommandAction>) -> Self {
        self.command_actions = command_actions;
        self
    }

    /// Its output.
    pub fn with_aggregated_output(mut self, output: impl Into<String>) -> Self {
        self.aggregated_output = Some(output.into());
        self
    }

    /// How it exited.
    pub const fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    /// How long it ran.
    pub const fn with_duration_ms(mut self, duration_ms: i64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }
}

impl From<CommandExecutionItem> for ThreadItem {
    fn from(item: CommandExecutionItem) -> Self {
        Self::CommandExecution {
            id: item.id,
            plugin_id: None,
            script_path: None,
            command: item.command,
            cwd: item.cwd,
            process_id: item.process_id,
            source: item.source,
            status: item.status,
            command_actions: item.command_actions,
            aggregated_output: item.aggregated_output,
            exit_code: item.exit_code,
            duration_ms: item.duration_ms,
        }
    }
}

/// A call of a client's tool, built up before it becomes a [`ThreadItem::DynamicToolCall`].
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicToolCallItem {
    id: String,
    namespace: Option<String>,
    tool: String,
    arguments: Value,
    status: DynamicToolCallStatus,
    content_items: Option<Vec<DynamicToolCallOutputContentItem>>,
    success: Option<bool>,
    duration_ms: Option<i64>,
}

impl DynamicToolCallItem {
    /// The call `id` of `tool` with `arguments`, where `status` says.
    pub fn new(
        id: impl Into<String>,
        tool: impl Into<String>,
        arguments: Value,
        status: DynamicToolCallStatus,
    ) -> Self {
        Self {
            id: id.into(),
            namespace: None,
            tool: tool.into(),
            arguments,
            status,
            content_items: None,
            success: None,
            duration_ms: None,
        }
    }

    /// The namespace of the tool.
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// What the tool returned, and whether it succeeded.
    pub fn with_response(
        mut self,
        content_items: Vec<DynamicToolCallOutputContentItem>,
        success: bool,
    ) -> Self {
        self.content_items = Some(content_items);
        self.success = Some(success);
        self
    }

    /// How long the call took.
    pub const fn with_duration_ms(mut self, duration_ms: i64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }
}

impl From<DynamicToolCallItem> for ThreadItem {
    fn from(item: DynamicToolCallItem) -> Self {
        Self::DynamicToolCall {
            id: item.id,
            namespace: item.namespace,
            tool: item.tool,
            arguments: item.arguments,
            status: item.status,
            content_items: item.content_items,
            success: item.success,
            duration_ms: item.duration_ms,
        }
    }
}

/// That an item started: Codex's `ItemStartedNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemStartedNotification {
    item: ThreadItem,
    thread_id: String,
    turn_id: String,
    started_at_ms: i64,
}

impl ItemStartedNotification {
    pub(crate) fn new(
        item: ThreadItem,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        started_at_ms: i64,
    ) -> Self {
        Self {
            item,
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            started_at_ms,
        }
    }

    /// The item, as it started.
    #[must_use]
    pub const fn item(&self) -> &ThreadItem {
        &self.item
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// When it started, in milliseconds since the Unix epoch.
    #[must_use]
    pub const fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }
}

/// That an item completed: Codex's `ItemCompletedNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemCompletedNotification {
    item: ThreadItem,
    thread_id: String,
    turn_id: String,
    completed_at_ms: i64,
}

impl ItemCompletedNotification {
    pub(crate) fn new(
        item: ThreadItem,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        completed_at_ms: i64,
    ) -> Self {
        Self {
            item,
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            completed_at_ms,
        }
    }

    /// The item, as it completed.
    #[must_use]
    pub const fn item(&self) -> &ThreadItem {
        &self.item
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// When it completed, in milliseconds since the Unix epoch.
    #[must_use]
    pub const fn completed_at_ms(&self) -> i64 {
        self.completed_at_ms
    }
}
