//! The rollouts of an agent tree's threads, side by side in one directory.
//!
//! Ported from Codex's local thread store as it serves spawned threads. Every agent a tree spawns
//! is a thread with a rollout of its own, named after its session as any rollout here is, whose
//! first record is session metadata saying where the thread was spawned from: Codex's
//! `SessionMeta` with its `ThreadSpawn` source. The threads spawned from one session are found by
//! reading that metadata, as Codex lists a thread's children by filtering its rollouts' metadata
//! on the parent thread.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, SessionErrorKind},
    session::{
        SessionId,
        rollout::{RolloutRecorder, RolloutThreadSpawn, RolloutThreadStore},
    },
};

use super::{
    reader::RolloutReader,
    recorder::RolloutFileRecorder,
    writer::{RolloutSessionMeta, session_rollout_path},
};

/// A directory holding the rollouts of the threads an agent tree spawns, and usually its root's.
///
/// As a [`RolloutThreadStore`], it gives each spawned agent's thread the rollout
/// `rollout-<session id>.jsonl` here, recorded through a [`RolloutFileRecorder`] that writes the
/// thread's session metadata first. The file is created when the agent's first run records into
/// it, as Codex defers creating a rollout until something is persisted.
#[derive(Debug, Clone)]
pub struct RolloutThreadDirectory {
    dir: PathBuf,
}

impl RolloutThreadDirectory {
    /// The rollouts in `dir`, which is created when the first of them is.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Where the rollout of `session_id` is kept here.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if `session_id` holds characters that would leave the directory.
    pub fn rollout_path(&self, session_id: &SessionId) -> Result<PathBuf> {
        session_rollout_path(&self.dir, session_id)
    }

    /// The threads spawned from `parent`, oldest first, each with its session metadata: Codex's
    /// direct children of a thread.
    ///
    /// A rollout whose first record cannot be read is skipped, as Codex's listing skips one it
    /// cannot summarize, and so is one whose first record has not been written yet. Generated
    /// session ids sort by creation time, and so do the rollouts named after them.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if the directory exists but cannot be listed.
    pub async fn children(
        &self,
        parent: &SessionId,
    ) -> Result<Vec<(RolloutReader, RolloutSessionMeta)>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(list_error(&error)),
        };
        let mut paths = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| list_error(&error))?
        {
            // A rollout's sidecar ends in `.json` and is left out.
            let is_rollout = entry.file_name().to_str().is_some_and(|name| {
                name.starts_with("rollout-")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext == "jsonl")
            });
            // Only regular files: anything else may never end when read.
            if is_rollout
                && entry
                    .file_type()
                    .await
                    .is_ok_and(|file_type| file_type.is_file())
            {
                paths.push(entry.path());
            }
        }
        paths.sort();

        let mut children = Vec::new();
        for path in paths {
            let reader = RolloutReader::open(path);
            match reader.session_meta().await {
                Ok(Some(meta)) if meta.parent_session_id() == Some(parent) => {
                    children.push((reader, meta));
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    path = %reader.path().display(),
                    %error,
                    "a rollout's session metadata could not be read; it is not listed"
                ),
            }
        }
        Ok(children)
    }
}

#[async_trait]
impl RolloutThreadStore for RolloutThreadDirectory {
    async fn create_thread(
        &self,
        session_id: &SessionId,
        spawn: &RolloutThreadSpawn,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        let path = self.rollout_path(session_id)?;
        let meta = RolloutSessionMeta::new(session_id.clone()).with_thread_spawn(spawn.clone());
        Ok(Arc::new(RolloutFileRecorder::create_with_session_meta(
            path, meta,
        )))
    }
}

fn list_error(error: &std::io::Error) -> Error {
    Error::session(
        SessionErrorKind::Io,
        format!("failed to list the rollout directory: {error}"),
    )
}
