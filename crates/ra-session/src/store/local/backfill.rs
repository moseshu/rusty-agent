//! Filling a new state database from the rollouts already written: Codex's backfill
//! (`state/src/runtime/backfill.rs`, `rollout/src/metadata.rs`) and the gate its startup waits at
//! (`rollout/src/state_db.rs`).

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ra_core::error::{Error, Result, SessionErrorKind};
use rusqlite::{OptionalExtension, params};

use super::{StateRuntime, repair::extract_metadata_from_rollout, threads::modified_time};
use crate::rollout::thread_files::ARCHIVED_SESSIONS_SUBDIR;

/// How many rollouts are read between two checkpoints: Codex's `BACKFILL_BATCH_SIZE`.
const BACKFILL_BATCH_SIZE: usize = 200;

/// How long a backfill another process claimed is left to it: Codex's `BACKFILL_LEASE_SECONDS`.
const BACKFILL_LEASE_SECONDS: i64 = 900;

/// How often startup looks again at a backfill another process is running: Codex's
/// `STARTUP_BACKFILL_POLL_INTERVAL`.
const STARTUP_BACKFILL_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long startup waits for a backfill another process is running: Codex's
/// `STARTUP_BACKFILL_WAIT_TIMEOUT`.
const STARTUP_BACKFILL_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the first backfill of a state database stands: Codex's `BackfillStatus`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum BackfillStatus {
    /// Not started.
    #[default]
    Pending,
    /// Claimed by a process, which may still be running it.
    Running,
    /// Every rollout found has been read.
    Complete,
}

impl BackfillStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Complete => "complete",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "complete" => Self::Complete,
            _ => Self::Pending,
        }
    }
}

/// The state of a database's first backfill: Codex's `BackfillState`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillState {
    status: BackfillStatus,
    last_watermark: Option<String>,
    last_success_at: Option<i64>,
}

impl BackfillState {
    /// Where the backfill stands.
    #[must_use]
    pub const fn status(&self) -> BackfillStatus {
        self.status
    }

    /// The last rollout read, relative to the rollout directory, if any was.
    #[must_use]
    pub fn last_watermark(&self) -> Option<&str> {
        self.last_watermark.as_deref()
    }

    /// When the backfill completed, in seconds since the Unix epoch.
    #[must_use]
    pub const fn last_success_at(&self) -> Option<i64> {
        self.last_success_at
    }
}

impl StateRuntime {
    /// Where the database's first backfill stands: Codex's `get_backfill_state`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be read.
    pub async fn get_backfill_state(&self) -> Result<BackfillState> {
        self.run(|connection| {
            ensure_backfill_state_row(connection)?;
            connection.query_row(
                "SELECT status, last_watermark, last_success_at FROM backfill_state WHERE id = 1",
                [],
                |row| {
                    Ok(BackfillState {
                        status: BackfillStatus::parse(&row.get::<_, String>(0)?),
                        last_watermark: row.get(1)?,
                        last_success_at: row.get(2)?,
                    })
                },
            )
        })
        .await
    }

    /// Claims the backfill for this process unless it is complete or another process claimed it
    /// less than `lease_seconds` ago: Codex's `try_claim_backfill`.
    async fn try_claim_backfill(&self, lease_seconds: i64) -> Result<bool> {
        self.run(move |connection| {
            ensure_backfill_state_row(connection)?;
            let now = unix_seconds();
            let changed = connection.execute(
                "UPDATE backfill_state SET status = ?1, updated_at = ?2
                 WHERE id = 1 AND status != ?3 AND (status != ?1 OR updated_at <= ?4)",
                params![
                    BackfillStatus::Running.as_str(),
                    now,
                    BackfillStatus::Complete.as_str(),
                    now.saturating_sub(lease_seconds.max(0)),
                ],
            )?;
            Ok(changed == 1)
        })
        .await
    }

    /// Records that the backfill has read every rollout up to `watermark`: Codex's
    /// `checkpoint_backfill`.
    async fn checkpoint_backfill(&self, watermark: String) -> Result<()> {
        self.run(move |connection| {
            connection
                .execute(
                    "UPDATE backfill_state SET status = ?1, last_watermark = ?2, updated_at = ?3
                     WHERE id = 1",
                    params![BackfillStatus::Running.as_str(), watermark, unix_seconds()],
                )
                .map(|_| ())
        })
        .await
    }

    /// Records the backfill complete: Codex's `mark_backfill_complete`.
    async fn mark_backfill_complete(&self, last_watermark: Option<String>) -> Result<()> {
        self.run(move |connection| {
            let now = unix_seconds();
            connection
                .execute(
                    "UPDATE backfill_state
                     SET status = ?1, last_watermark = COALESCE(?2, last_watermark),
                         last_success_at = ?3, updated_at = ?3
                     WHERE id = 1",
                    params![BackfillStatus::Complete.as_str(), last_watermark, now],
                )
                .map(|_| ())
        })
        .await
    }
}

fn ensure_backfill_state_row(connection: &rusqlite::Connection) -> rusqlite::Result<()> {
    let exists = connection
        .query_row("SELECT 1 FROM backfill_state WHERE id = 1", [], |_| Ok(()))
        .optional()?
        .is_some();
    if !exists {
        connection.execute(
            "INSERT INTO backfill_state (id, status, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO NOTHING",
            params![BackfillStatus::Pending.as_str(), unix_seconds()],
        )?;
    }
    Ok(())
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Waits until the database's first backfill is complete, running it here if no other process
/// is: Codex's `wait_for_backfill_gate`.
pub(crate) async fn wait_for_backfill_gate(db: &StateRuntime, rollout_dir: &Path) -> Result<()> {
    let started = Instant::now();
    let mut reported = false;
    loop {
        if db.get_backfill_state().await?.status() == BackfillStatus::Complete {
            return Ok(());
        }
        backfill_sessions(db, rollout_dir, BACKFILL_LEASE_SECONDS).await;
        let state = db.get_backfill_state().await?;
        if state.status() == BackfillStatus::Complete {
            return Ok(());
        }
        if started.elapsed() >= STARTUP_BACKFILL_WAIT_TIMEOUT {
            return Err(Error::session(
                SessionErrorKind::Io,
                format!(
                    "timed out waiting for the state database backfill of `{}` after {:?} \
                     (status: {})",
                    rollout_dir.display(),
                    STARTUP_BACKFILL_WAIT_TIMEOUT,
                    state.status().as_str()
                ),
            ));
        }
        if !reported {
            tracing::warn!(
                dir = %rollout_dir.display(),
                status = state.status().as_str(),
                "another process is backfilling the state database; waiting for it"
            );
            reported = true;
        }
        tokio::time::sleep(STARTUP_BACKFILL_POLL_INTERVAL).await;
    }
}

/// A rollout the backfill reads, and the watermark that orders it: its path relative to the
/// rollout directory.
struct BackfillRollout {
    watermark: String,
    path: PathBuf,
    archived: bool,
}

/// Reads every rollout in `rollout_dir` and its archive into the database, unless the backfill is
/// complete or another process holds it: Codex's `backfill_sessions_with_lease`.
///
/// Rollouts are read in the order of their watermark, from after the last one a previous attempt
/// checkpointed, and a checkpoint is written after every [`BACKFILL_BATCH_SIZE`]. Each row is
/// rebuilt from its rollout, keeping a name the existing row was given; an archived rollout is
/// archived at its modification time. A rollout that cannot be read is logged and skipped, as
/// Codex's is.
pub(crate) async fn backfill_sessions(db: &StateRuntime, rollout_dir: &Path, lease_seconds: i64) {
    let Ok(state) = db.get_backfill_state().await else {
        return;
    };
    if state.status() == BackfillStatus::Complete {
        return;
    }
    match db.try_claim_backfill(lease_seconds).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(dir = %rollout_dir.display(), %error, "failed to claim the state database backfill");
            return;
        }
    }
    let state = db.get_backfill_state().await.unwrap_or_default();

    let mut rollouts = Vec::new();
    for (dir, archived) in [
        (rollout_dir.to_path_buf(), false),
        (rollout_dir.join(ARCHIVED_SESSIONS_SUBDIR), true),
    ] {
        match crate::lite::rollout_files(&dir).await {
            Ok(paths) => rollouts.extend(paths.into_iter().map(|path| BackfillRollout {
                watermark: watermark(rollout_dir, &path),
                path,
                archived,
            })),
            Err(error) => {
                tracing::warn!(dir = %dir.display(), %error, "failed to list rollouts to backfill");
            }
        }
    }
    rollouts.sort_by(|left, right| left.watermark.cmp(&right.watermark));
    if let Some(last) = state.last_watermark() {
        rollouts.retain(|rollout| rollout.watermark.as_str() > last);
    }

    let (mut upserted, mut failed) = (0usize, 0usize);
    let mut last_watermark = state.last_watermark;
    for batch in rollouts.chunks(BACKFILL_BATCH_SIZE) {
        for rollout in batch {
            match backfill_one(db, rollout).await {
                Ok(true) => upserted += 1,
                Ok(false) => {}
                Err(error) => {
                    failed += 1;
                    tracing::warn!(path = %rollout.path.display(), %error, "failed to backfill a rollout");
                }
            }
        }
        if let Some(last) = batch.last() {
            match db.checkpoint_backfill(last.watermark.clone()).await {
                Ok(()) => last_watermark = Some(last.watermark.clone()),
                Err(error) => {
                    tracing::warn!(dir = %rollout_dir.display(), %error, "failed to checkpoint the state database backfill");
                }
            }
        }
    }
    if let Err(error) = db.mark_backfill_complete(last_watermark).await {
        tracing::warn!(dir = %rollout_dir.display(), %error, "failed to mark the state database backfill complete");
    }
    tracing::info!(upserted, failed, "state database backfill finished");
}

/// Reads one rollout into its row, reporting whether it described a thread.
async fn backfill_one(db: &StateRuntime, rollout: &BackfillRollout) -> Result<bool> {
    let Some(mut metadata) = extract_metadata_from_rollout(&rollout.path).await? else {
        return Ok(false);
    };
    if let Some(existing) = db.get_thread(&metadata.session_id).await? {
        metadata.prefer_existing_explicit_title(&existing);
    }
    if rollout.archived && metadata.archived_at.is_none() {
        metadata.archived_at = Some(modified_time(&rollout.path).unwrap_or(metadata.updated_at));
    }
    db.upsert_thread(&metadata).await?;
    Ok(true)
}

fn watermark(rollout_dir: &Path, path: &Path) -> String {
    path.strip_prefix(rollout_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
