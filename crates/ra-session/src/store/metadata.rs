//! What [`ThreadStore::update_thread_metadata`](super::ThreadStore::update_thread_metadata),
//! archiving and deletion take: Codex's `ThreadMetadataPatch`, `UpdateThreadMetadataParams`,
//! `ArchiveThreadParams`, `ArchiveThreadsParams`, `DeleteThreadParams` and `DeleteThreadsParams`
//! (`thread-store/src/types.rs`).

use ra_core::{event::EventTimestamp, session::SessionId};

/// A change to a thread's metadata: Codex's `ThreadMetadataPatch`, with the fields this
/// framework's records have.
///
/// A field left unset is left as it is. A field whose value may itself be cleared takes an inner
/// `Option`, where `Some(None)` clears it.
///
/// Codex's patch also carries the rollout path, recency, the creator's identity, the session
/// source and agent nickname, role and path, the approval mode and permission profile, the token
/// usage, Git facts, the memory mode and the product's project and Daybreak preferences. Those
/// are its product's or its state database's, and are not ported.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(
    clippy::option_option,
    reason = "Codex's `ClearableField`: unset, set, or cleared"
)]
pub struct ThreadMetadataPatch {
    name: Option<Option<String>>,
    preview: Option<String>,
    title: Option<String>,
    first_user_message: Option<String>,
    model_provider: Option<String>,
    model: Option<String>,
    effort: Option<Option<String>>,
    created_at: Option<EventTimestamp>,
    updated_at: Option<EventTimestamp>,
    cwd: Option<String>,
    cli_version: Option<String>,
    originator: Option<String>,
}

impl ThreadMetadataPatch {
    /// A patch that changes nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the thread's user-facing name, or clears it with `None`.
    #[must_use]
    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = Some(name);
        self
    }

    /// Sets the preview a listing shows.
    #[must_use]
    pub fn with_preview(mut self, preview: impl Into<String>) -> Self {
        self.preview = Some(preview.into());
        self
    }

    /// Sets the title derived from the history.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Sets the first user message.
    #[must_use]
    pub fn with_first_user_message(mut self, message: impl Into<String>) -> Self {
        self.first_user_message = Some(message.into());
        self
    }

    /// Sets the model provider.
    #[must_use]
    pub fn with_model_provider(mut self, provider: impl Into<String>) -> Self {
        self.model_provider = Some(provider.into());
        self
    }

    /// Sets the latest model.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the latest effort, or clears it with `None`: Codex's `reasoning_effort`.
    #[must_use]
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Sets when the thread was created.
    #[must_use]
    pub const fn with_created_at(mut self, created_at: EventTimestamp) -> Self {
        self.created_at = Some(created_at);
        self
    }

    /// Sets when the thread was last updated.
    #[must_use]
    pub const fn with_updated_at(mut self, updated_at: EventTimestamp) -> Self {
        self.updated_at = Some(updated_at);
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the version of the program that created the thread.
    #[must_use]
    pub fn with_cli_version(mut self, version: impl Into<String>) -> Self {
        self.cli_version = Some(version.into());
        self
    }

    /// Sets the originator recorded at creation.
    #[must_use]
    pub fn with_originator(mut self, originator: impl Into<String>) -> Self {
        self.originator = Some(originator.into());
        self
    }

    /// Merges `next` into this patch, as Codex's `merge` does: what `next` sets replaces what this
    /// one sets, clears included, and what it leaves unset stays.
    pub fn merge(&mut self, next: Self) {
        fn take<T>(current: &mut Option<T>, next: Option<T>) {
            if next.is_some() {
                *current = next;
            }
        }
        take(&mut self.name, next.name);
        take(&mut self.preview, next.preview);
        take(&mut self.title, next.title);
        take(&mut self.first_user_message, next.first_user_message);
        take(&mut self.model_provider, next.model_provider);
        take(&mut self.model, next.model);
        take(&mut self.effort, next.effort);
        take(&mut self.created_at, next.created_at);
        take(&mut self.updated_at, next.updated_at);
        take(&mut self.cwd, next.cwd);
        take(&mut self.cli_version, next.cli_version);
        take(&mut self.originator, next.originator);
    }

    /// Whether the patch changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The name to set, `Some(None)` to clear it.
    #[must_use]
    pub fn name(&self) -> Option<Option<&str>> {
        self.name.as_ref().map(Option::as_deref)
    }

    /// The preview to set.
    #[must_use]
    pub fn preview(&self) -> Option<&str> {
        self.preview.as_deref()
    }

    /// The title to set.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The first user message to set.
    #[must_use]
    pub fn first_user_message(&self) -> Option<&str> {
        self.first_user_message.as_deref()
    }

    /// The model provider to set.
    #[must_use]
    pub fn model_provider(&self) -> Option<&str> {
        self.model_provider.as_deref()
    }

    /// The model to set.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The effort to set, `Some(None)` to clear it.
    #[must_use]
    pub fn effort(&self) -> Option<Option<&str>> {
        self.effort.as_ref().map(Option::as_deref)
    }

    /// The creation time to set.
    #[must_use]
    pub const fn created_at(&self) -> Option<EventTimestamp> {
        self.created_at
    }

    /// The update time to set.
    #[must_use]
    pub const fn updated_at(&self) -> Option<EventTimestamp> {
        self.updated_at
    }

    /// The working directory to set.
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// The program version to set.
    #[must_use]
    pub fn cli_version(&self) -> Option<&str> {
        self.cli_version.as_deref()
    }

    /// The originator to set.
    #[must_use]
    pub fn originator(&self) -> Option<&str> {
        self.originator.as_deref()
    }
}

/// Which thread's metadata to change, and how: Codex's `UpdateThreadMetadataParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateThreadMetadataParams {
    session_id: SessionId,
    patch: ThreadMetadataPatch,
    include_archived: bool,
}

impl UpdateThreadMetadataParams {
    /// Applies `patch` to the active thread of `session_id`.
    #[must_use]
    pub const fn new(session_id: SessionId, patch: ThreadMetadataPatch) -> Self {
        Self {
            session_id,
            patch,
            include_archived: false,
        }
    }

    /// Lets an archived thread be changed as well: Codex's `include_archived`.
    #[must_use]
    pub const fn including_archived(mut self) -> Self {
        self.include_archived = true;
        self
    }

    /// The thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The change.
    #[must_use]
    pub const fn patch(&self) -> &ThreadMetadataPatch {
        &self.patch
    }

    /// Whether an archived thread may be changed.
    #[must_use]
    pub const fn include_archived(&self) -> bool {
        self.include_archived
    }
}

/// Which thread to archive or unarchive: Codex's `ArchiveThreadParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveThreadParams {
    session_id: SessionId,
}

impl ArchiveThreadParams {
    /// Archives or unarchives the thread of `session_id`.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }

    /// The thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// Which threads to archive, in order: Codex's `ArchiveThreadsParams`.
///
/// Codex's `writer_lock_thread_ids`, for descendants of its paginated mode whose rollout has not
/// materialized yet, is not ported.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveThreadsParams {
    session_ids: Vec<SessionId>,
}

impl ArchiveThreadsParams {
    /// Archives the threads of `session_ids`, in order.
    #[must_use]
    pub const fn new(session_ids: Vec<SessionId>) -> Self {
        Self { session_ids }
    }

    /// The threads' sessions, in order.
    #[must_use]
    pub fn session_ids(&self) -> &[SessionId] {
        &self.session_ids
    }
}

/// Which thread to delete: Codex's `DeleteThreadParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteThreadParams {
    session_id: SessionId,
}

impl DeleteThreadParams {
    /// Deletes the thread of `session_id`.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }

    /// The thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// Which threads to delete, in order: Codex's `DeleteThreadsParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteThreadsParams {
    session_ids: Vec<SessionId>,
}

impl DeleteThreadsParams {
    /// Deletes the threads of `session_ids`, in order.
    #[must_use]
    pub const fn new(session_ids: Vec<SessionId>) -> Self {
        Self { session_ids }
    }

    /// The threads' sessions, in order.
    #[must_use]
    pub fn session_ids(&self) -> &[SessionId] {
        &self.session_ids
    }
}
