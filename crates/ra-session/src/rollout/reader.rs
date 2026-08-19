//! Session replay, validation, and rollout line reading.

use std::{
    collections::HashMap,
    hash::BuildHasher,
    path::{Path, PathBuf},
};

use ra_core::{
    error::{Error, Result, SessionErrorKind},
    event::AgentOperationId,
    session::SessionId,
    state::RunId,
    usage::Usage,
};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom},
};

use super::writer::{ChildAnchorKind, RolloutCheckpoint, RolloutPayload, RolloutRecord};

/// Reconstructed summary of a rollout file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutSummary {
    session_id: Option<SessionId>,
    record_count: usize,
    first_timeline_seq: Option<u64>,
    last_timeline_seq: Option<u64>,
    persisted_run_max_seq: HashMap<RunId, u64>,
    usage_totals: Usage,
    valid_bytes_len: u64,
    corrupted_trailing_bytes: Option<usize>,
    non_monotonic_timeline_seq: Option<(u64, u64)>,
    last_checkpoint_offset: Option<u64>,
    records_since_last_checkpoint: usize,
}

impl RolloutSummary {
    /// Session identity if identified from metadata.
    #[must_use]
    pub const fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// Total number of valid records read.
    #[must_use]
    pub const fn record_count(&self) -> usize {
        self.record_count
    }

    /// Sequence number of the first record in the scanned range.
    ///
    /// Distinct from [`RolloutSummary::last_timeline_seq`] when the scan started at an offset:
    /// checking a tail against what precedes it needs the boundary record, not the maximum.
    #[must_use]
    pub const fn first_timeline_seq(&self) -> Option<u64> {
        self.first_timeline_seq
    }

    /// Maximum timeline sequence number present in the scanned range.
    #[must_use]
    pub const fn last_timeline_seq(&self) -> Option<u64> {
        self.last_timeline_seq
    }

    /// Map of maximum persisted host event sequence number per run.
    #[must_use]
    pub const fn persisted_run_max_seq(&self) -> &HashMap<RunId, u64> {
        &self.persisted_run_max_seq
    }

    /// Accumulated unpruned token usage totals across all model usage records in the file.
    #[must_use]
    pub const fn usage_totals(&self) -> &Usage {
        &self.usage_totals
    }

    /// Absolute byte offset just past the last valid record read.
    #[must_use]
    pub const fn valid_bytes_len(&self) -> u64 {
        self.valid_bytes_len
    }

    /// Number of bytes in a truncated/corrupted trailing line at EOF, if one was detected.
    #[must_use]
    pub const fn corrupted_trailing_bytes(&self) -> Option<usize> {
        self.corrupted_trailing_bytes
    }

    /// Byte offset of the last checkpoint record in the scanned range.
    ///
    /// Recovery uses this to keep its index pointing at the newest checkpoint even when it
    /// started from an older one.
    #[must_use]
    pub const fn last_checkpoint_offset(&self) -> Option<u64> {
        self.last_checkpoint_offset
    }

    /// Records that follow the last checkpoint in the scanned range.
    ///
    /// A writer resuming here has this many records behind it since the last checkpoint, so
    /// carrying it over is what keeps the checkpoint interval honest across restarts.
    #[must_use]
    pub const fn records_since_last_checkpoint(&self) -> usize {
        self.records_since_last_checkpoint
    }

    /// The first `(previous, offending)` pair where `timeline_seq` failed to increase, if any.
    ///
    /// The writer assigns `timeline_seq` at append time and only advances it once the record is
    /// flushed, so on a healthy file the values strictly increase. A repeat or a step backwards
    /// means two writers interleaved on the file, or it was edited by something that is not this
    /// writer — either way the sequence can no longer be trusted to order the timeline.
    ///
    /// **Gaps are not reported here**: truncating a corrupt tail legitimately leaves one, and per
    /// the run-state contract sequence spaces are allowed holes. Only non-increase is a defect.
    #[must_use]
    pub const fn non_monotonic_timeline_seq(&self) -> Option<(u64, u64)> {
        self.non_monotonic_timeline_seq
    }
}

/// A reader for parsing, inspecting, and replaying rollout event files.
#[derive(Debug, Clone)]
pub struct RolloutReader {
    path: PathBuf,
}

impl RolloutReader {
    /// Creates a reader for the rollout file at `path`.
    #[must_use]
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Filesystem path of the rollout file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads all valid records from the file.
    ///
    /// If the file ends with an unterminated partial line (for instance, following a process
    /// crash or power interruption), that trailing fragment is safely ignored and does not
    /// invalidate earlier valid records.
    ///
    /// Records whose payload this build cannot interpret are still returned: they round-trip
    /// verbatim through [`RolloutRecord`], and [`RolloutRecord::payload`] reports the failure to
    /// whoever asks for a typed payload.
    ///
    /// **This validates line-level envelopes only.** Payloads are left unparsed, so a record whose
    /// contents are damaged still comes back here, and checkpoints are not held to the records
    /// they summarize. [`RolloutReader::scan_summary`] is the check that does both.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if an I/O error occurs, or if a newline-terminated line is not valid
    /// UTF-8 or not a valid record envelope.
    pub async fn read_all(&self) -> Result<Vec<RolloutRecord>> {
        let (records, _, _) = self.read_records_internal(0).await?;
        Ok(records.into_iter().map(|(_, record)| record).collect())
    }

    /// Scans the rollout file and produces a summary including sequence bounds, usage totals,
    /// and the valid byte offset.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if reading fails.
    pub async fn scan_summary(&self) -> Result<RolloutSummary> {
        self.scan_summary_from(0).await
    }

    /// Scans from byte offset `from` to the end of the file.
    ///
    /// `from` must sit on a record boundary; a caller that has one is expected to have got it by
    /// reading a record there, not by guessing. Records before `from` are not read, so the
    /// resulting summary describes only the tail: [`RolloutSummary::usage_totals`] and
    /// [`RolloutSummary::persisted_run_max_seq`] must be folded onto whatever covers the head,
    /// and corruption earlier in the file is not detected.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if reading fails, or if a newline-terminated line in the scanned range
    /// is not valid UTF-8 or not a valid record envelope.
    pub async fn scan_summary_from(&self, from: u64) -> Result<RolloutSummary> {
        let (records, valid_bytes_len, corrupted_trailing_bytes) =
            self.read_records_internal(from).await?;

        let mut session_id = None;
        let mut first_timeline_seq: Option<u64> = None;
        let mut last_timeline_seq: Option<u64> = None;
        let mut persisted_run_max_seq = HashMap::new();
        let mut usage_totals = Usage::default();
        let mut non_monotonic_timeline_seq = None;
        let mut last_checkpoint_offset = None;
        let mut records_since_last_checkpoint = 0usize;

        // Only a scan that started at the top has the prefix a checkpoint claims to summarize,
        // and so only it can hold the checkpoint to that claim.
        let verifies_checkpoints = from == 0;

        for (offset, record) in &records {
            let seq = record.timeline_seq();
            if first_timeline_seq.is_none() {
                first_timeline_seq = Some(seq);
            }
            if let Some(prev) = last_timeline_seq
                && seq <= prev
                && non_monotonic_timeline_seq.is_none()
            {
                non_monotonic_timeline_seq = Some((prev, seq));
            }
            last_timeline_seq = Some(last_timeline_seq.map_or(seq, |prev| prev.max(seq)));
            records_since_last_checkpoint += 1;

            if let Ok(payload) = record.payload() {
                match payload {
                    RolloutPayload::SessionMeta(meta) => {
                        if session_id.is_none() {
                            session_id = Some(meta.session_id().clone());
                        }
                    }
                    RolloutPayload::Event(event) => {
                        let entry = persisted_run_max_seq
                            .entry(event.run_id().clone())
                            .or_insert(0);
                        *entry = (*entry).max(event.seq());
                    }
                    RolloutPayload::ModelUsage(mu) => {
                        usage_totals = usage_totals.accumulate(mu.usage());
                    }
                    RolloutPayload::Checkpoint(checkpoint) => {
                        if verifies_checkpoints {
                            verify_checkpoint(
                                &self.path,
                                seq,
                                &checkpoint,
                                session_id.as_ref(),
                                &usage_totals,
                                &persisted_run_max_seq,
                            )?;
                        }
                        last_checkpoint_offset = Some(*offset);
                        records_since_last_checkpoint = 0;
                    }
                    _ => {}
                }
            }
        }

        Ok(RolloutSummary {
            session_id,
            record_count: records.len(),
            first_timeline_seq,
            last_timeline_seq,
            persisted_run_max_seq,
            usage_totals,
            valid_bytes_len,
            corrupted_trailing_bytes,
            non_monotonic_timeline_seq,
            last_checkpoint_offset,
            records_since_last_checkpoint,
        })
    }

    /// Reads the file line by line, returning the valid records, the byte offset just past the
    /// last one, and the size of an unterminated trailing fragment if the file ends in one.
    ///
    /// A line is only ever treated as a torn write when it carries **no terminating newline**.
    /// `read_until` stops short of its delimiter at EOF and nowhere else, so a missing newline is
    /// the one signal that distinguishes "the process died mid-`write_all`" from "this line was
    /// written in full and this build cannot read it". The distinction decides whether
    /// [`RolloutWriter::open`](super::writer::RolloutWriter::open) is allowed to truncate those
    /// bytes away, so conflating the two would let a downgrade delete a complete record.
    async fn read_records_internal(
        &self,
        from: u64,
    ) -> Result<(Vec<(u64, RolloutRecord)>, u64, Option<usize>)> {
        if !self.path.exists() {
            return Ok((Vec::new(), 0, None));
        }

        let mut file = File::open(&self.path).await.map_err(|e| {
            Error::session(
                SessionErrorKind::Io,
                format!("failed to open rollout file for reading: {e}"),
            )
            .with_source(e)
        })?;

        if from > 0 {
            file.seek(SeekFrom::Start(from)).await.map_err(|e| {
                Error::session(
                    SessionErrorKind::Io,
                    format!("failed to seek into rollout file: {e}"),
                )
                .with_source(e)
            })?;
        }

        let mut reader = BufReader::new(file);
        let mut byte_buffer = Vec::new();
        let mut line_number: usize = 0;
        let mut records = Vec::new();
        let mut valid_bytes_len: u64 = from;
        let mut corrupted_trailing_bytes = None;

        loop {
            byte_buffer.clear();
            let bytes_read = reader
                .read_until(b'\n', &mut byte_buffer)
                .await
                .map_err(|e| {
                    Error::session(
                        SessionErrorKind::Io,
                        format!("failed to read line from rollout file: {e}"),
                    )
                    .with_source(e)
                })?;

            if bytes_read == 0 {
                break;
            }

            line_number += 1;
            let terminated = byte_buffer.last() == Some(&b'\n');

            let Ok(line_str) = std::str::from_utf8(&byte_buffer) else {
                // Multibyte UTF-8 cut mid-character: a torn write only if nothing terminated it.
                if terminated {
                    return Err(Error::session(
                        SessionErrorKind::Corrupted,
                        format!("corrupted non-UTF8 rollout record at line {line_number}"),
                    ));
                }
                corrupted_trailing_bytes = Some(bytes_read);
                break;
            };

            let trimmed = line_str.trim();
            if trimmed.is_empty() {
                valid_bytes_len += bytes_read as u64;
                continue;
            }

            match serde_json::from_str::<RolloutRecord>(trimmed) {
                // The record is kept whether or not `payload()` can interpret it. A payload this
                // build cannot parse is what a record from a newer build looks like on a
                // downgrade read; the raw value and unknown fields survive on the record, so
                // keeping the line preserves it verbatim. Rejecting it here would fail the whole
                // file, and marking it corrupt would hand the writer permission to erase it.
                Ok(record) => {
                    records.push((valid_bytes_len, record));
                    valid_bytes_len += bytes_read as u64;
                }
                Err(err) => {
                    if terminated {
                        return Err(Error::session(
                            SessionErrorKind::Corrupted,
                            format!("corrupted rollout record at line {line_number}: {err}"),
                        ));
                    }
                    corrupted_trailing_bytes = Some(bytes_read);
                    break;
                }
            }
        }

        Ok((records, valid_bytes_len, corrupted_trailing_bytes))
    }
}

/// Holds a checkpoint to the prefix it claims to summarize.
///
/// A checkpoint is a derived value living among primary facts, which makes it the one record in
/// the log that can be checked against the others. Doing so costs nothing here because the scan
/// has already read that prefix — and it is the only place the check is possible, since knowing
/// whether the figures follow from the records means having read the records. Resume's fast path
/// deliberately does not read them, so a checkpoint whose numbers were edited in place is
/// believed there and caught here.
fn verify_checkpoint(
    path: &Path,
    timeline_seq: u64,
    checkpoint: &RolloutCheckpoint,
    log_session_id: Option<&SessionId>,
    expected_usage: &Usage,
    expected_run_max_seq: &HashMap<RunId, u64>,
) -> Result<()> {
    // Only checkable when the log states its own identity; a rollout without `session_meta` has
    // nothing to contradict. Recovery covers that gap on its side by refusing to index a
    // checkpoint it cannot use.
    if let Some(log_session_id) = log_session_id
        && checkpoint.session_id() != log_session_id
    {
        return Err(Error::session(
            SessionErrorKind::Corrupted,
            format!(
                "rollout checkpoint at timeline_seq {timeline_seq} in {} names session {}, but \
                 the log belongs to {}",
                path.display(),
                checkpoint.session_id().as_str(),
                log_session_id.as_str()
            ),
        ));
    }

    let found = checkpoint.usage_totals();
    let usage_matches = found.input_tokens() == expected_usage.input_tokens()
        && found.output_tokens() == expected_usage.output_tokens()
        && found.cached_input_tokens() == expected_usage.cached_input_tokens()
        && found.cache_write_tokens() == expected_usage.cache_write_tokens()
        && found.reasoning_tokens() == expected_usage.reasoning_tokens();

    if !usage_matches {
        return Err(Error::session(
            SessionErrorKind::Corrupted,
            format!(
                "rollout checkpoint at timeline_seq {timeline_seq} in {} disagrees with the \
                 records it summarizes: it claims {} input tokens, they add up to {}",
                path.display(),
                found.input_tokens(),
                expected_usage.input_tokens()
            ),
        ));
    }

    if checkpoint.persisted_run_max_seq() != expected_run_max_seq {
        return Err(Error::session(
            SessionErrorKind::Corrupted,
            format!(
                "rollout checkpoint at timeline_seq {timeline_seq} in {} disagrees with the \
                 records it summarizes: per-run event sequences do not match",
                path.display()
            ),
        ));
    }

    Ok(())
}

/// An individual replay item in a unified multi-agent session replay stream.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum UnifiedReplayItem {
    /// A record belonging to the root timeline.
    Root(RolloutRecord),
    /// A child agent record grafted at an anchor point.
    Child {
        /// Associated operation identifier.
        operation_id: AgentOperationId,
        /// The child transcript record.
        record: RolloutRecord,
    },
}

/// Grafts independent child agent transcripts into a unified root timeline using anchor events.
///
/// Child run records belonging to each `operation_id` are inserted immediately following
/// the child's `Spawned` anchor event in child timeline order.
#[must_use]
pub fn graft_child_transcripts<S: BuildHasher>(
    root_records: &[RolloutRecord],
    child_transcripts: &HashMap<AgentOperationId, Vec<RolloutRecord>, S>,
) -> Vec<UnifiedReplayItem> {
    let mut unified = Vec::new();

    for record in root_records {
        unified.push(UnifiedReplayItem::Root(record.clone()));

        if let Ok(RolloutPayload::ChildAnchor(anchor)) = record.payload()
            && anchor.kind() == &ChildAnchorKind::Spawned
            && let Some(child_recs) = child_transcripts.get(anchor.operation_id())
        {
            for child_rec in child_recs {
                unified.push(UnifiedReplayItem::Child {
                    operation_id: anchor.operation_id().clone(),
                    record: child_rec.clone(),
                });
            }
        }
    }

    unified
}
