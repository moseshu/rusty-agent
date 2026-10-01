//! Records a run into a rollout file: the storage side of the runtime's rollout port.
//!
//! Ported from Codex's `RolloutRecorder`, its writer state and its persistence policy
//! (`rollout/src/recorder.rs`, `rollout/src/policy.rs`). Items are handed to a writer task over a
//! channel, so recording never waits on the disk, and are written in the order they were recorded.
//! What is not worth keeping is filtered out before it is queued.
//!
//! # Failures are retried, not dropped
//!
//! As in Codex, an item leaves the writer's queue only once it has been written. A write or open
//! that fails drops the file handle and keeps the unwritten items queued; every later item and
//! every flush is a barrier that reopens the file and tries again, once more on failure before
//! reporting. So a disk that comes back loses nothing that was recorded meanwhile. Reopening goes
//! through [`RolloutWriter::open`], which repairs a torn last line and resumes the sequence from
//! what is on disk.
//!
//! An item whose write failed after it reached the file — the flush failed, not the write — is
//! written again on retry and so appears twice, as in Codex; each copy is a complete record, and a
//! reader that keys records by their item id sees one.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, SessionErrorKind},
    event::{FileEvent, HostEventBody},
    session::{
        SessionId,
        rollout::{RolloutItem, RolloutRecorder},
    },
};
use tokio::sync::{mpsc, oneshot};

use super::writer::{RolloutPayload, RolloutWriter};

/// Whether `item` belongs in a rollout file.
///
/// Codex's `is_persisted_rollout_item` in its default (`legacy`) history mode, over this
/// framework's records. Every run record, turn context and usage record is kept. Of the host
/// events, only those a later reader needs to know happened are: file changes (Codex keeps
/// `PatchApplyEnd`) and multi-agent activity (Codex keeps sub-agent activity), and families this
/// build cannot name, which a newer reader may. Process execution, file reads and hook reports are
/// narration — Codex drops its exec, terminal and hook events — and so are dropped too.
#[must_use]
pub fn is_persisted_rollout_item(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::Event(event) => !matches!(
            event.body(),
            HostEventBody::Exec(_)
                | HostEventBody::Hook(_)
                | HostEventBody::File(FileEvent::Read(_))
        ),
        _ => true,
    }
}

enum Command {
    Record(Box<RolloutPayload>),
    Flush(oneshot::Sender<Result<()>>),
}

/// A [`RolloutRecorder`] that writes to a rollout file on a task of its own.
///
/// Clones share the task. Dropping every handle lets the task make a last attempt at what is still
/// queued and close the file.
#[derive(Clone)]
pub struct RolloutFileRecorder {
    commands: mpsc::UnboundedSender<Command>,
}

impl RolloutFileRecorder {
    /// Starts recording into `writer`, an open rollout.
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime, which the writer task needs.
    #[must_use]
    pub fn spawn(writer: RolloutWriter) -> Self {
        let state = WriterState {
            path: writer.path().to_path_buf(),
            session_id: writer.session_id().clone(),
            writer: Some(writer),
            pending: VecDeque::new(),
            last_logged_error: None,
        };
        Self::start(state)
    }

    /// Starts recording into the rollout at `path`, which is opened — and created if missing — when
    /// the first item is written, as Codex defers creating a rollout until it is first persisted.
    ///
    /// A file that cannot be opened yet is retried at every later item and flush.
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime, which the writer task needs.
    #[must_use]
    pub fn create(path: impl Into<PathBuf>, session_id: SessionId) -> Self {
        Self::start(WriterState {
            path: path.into(),
            session_id,
            writer: None,
            pending: VecDeque::new(),
            last_logged_error: None,
        })
    }

    fn start(state: WriterState) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        tokio::spawn(write_all(state, receiver));
        Self { commands }
    }
}

impl std::fmt::Debug for RolloutFileRecorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RolloutFileRecorder")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RolloutRecorder for RolloutFileRecorder {
    fn record(&self, item: RolloutItem) {
        if is_persisted_rollout_item(&item) {
            // The task outlives every sender, so a send fails only once it has stopped, which a
            // flush then reports.
            let _ = self.commands.send(Command::Record(Box::new(item.into())));
        }
    }

    async fn flush(&self) -> Result<()> {
        let (done, written) = oneshot::channel();
        if self.commands.send(Command::Flush(done)).is_err() {
            return Err(stopped());
        }
        written.await.map_err(|_| stopped())?
    }
}

/// What the writer task owns: Codex's `RolloutWriterState`.
struct WriterState {
    path: PathBuf,
    session_id: SessionId,
    /// The open file, or `None` before the first write and after a failure.
    writer: Option<RolloutWriter>,
    /// Recorded and not yet written, oldest first.
    pending: VecDeque<RolloutPayload>,
    last_logged_error: Option<String>,
}

impl WriterState {
    /// Writes what is queued, reopening and trying once more if that fails; with `sync`, also
    /// flushes the file. An item leaves the queue only once written.
    async fn write_with_recovery(&mut self, sync: bool) -> Result<()> {
        match self.write_once(sync).await {
            Ok(()) => {
                self.last_logged_error = None;
                Ok(())
            }
            Err(first) => {
                self.enter_recovery(&first);
                match self.write_once(sync).await {
                    Ok(()) => {
                        self.last_logged_error = None;
                        Ok(())
                    }
                    Err(second) => {
                        self.enter_recovery(&second);
                        Err(second)
                    }
                }
            }
        }
    }

    async fn write_once(&mut self, sync: bool) -> Result<()> {
        if self.writer.is_none() {
            self.writer = Some(open(&self.path, &self.session_id).await?);
        }
        let Some(writer) = self.writer.as_mut() else {
            return Err(stopped());
        };
        while let Some(payload) = self.pending.front() {
            writer.append(payload.clone()).await?;
            self.pending.pop_front();
        }
        if sync {
            writer.flush().await?;
        }
        Ok(())
    }

    /// Drops the file handle so the next barrier reopens it, keeping everything still queued.
    fn enter_recovery(&mut self, error: &Error) {
        let message = error.to_string();
        if self.last_logged_error.as_ref() != Some(&message) {
            tracing::warn!(
                path = %self.path.display(),
                queued = self.pending.len(),
                %error,
                "a rollout write failed; the queued records will be retried"
            );
        }
        self.last_logged_error = Some(message);
        self.writer = None;
    }
}

async fn open(path: &Path, session_id: &SessionId) -> Result<RolloutWriter> {
    RolloutWriter::open(path, session_id.clone()).await
}

async fn write_all(mut state: WriterState, mut commands: mpsc::UnboundedReceiver<Command>) {
    while let Some(command) = commands.recv().await {
        match command {
            Command::Record(payload) => {
                state.pending.push_back(*payload);
                // As Codex writes each batch as it arrives; a failure leaves it queued for the next
                // barrier and is reported by the next flush that cannot recover it.
                let _ = state.write_with_recovery(false).await;
            }
            Command::Flush(done) => {
                let _ = done.send(state.write_with_recovery(true).await);
            }
        }
    }
    // Every handle is gone: one last attempt at what is still queued.
    if !state.pending.is_empty() {
        let _ = state.write_with_recovery(true).await;
    }
}

fn stopped() -> Error {
    Error::session(
        SessionErrorKind::Io,
        "the rollout recorder's writer task has stopped",
    )
}
