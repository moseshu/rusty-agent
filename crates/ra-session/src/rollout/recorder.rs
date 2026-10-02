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
//! A write can also fail after its record reached the file: the writer cannot tell how much of the
//! line the system took, and poisons itself. Codex writes such a record again on retry, so it
//! appears twice. Here the recorder remembers the sequence number the record was written under,
//! and once the file is reopened — which repairs or seals the line — looks for it: a record that
//! landed is not written again. A duplicated run start would otherwise repeat its input in the
//! rebuilt history, and a duplicated usage record would bill a model call twice.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, SessionErrorKind},
    event::{EventTimestamp, FileEvent, HostEventBody},
    session::{
        SessionId,
        rollout::{RolloutItem, RolloutRecorder},
    },
};
use tokio::sync::{mpsc, oneshot};

use super::{
    reader::RolloutReader,
    writer::{RolloutPayload, RolloutRecord, RolloutSessionMeta, RolloutWriter},
};

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
            session_meta: None,
            pending: VecDeque::new(),
            unconfirmed: None,
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
            session_meta: None,
            pending: VecDeque::new(),
            unconfirmed: None,
            last_logged_error: None,
        })
    }

    /// As [`Self::create`], for the session `meta` describes, which is written as the file's first
    /// record when the file is opened and holds none yet — as Codex writes `SessionMeta` first when
    /// it creates a rollout, and not again when it resumes one.
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime, which the writer task needs.
    #[must_use]
    pub fn create_with_session_meta(path: impl Into<PathBuf>, meta: RolloutSessionMeta) -> Self {
        Self::start(WriterState {
            path: path.into(),
            session_id: meta.session_id().clone(),
            writer: None,
            session_meta: Some(meta),
            pending: VecDeque::new(),
            unconfirmed: None,
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
    /// The session's metadata, until the file is first opened: it is then queued ahead of
    /// everything else if the file holds no record yet.
    session_meta: Option<RolloutSessionMeta>,
    /// Recorded and not yet written, oldest first.
    pending: VecDeque<RolloutPayload>,
    /// The sequence number the front of the queue was written under when that write failed with
    /// its outcome unknown, until the reopened file says whether it landed.
    unconfirmed: Option<u64>,
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
            let writer = open(&self.path, &self.session_id).await?;
            self.settle_unconfirmed(&writer).await?;
            if let Some(meta) = self.session_meta.take()
                && writer.next_timeline_seq() == 0
            {
                self.pending.push_front(RolloutPayload::SessionMeta(meta));
            }
            self.writer = Some(writer);
        }
        let Some(writer) = self.writer.as_mut() else {
            return Err(stopped());
        };
        while let Some(payload) = self.pending.front().cloned() {
            let timeline_seq = writer.next_timeline_seq();
            let was_poisoned = writer.is_poisoned();
            if let Err(error) = writer.append(payload).await {
                // Poisoned by this append: its bytes may be on disk, in part or in full.
                if !was_poisoned && writer.is_poisoned() {
                    self.unconfirmed = Some(timeline_seq);
                }
                return Err(error);
            }
            self.pending.pop_front();
        }
        if sync {
            writer.flush().await?;
        }
        Ok(())
    }

    /// Takes the front of the queue off if the write whose outcome was unknown landed after all.
    ///
    /// Called once the file has been reopened, which truncates a torn line and seals one that is
    /// complete but for its newline, so a record found under the sequence number it was written
    /// under, with its type and payload, is the one that write produced. Anything else at that
    /// number — nothing, or a checkpoint that took it — means it did not land.
    ///
    /// The file is only read when the reopened writer resumed past that number, which is the only
    /// way anything can sit there. Otherwise nothing landed, and nothing is read: a path that is not
    /// a regular file, such as a device that reports no length and never ends, is not scanned.
    async fn settle_unconfirmed(&mut self, reopened: &RolloutWriter) -> Result<()> {
        let Some(timeline_seq) = self.unconfirmed else {
            return Ok(());
        };
        if reopened.next_timeline_seq() > timeline_seq
            && let Some(payload) = self.pending.front()
        {
            let expected = RolloutRecord::new(
                timeline_seq,
                EventTimestamp::from_millis(0),
                payload.clone(),
            )?;
            let landed = RolloutReader::open(&self.path)
                .read_all()
                .await?
                .iter()
                .any(|record| {
                    record.timeline_seq() == timeline_seq
                        && record.type_name() == expected.type_name()
                        && record.payload_value() == expected.payload_value()
                });
            if landed {
                self.pending.pop_front();
            }
        }
        self.unconfirmed = None;
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
