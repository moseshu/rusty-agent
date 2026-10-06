//! The state database a rollout directory keeps beside its rollouts: Codex's `SQLite` state
//! database (`state/`) as its local store uses it for threads (`rollout/src/state_db.rs`,
//! `rollout/src/metadata.rs`, `thread-store/src/local/`).
//!
//! Without it, a listing reads the head of every rollout, which takes time in proportion to the
//! number of threads — about 40 ms for a thousand and 400 ms for ten thousand on a warm cache — as
//! the rollouts are named after their session, not their creation time. With it, a listing reads
//! one page of indexed rows.
//!
//! [`init`] opens the database in the rollout directory and fills it, the first time, from every
//! rollout there and in its archive, as Codex's startup backfill does. Attached to the directory
//! with [`with_state_db`](crate::rollout::RolloutThreadDirectory::with_state_db), it is kept in
//! step as the directory works:
//!
//! - what a live thread derives from its records — preview, first user message, title, model,
//!   effort, working directory, update time — is written to the thread's row, as Codex's
//!   `ThreadMetadataSync` writes through `record_thread_metadata`; a thread's row is created the
//!   first time, from that metadata;
//! - a name is kept in the row's title, as Codex keeps it in its legacy history mode, and in the
//!   session index; the row is then rebuilt from the rollout, keeping the name;
//! - archiving and unarchiving move the row with the rollout, and deleting a thread deletes its
//!   row; a move the database cannot follow is undone;
//! - reading a thread adds the model, effort and name the row holds to what its rollout says, as
//!   Codex does for a thread of its legacy history mode.
//!
//! A listing asked for [`ListThreadsParams::with_state_db_only`](crate::store::ListThreadsParams)
//! reads the database alone, as Codex's resume picker reads its first page. Any other listing scans
//! the rollouts as it does without the database, repairs the row of every thread it finds, and
//! returns the database's page, as Codex's `list_threads_with_db_fallback` does — see
//! [`RolloutThreadDirectory`](crate::rollout::RolloutThreadDirectory)'s listing for the details.
//!
//! # Differences from Codex
//!
//! - **One directory, one database.** Codex keeps one database per home, beside a tree of dated
//!   session directories. The database here lives in the rollout directory, named
//!   [`STATE_DB_FILENAME`], and indexes that directory's rollouts.
//! - **Schema.** Codex's thread table carries its product's columns — source, sandbox policy,
//!   approval mode, Git facts, sections, projects, recency, memory mode, token usage. This one
//!   keeps the columns this framework's threads have, under Codex's names, plus where a spawned
//!   thread came from and what a fork was forked from, so a listing from the database returns what
//!   a listing of rollouts returns. Its other tables — logs, goals, memories, spawn edges, queued
//!   items, paginated history — are not ported.
//! - **Connections.** Codex's `sqlx` pool is replaced by up to five `rusqlite` connections run on
//!   the blocking pool, with Codex's settings: write-ahead logging, normal synchronization, a five
//!   second busy timeout, incremental auto-vacuum.
//! - **Patch serialization.** Codex holds its per-thread pending metadata lock while reading,
//!   merging and writing a patch. Independently opened directory handles here have no shared
//!   pending metadata owner, so those database steps use one immediate transaction instead. It
//!   also runs to completion if its async caller stops waiting.
//! - **Migrations.** Codex runs `sqlx` migrations; here the schema version is the database's
//!   `user_version`. As with Codex's migrator, a database a newer build migrated further still
//!   opens.
//! - **Ties.** Codex breaks ties between threads of one creation or update time only for its
//!   recency sort; here every sort breaks them by id, as the listing of rollouts does, and a
//!   cursor reads the same either way.
//! - **Repairs.** Codex never points a row at another rollout during a repair, since after a
//!   revert several rollouts of one thread can be found. A thread here has one rollout, active or
//!   archived, so a row whose rollout is gone is pointed at the one a listing finds.
//! - **Search.** Codex's search matches a thread's name, title and preview in the database. Here
//!   a name lives in the title or the session index; the database search matches title and
//!   preview, so a name only the session index holds is found by the scan of rollouts, not by a
//!   listing of the database alone. The backfill, as Codex's, does not copy the session index.
//! - **Unavailable database.** As Codex's, a listing of the database alone returns nothing when
//!   the directory has none attached.

mod backfill;
mod list;
mod repair;
mod runtime;
mod threads;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use ra_core::{error::Result, event::EventTimestamp, session::SessionId};

pub use backfill::{BackfillState, BackfillStatus};
pub use runtime::{STATE_DB_FILENAME, StateRuntime};
pub use threads::ThreadMetadata;

use super::{ListThreadsParams, StoredThread, ThreadMetadataPatch, ThreadPage, index::ThreadIndex};

/// Opens the state database of the rollout directory `rollout_dir` and waits until its first
/// backfill is complete, running it here unless another process is: Codex's `state_db::try_init`.
///
/// # Errors
///
/// Returns an error if the database cannot be opened, or if another process's backfill does not
/// complete within thirty seconds.
pub async fn try_init(rollout_dir: impl AsRef<Path>) -> Result<Arc<StateRuntime>> {
    let rollout_dir = rollout_dir.as_ref();
    let runtime = StateRuntime::init(rollout_dir).await?;
    backfill::wait_for_backfill_gate(&runtime, rollout_dir).await?;
    Ok(runtime)
}

/// As [`try_init`], logging a failure and returning `None`, so the directory carries on without
/// the database: Codex's `state_db::init`.
pub async fn init(rollout_dir: impl AsRef<Path>) -> Option<Arc<StateRuntime>> {
    match try_init(rollout_dir).await {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            tracing::warn!(%error, "failed to initialize the state database");
            None
        }
    }
}

#[async_trait]
impl ThreadIndex for StateRuntime {
    async fn list_threads(
        &self,
        dir: &Path,
        index_dir: &Path,
        params: &ListThreadsParams,
    ) -> Result<ThreadPage> {
        list::list_threads(self, dir, index_dir, params).await
    }

    async fn overlay(&self, thread: &mut StoredThread) {
        list::overlay(self, thread).await;
    }

    async fn record_thread_metadata(
        &self,
        session_id: &SessionId,
        patch: &ThreadMetadataPatch,
        active_path: PathBuf,
    ) {
        let located = match self.get_thread(session_id).await {
            Ok(Some(row)) => (row.rollout_path, row.archived_at.is_some()),
            Ok(None) => (active_path, false),
            Err(error) => {
                tracing::warn!(%session_id, %error, "the state database could not record a thread's metadata");
                return;
            }
        };
        if let Err(error) =
            repair::apply_metadata_update(self, session_id, patch, located.0, located.1).await
        {
            tracing::warn!(%session_id, %error, "the state database could not record a thread's metadata");
        }
    }

    /// Codex's `update_thread_metadata` for its legacy history mode: the patch is written, and a
    /// name is followed by rebuilding the row from the rollout, which keeps the name.
    async fn update_thread_metadata(
        &self,
        session_id: &SessionId,
        patch: &ThreadMetadataPatch,
        rollout_path: PathBuf,
        archived: bool,
    ) {
        if let Err(error) =
            repair::apply_metadata_update(self, session_id, patch, rollout_path.clone(), archived)
                .await
        {
            tracing::warn!(%session_id, %error, "the state database could not update a thread's metadata");
            return;
        }
        if patch.name().is_some() {
            repair::reconcile_rollout(self, &rollout_path, Some(archived)).await;
        }
    }

    fn mark_archived(&self, session_id: &SessionId, path: &Path) -> Result<()> {
        self.mark_archived_blocking(session_id, path, EventTimestamp::now())
    }

    fn mark_unarchived(&self, session_id: &SessionId, path: &Path) -> Result<()> {
        self.mark_unarchived_blocking(session_id, path)
    }

    fn delete_threads(&self, session_ids: &[SessionId]) -> Result<()> {
        self.delete_threads_blocking(session_ids)
    }
}
