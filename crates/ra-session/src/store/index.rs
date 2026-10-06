//! What the rollout directory asks of an index it keeps of its threads, so the directory reads the
//! same with or without one. The state database is the only index; without the `sqlite` feature
//! there is none.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ra_core::{error::Result, session::SessionId};

use super::{ListThreadsParams, StoredThread, ThreadMetadataPatch, ThreadPage};

/// An index of a rollout directory's threads, kept beside their rollouts.
///
/// Writing metadata to it is best effort, as Codex's writes to its state database are for what a
/// thread's records say: a failure is logged and the rollout stays the record. Moving and deleting
/// rollouts is not: a move the index cannot follow is undone, and a deletion it cannot follow is
/// reported, as Codex's local store does. Those run on the blocking task that moves the files.
#[async_trait]
pub(crate) trait ThreadIndex: Send + Sync + std::fmt::Debug {
    /// Lists the threads of the rollouts in `dir`, with names from the session index in
    /// `index_dir`.
    async fn list_threads(
        &self,
        dir: &Path,
        index_dir: &Path,
        params: &ListThreadsParams,
    ) -> Result<ThreadPage>;

    /// Adds what the index knows of `thread` to what its rollout says.
    async fn overlay(&self, thread: &mut StoredThread);

    /// Writes metadata a live thread derived, for a thread whose rollout is or will be
    /// `active_path` unless the index already knows where it is.
    async fn record_thread_metadata(
        &self,
        session_id: &SessionId,
        patch: &ThreadMetadataPatch,
        active_path: PathBuf,
    );

    /// Writes metadata a caller set on the thread whose rollout is `path`, archived when
    /// `archived`.
    async fn update_thread_metadata(
        &self,
        session_id: &SessionId,
        patch: &ThreadMetadataPatch,
        path: PathBuf,
        archived: bool,
    );

    /// Records the thread archived, its rollout now at `path`.
    fn mark_archived(&self, session_id: &SessionId, path: &Path) -> Result<()>;

    /// Records the thread active again, its rollout now at `path`.
    fn mark_unarchived(&self, session_id: &SessionId, path: &Path) -> Result<()>;

    /// Forgets the threads of `session_ids`.
    fn delete_threads(&self, session_ids: &[SessionId]) -> Result<()>;
}
