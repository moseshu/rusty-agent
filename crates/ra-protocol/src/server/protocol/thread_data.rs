//! The thread a client sees and the notifications about it: Codex's `Thread`, `ThreadStatus` and
//! their kin (`protocol/v2/thread_data.rs`, `protocol/v2/thread.rs`).
//!
//! The shape is Codex's, member for member. What a member says that this framework does not keep
//! is sent as Codex sends it for a thread without it: no environments, extra data, section,
//! project or Daybreak choice, and the history mode is always `legacy`, since a store
//! here keeps the rollout rather than a paginated projection.
//!
//! # Differences from Codex
//!
//! - `cwd` is `null` for a thread whose working directory nobody recorded. Codex always records
//!   one, since its sessions run in a directory; a thread here belongs to its host, which may have
//!   none.
//! - `source` is `subAgent` for a spawned agent's thread and `unknown` otherwise: Codex records the
//!   source its embedding product names, which nothing here names.
//! - `recencyAt` is `null`: Codex's state database orders threads by a recency of its own, which no
//!   store here keeps.
//! - `reasoningEffort` is the effort's label — `low` through `max` — as `thread/settings/updated`
//!   reports it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Turn;

/// The method of the notification that a thread was started, resumed into a new thread or
/// forked.
pub const THREAD_STARTED_METHOD: &str = "thread/started";

/// The method of the notification that a loaded thread's status changed.
pub const THREAD_STATUS_CHANGED_METHOD: &str = "thread/status/changed";

/// Which persisted history contract a thread uses: Codex's `ThreadHistoryMode`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadHistoryMode {
    /// The thread's rollout is its history, rebuilt into turns when read.
    #[default]
    Legacy,
    /// The thread keeps a paginated item projection; no store here keeps one.
    Paginated,
}

/// What a waiting active thread waits for: Codex's `ThreadActiveFlag`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadActiveFlag {
    /// An approval the client was asked for.
    WaitingOnApproval,
    /// An answer from the user.
    WaitingOnUserInput,
}

/// What a thread is doing: Codex's `ThreadStatus`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadStatus {
    /// The thread is not loaded.
    NotLoaded,
    /// The thread is loaded and running nothing.
    Idle,
    /// The thread is loaded, running nothing, and its last turn hit an error.
    SystemError,
    /// The thread is running a turn, or waiting for the client.
    #[serde(rename_all = "camelCase")]
    Active {
        /// What it waits for, if anything.
        active_flags: Vec<ThreadActiveFlag>,
    },
}

impl ThreadStatus {
    /// Active, waiting for nothing.
    #[must_use]
    pub const fn active() -> Self {
        Self::Active {
            active_flags: Vec::new(),
        }
    }
}

/// A thread as a client sees it: Codex's `Thread`.
#[allow(clippy::struct_field_names, reason = "Codex's wire names")]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) environments: Option<Vec<Value>>,
    #[serde(default)]
    pub(crate) extra: Option<Value>,
    pub(crate) session_id: String,
    pub(crate) forked_from_id: Option<String>,
    pub(crate) parent_thread_id: Option<String>,
    pub(crate) preview: String,
    pub(crate) ephemeral: bool,
    #[serde(default)]
    pub(crate) section: Option<Value>,
    #[serde(default)]
    pub(crate) section_entered_at: Option<i64>,
    #[serde(default)]
    pub(crate) project_id: Option<String>,
    #[serde(default)]
    pub(crate) history_mode: ThreadHistoryMode,
    pub(crate) model_provider: String,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) recency_at: Option<i64>,
    pub(crate) status: ThreadStatus,
    pub(crate) path: Option<PathBuf>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) cli_version: String,
    pub(crate) originator: Option<String>,
    pub(crate) source: Value,
    pub(crate) can_accept_direct_input: Option<bool>,
    pub(crate) thread_source: Option<String>,
    pub(crate) agent_nickname: Option<String>,
    pub(crate) agent_role: Option<String>,
    pub(crate) git_info: Option<Value>,
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) daybreak_enabled: Option<bool>,
    pub(crate) turns: Vec<Turn>,
}

impl Thread {
    /// The thread's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The id shared by the threads of one session tree: the root thread's.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The thread it was forked from, if it is a fork.
    #[must_use]
    pub fn forked_from_id(&self) -> Option<&str> {
        self.forked_from_id.as_deref()
    }

    /// The thread whose agent spawned this one's, if it is a spawned agent's.
    #[must_use]
    pub fn parent_thread_id(&self) -> Option<&str> {
        self.parent_thread_id.as_deref()
    }

    /// Usually its first user message; empty when there is none.
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// Whether the thread keeps no rollout.
    #[must_use]
    pub const fn ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// The persisted history contract it uses.
    #[must_use]
    pub const fn history_mode(&self) -> ThreadHistoryMode {
        self.history_mode
    }

    /// The model provider it runs on, or last ran on.
    #[must_use]
    pub fn model_provider(&self) -> &str {
        &self.model_provider
    }

    /// The model it runs on when loaded, otherwise the last one recorded.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The reasoning effort it runs with when loaded, otherwise the last one recorded.
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    /// When it was created, in seconds since the Unix epoch.
    #[must_use]
    pub const fn created_at(&self) -> i64 {
        self.created_at
    }

    /// When it was last written to, in seconds since the Unix epoch.
    #[must_use]
    pub const fn updated_at(&self) -> i64 {
        self.updated_at
    }

    /// What it is doing.
    #[must_use]
    pub const fn status(&self) -> &ThreadStatus {
        &self.status
    }

    /// The file its rollout is kept in, if the store reads it from one.
    #[must_use]
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// The working directory recorded for it, if any.
    #[must_use]
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// The version of the program that created it.
    #[must_use]
    pub fn cli_version(&self) -> &str {
        &self.cli_version
    }

    /// Where the session came from: Codex's `SessionSource`.
    #[must_use]
    pub const fn source(&self) -> &Value {
        &self.source
    }

    /// Whether the server takes turn input for it directly; `None` when it is not loaded.
    #[must_use]
    pub const fn can_accept_direct_input(&self) -> Option<bool> {
        self.can_accept_direct_input
    }

    /// The client's classification of where it came from, such as `user`.
    #[must_use]
    pub fn thread_source(&self) -> Option<&str> {
        self.thread_source.as_deref()
    }

    /// The role of the spawned agent it belongs to, if it is a spawned agent's.
    #[must_use]
    pub fn agent_role(&self) -> Option<&str> {
        self.agent_role.as_deref()
    }

    /// Its user-facing title, if it was given one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Its turns, where the method fills them in.
    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }
}

/// That a thread was started, sent to every connection: Codex's `ThreadStartedNotification`. The
/// thread carries no turns.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartedNotification {
    thread: Thread,
}

impl ThreadStartedNotification {
    /// Codex's `thread_started_notification`, which drops the turns.
    pub(crate) fn new(mut thread: Thread) -> Self {
        thread.turns.clear();
        Self { thread }
    }

    /// The thread.
    #[must_use]
    pub const fn thread(&self) -> &Thread {
        &self.thread
    }
}

/// That a loaded thread's status changed, sent to every connection: Codex's
/// `ThreadStatusChangedNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStatusChangedNotification {
    thread_id: String,
    status: ThreadStatus,
}

impl ThreadStatusChangedNotification {
    pub(crate) fn new(thread_id: impl Into<String>, status: ThreadStatus) -> Self {
        Self {
            thread_id: thread_id.into(),
            status,
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Its status now.
    #[must_use]
    pub const fn status(&self) -> &ThreadStatus {
        &self.status
    }
}
