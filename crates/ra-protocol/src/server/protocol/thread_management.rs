//! Thread management payloads ported from Codex's `protocol/v2/thread.rs`.
//!
//! Git metadata is stored by the session layer. Project, section and Daybreak controls require
//! product state that this port does not keep and are explicitly refused when supplied.

use super::{SortDirection, Thread, settings::double_option};
use serde::{Deserialize, Serialize};

/// Archives a persistent thread and its spawned descendants.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadArchiveParams {
    pub(crate) thread_id: String,
}

impl ThreadArchiveParams {
    /// Targets `thread_id`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
        }
    }
    /// The thread to change.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

/// The empty successful answer to `thread/archive`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadArchiveResponse {}

/// Deletes a persistent thread and its spawned descendants.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDeleteParams {
    pub(crate) thread_id: String,
}

impl ThreadDeleteParams {
    /// Targets `thread_id`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
        }
    }
    /// The thread to change.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

/// The empty successful answer to `thread/delete`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadDeleteResponse {}

/// Restores an archived thread.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUnarchiveParams {
    pub(crate) thread_id: String,
}

impl ThreadUnarchiveParams {
    /// Targets `thread_id`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
        }
    }
    /// The thread to change.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

/// The broadcast that a thread was archived.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadArchivedNotification {
    /// The changed thread.
    pub thread_id: String,
}

/// The method of the archived broadcast.
pub const THREAD_ARCHIVED_METHOD: &str = "thread/archived";

/// The broadcast that a thread was deleted.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDeletedNotification {
    /// The changed thread.
    pub thread_id: String,
}

/// The method of the deleted broadcast.
pub const THREAD_DELETED_METHOD: &str = "thread/deleted";

/// The broadcast that a thread was unarchived.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUnarchivedNotification {
    /// The changed thread.
    pub thread_id: String,
}

/// The method of the unarchived broadcast.
pub const THREAD_UNARCHIVED_METHOD: &str = "thread/unarchived";

/// Changes the name shown for a persistent thread.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSetNameParams {
    pub(crate) thread_id: String,
    pub(crate) name: String,
}

impl ThreadSetNameParams {
    /// Names `thread_id` with `name`, trimmed by the server.
    #[must_use]
    pub fn new(thread_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            name: name.into(),
        }
    }
    /// The thread to rename.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

/// The empty successful answer to `thread/name/set`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSetNameResponse {}

/// The broadcast that a thread's name changed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadNameUpdatedNotification {
    /// The renamed thread.
    pub thread_id: String,
    /// Its new name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_name: Option<String>,
}

/// The method of the name change broadcast.
pub const THREAD_NAME_UPDATED_METHOD: &str = "thread/name/updated";

/// A partial Git metadata change; omitted, null and string mean keep, clear and replace.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::option_option, reason = "Codex's clearable Git fields")]
pub struct ThreadMetadataGitInfoUpdateParams {
    /// The commit to set or clear.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub sha: Option<Option<String>>,
    /// The branch to set or clear.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub branch: Option<Option<String>>,
    /// The remote to set or clear, sanitized before persistence.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub origin_url: Option<Option<String>>,
}

/// Updates the persistent thread's metadata.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadMetadataUpdateParams {
    pub(crate) thread_id: String,
    #[serde(default)]
    pub(crate) git_info: Option<ThreadMetadataGitInfoUpdateParams>,
    #[serde(default)]
    pub(crate) project_id: Option<String>,
    #[serde(default)]
    pub(crate) daybreak_enabled: Option<bool>,
}

impl ThreadMetadataUpdateParams {
    /// Updates `thread_id` with `git_info`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>, git_info: ThreadMetadataGitInfoUpdateParams) -> Self {
        Self {
            thread_id: thread_id.into(),
            git_info: Some(git_info),
            project_id: None,
            daybreak_enabled: None,
        }
    }
    /// The thread to update.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
    /// The first experimental field supplied.
    #[must_use]
    pub fn experimental_reason(&self) -> Option<&'static str> {
        if self.project_id.is_some() {
            Some("thread/metadata/update.projectId")
        } else if self.daybreak_enabled.is_some() {
            Some("thread/metadata/update.daybreakEnabled")
        } else {
            None
        }
    }
}

/// The successful answer containing the updated thread.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUnarchiveResponse {
    /// The updated thread, without turns.
    pub thread: Thread,
}

/// The successful answer containing the updated thread.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadMetadataUpdateResponse {
    /// The updated thread, without turns.
    pub thread: Thread,
}

/// What persisted threads are sorted by, as in Codex.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadSortKey {
    /// Creation time.
    CreatedAt,
    /// Last update time.
    UpdatedAt,
    /// Product recency, falling back to update time without a recency index.
    RecencyAt,
    /// Product section order, requiring a section index.
    SectionPosition,
}

/// One or several exact working-directory filters.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThreadListCwdFilter {
    /// A single directory.
    One(String),
    /// Any of the directories.
    Many(Vec<String>),
}

/// Codex's session source vocabulary for thread listings.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadSourceKind {
    /// CLI session.
    Cli,
    /// VS Code session.
    #[serde(rename = "vscode")]
    VsCode,
    /// Noninteractive session.
    Exec,
    /// App server session.
    AppServer,
    /// Any sub-agent session.
    SubAgent,
    /// Review sub-agent.
    SubAgentReview,
    /// Compaction sub-agent.
    SubAgentCompact,
    /// Spawned thread.
    SubAgentThreadSpawn,
    /// Other sub-agent.
    SubAgentOther,
    /// Unspecified source.
    Unknown,
}

/// Filters and pages persisted threads; distinct from `thread/loaded/list`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::option_option,
    reason = "Codex's omitted, null and set filters"
)]
pub struct ThreadListParams {
    /// Opaque cursor of the preceding page.
    pub cursor: Option<String>,
    /// Page size, clamped to 1 through 100, default 25.
    pub limit: Option<u32>,
    /// Sort key, default creation time.
    pub sort_key: Option<ThreadSortKey>,
    /// Sort direction, default newest first.
    pub sort_direction: Option<SortDirection>,
    /// Providers to include; an empty list includes all.
    pub model_providers: Option<Vec<String>>,
    /// Session sources to include.
    pub source_kinds: Option<Vec<ThreadSourceKind>>,
    /// Hosted-only originator filter; the local server refuses a nonempty list.
    pub originators: Option<Vec<String>>,
    /// Lists archived threads when true.
    pub archived: Option<bool>,
    /// Product section filter.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub section_id: Option<Option<String>>,
    /// Product project filter.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub project_id: Option<Option<String>>,
    /// Exact working directories to include.
    pub cwd: Option<ThreadListCwdFilter>,
    /// Reads only the state database without scanning rollouts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub use_state_db_only: bool,
    /// Substring search over stored titles.
    pub search_term: Option<String>,
    /// Direct spawn parent filter.
    pub parent_thread_id: Option<String>,
    /// Spawn ancestor filter.
    pub ancestor_thread_id: Option<String>,
}

impl ThreadListParams {
    /// The default listing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// The first experimental filter supplied.
    #[must_use]
    pub fn experimental_reason(&self) -> Option<&'static str> {
        if self.project_id.is_some() {
            Some("thread/list.projectId")
        } else if self.parent_thread_id.is_some() {
            Some("thread/list.parentThreadId")
        } else if self.ancestor_thread_id.is_some() {
            Some("thread/list.ancestorThreadId")
        } else {
            None
        }
    }
}

/// A page of persisted threads with opaque forward and reverse cursors.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResponse {
    /// The page's threads, without turns.
    pub data: Vec<Thread>,
    /// Continues in the current direction.
    pub next_cursor: Option<String>,
    /// Continues in the opposite direction.
    pub backwards_cursor: Option<String>,
}
