//! Execution output collection, eviction reasons, and summary metadata.

use std::{collections::VecDeque, fmt::Write as _, time::Duration};

use ra_core::compat::{SchemaVersion, Unknown};
pub use ra_core::event::exec::ExecEvictionReason;
use serde::{Deserialize, Serialize};

use crate::EXEC_SCHEMA_VERSION;

const fn default_schema_version() -> SchemaVersion {
    EXEC_SCHEMA_VERSION
}

#[inline]
const fn duration_to_millis_clamped(d: Duration) -> u64 {
    let millis = d.as_millis();
    if millis > u64::MAX as u128 {
        u64::MAX
    } else {
        #[allow(clippy::cast_possible_truncation)]
        {
            millis as u64
        }
    }
}

/// A structured summary of a completed or yielded command execution.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutputSummary {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    stdout_bytes: usize,
    #[serde(default)]
    stderr_bytes: usize,
    /// Absent in records written before this field existed, which is why it is an option rather
    /// than a plain count. A missing value is "not recorded", and reading it as `0` would tell a
    /// replayed truncation that nothing survived a command whose output is sitting right there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retained_bytes: Option<usize>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(default)]
    is_truncated: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ExecOutputSummary {
    /// Creates a new output summary for the given captured output.
    #[must_use]
    pub fn new(stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        let stdout = stdout.into();
        let stderr = stderr.into();
        let stdout_bytes = stdout.len();
        let stderr_bytes = stderr.len();
        Self {
            schema_version: EXEC_SCHEMA_VERSION,
            stdout,
            stderr,
            stdout_bytes,
            stderr_bytes,
            retained_bytes: Some(stdout_bytes.saturating_add(stderr_bytes)),
            duration_ms: 0,
            exit_code: None,
            is_truncated: false,
            unknown: Unknown::new(),
        }
    }

    /// Sets the byte count for standard output.
    #[must_use]
    pub const fn with_stdout_bytes(mut self, bytes: usize) -> Self {
        self.stdout_bytes = bytes;
        self
    }

    /// Sets the byte count for standard error.
    #[must_use]
    pub const fn with_stderr_bytes(mut self, bytes: usize) -> Self {
        self.stderr_bytes = bytes;
        self
    }

    /// Sets how many of the produced bytes are still held.
    #[must_use]
    pub const fn with_retained_bytes(mut self, bytes: usize) -> Self {
        self.retained_bytes = Some(bytes);
        self
    }

    /// Sets the duration elapsed.
    #[must_use]
    pub const fn with_duration(mut self, duration: Duration) -> Self {
        self.duration_ms = duration_to_millis_clamped(duration);
        self
    }

    /// Sets the duration in milliseconds.
    #[must_use]
    pub const fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    /// Sets the exit code.
    #[must_use]
    pub const fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    /// Sets whether output was truncated.
    #[must_use]
    pub const fn with_truncated(mut self, truncated: bool) -> Self {
        self.is_truncated = truncated;
        self
    }

    /// Captured standard output text.
    #[must_use]
    pub fn stdout(&self) -> &str {
        &self.stdout
    }

    /// Captured standard error text.
    #[must_use]
    pub fn stderr(&self) -> &str {
        &self.stderr
    }

    /// Total bytes collected on stdout.
    #[must_use]
    pub const fn stdout_bytes(&self) -> usize {
        self.stdout_bytes
    }

    /// Total bytes collected on stderr.
    #[must_use]
    pub const fn stderr_bytes(&self) -> usize {
        self.stderr_bytes
    }

    /// Bytes of the produced output still held after any per-stream truncation.
    ///
    /// The pair with [`Self::total_bytes`] is what a truncation record is made of; it counts source
    /// bytes only, never the marker text that stands in for the omitted middle.
    ///
    /// A record written before this was tracked falls back to the length of the text it carries.
    /// That is an over-count for a truncated record, by exactly the marker standing in for the
    /// omitted middle, and it is the closest true statement available: the alternative is reporting
    /// that nothing survived a command whose output the same record is holding.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
            .unwrap_or_else(|| self.stdout.len().saturating_add(self.stderr.len()))
    }

    /// Wall duration elapsed.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }

    /// Wall duration elapsed in milliseconds.
    #[must_use]
    pub const fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// Process exit code, if finished.
    #[must_use]
    pub const fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Whether output was truncated.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.is_truncated
    }

    /// Combined output length in bytes, saturating at [`usize::MAX`].
    ///
    /// The two counts are set independently through the builder and deserialized from records this
    /// process did not write, so nothing upstream bounds their sum. Plain `+` split the failure
    /// across profiles — a panic in debug, a wrapped total in release — and the wrapped total is
    /// the dangerous one: a summary would report *less* output than either stream alone, which
    /// reads as a plausible number rather than as a fault.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.stdout_bytes.saturating_add(self.stderr_bytes)
    }

    /// Schema version of this output summary.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A bounded byte buffer retaining the beginning (head) and end (tail) of a stream.
///
/// When total input exceeds `capacity`, the first `capacity / 2` bytes are preserved in `head`
/// and the last `capacity - (capacity / 2)` bytes are kept in `tail`. Any intermediate bytes
/// are dropped, with exact tracking of omitted bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadTailBuffer {
    capacity: usize,
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total_bytes: usize,
}

/// Retained stream content past a cursor, split wherever the buffer dropped bytes.
///
/// [`HeadTailBuffer::read_from`] renders an omission marker in a gap's place, which is the right
/// answer for a reader that will show the output to someone and the wrong one for a reader that
/// searches it: the marker is prose the stream never produced, so a literal search would report a
/// match on the words describing the gap. This carries the gap as structure instead.
#[derive(Debug, Clone)]
pub(crate) struct RetainedRead {
    /// Text continuing directly from the requested offset. Empty when bytes were dropped there.
    pub(crate) continued: String,
    /// Text resuming after dropped bytes, when this read crossed a gap.
    pub(crate) resumed: Option<String>,
    /// The offset a following read resumes from.
    pub(crate) next_offset: usize,
}

impl HeadTailBuffer {
    /// Creates a new buffer with the specified maximum byte capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let head_cap = capacity / 2;
        let tail_cap = capacity.saturating_sub(head_cap);
        Self {
            capacity,
            head_cap,
            tail_cap,
            head: Vec::with_capacity(head_cap),
            tail: VecDeque::with_capacity(tail_cap),
            total_bytes: 0,
        }
    }

    /// Pushes a slice of raw bytes into the buffer.
    pub fn push_bytes(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());

        let mut remaining = chunk;
        if self.head.len() < self.head_cap {
            let needed = self.head_cap - self.head.len();
            let take = needed.min(remaining.len());
            self.head.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
        }

        if !remaining.is_empty() && self.tail_cap > 0 {
            if remaining.len() >= self.tail_cap {
                let start = remaining.len() - self.tail_cap;
                self.tail.clear();
                self.tail.extend(&remaining[start..]);
            } else {
                let overflow = (self.tail.len() + remaining.len()).saturating_sub(self.tail_cap);
                if overflow > 0 {
                    self.tail.drain(..overflow);
                }
                self.tail.extend(remaining);
            }
        }
    }

    /// Total number of bytes pushed into this buffer.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Maximum byte capacity configured for this buffer.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns true if the buffer has exceeded its capacity and omitted bytes.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.total_bytes > self.capacity
    }

    /// Number of bytes omitted between the head and tail portions.
    #[must_use]
    pub fn omitted_bytes(&self) -> usize {
        self.total_bytes.saturating_sub(self.retained_bytes())
    }

    /// Number of bytes still held, across both the head and the tail.
    ///
    /// Reported separately from [`Self::total_bytes`] because a truncation record needs both
    /// numbers: how much the stream produced, and how much of it survived. Deriving the second
    /// from the rendered string would count the omission marker as retained output.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    /// Converts retained content to a UTF-8 string, using lossy conversion for invalid sequences.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        self.read_from(0).map_or_else(String::new, |(text, _)| text)
    }

    /// Reads everything retained at or after absolute stream offset `offset`.
    ///
    /// Returns the text and the offset a following read resumes from, or `None` when `offset` has
    /// already reached the end of the stream — which is what lets a poller tell "nothing new yet"
    /// from "here is more output".
    ///
    /// The returned offset is the stream total rather than `offset` plus the text length, because
    /// the two differ exactly when this buffer dropped bytes: the text carries an omission marker
    /// in their place, and advancing by its length would leave the cursor pointing into a gap that
    /// no later read can ever deliver. Omitted bytes are announced rather than skipped silently, so
    /// a reader does not attribute the surviving tail to the point where the head stopped.
    #[must_use]
    pub fn read_from(&self, offset: usize) -> Option<(String, usize)> {
        if offset >= self.total_bytes {
            return None;
        }
        let head_end = self.head.len();
        let tail_start = self.total_bytes.saturating_sub(self.tail.len());
        let mut text = String::new();
        if offset < head_end {
            text.push_str(&String::from_utf8_lossy(&self.head[offset..]));
        }
        let omitted = tail_start.saturating_sub(head_end.max(offset));
        if omitted > 0 {
            let _ = write!(text, "\n... [omitted {omitted} bytes] ...\n");
        }
        let tail_from = offset.saturating_sub(tail_start).min(self.tail.len());
        if tail_from < self.tail.len() {
            let tail: Vec<u8> = self.tail.iter().skip(tail_from).copied().collect();
            text.push_str(&String::from_utf8_lossy(&tail));
        }
        Some((text, self.total_bytes))
    }

    /// Reads retained content at or after `offset`, reporting a gap rather than describing one.
    ///
    /// The companion to [`Self::read_from`] for a caller that searches the output instead of
    /// showing it. Surviving text arrives in the order the stream produced it, split into the part
    /// that continues from `offset` and the part that resumes after dropped bytes, so a search can
    /// tell that the two are not adjacent. See [`RetainedRead`] for why the rendered marker is
    /// unusable here.
    pub(crate) fn read_retained_from(&self, offset: usize) -> Option<RetainedRead> {
        if offset >= self.total_bytes {
            return None;
        }
        let head_end = self.head.len();
        let tail_start = self.total_bytes.saturating_sub(self.tail.len());
        let mut continued = String::new();
        if offset < head_end {
            continued.push_str(&String::from_utf8_lossy(&self.head[offset..]));
        }
        let tail_from = offset.saturating_sub(tail_start).min(self.tail.len());
        let tail = if tail_from < self.tail.len() {
            let bytes: Vec<u8> = self.tail.iter().skip(tail_from).copied().collect();
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            String::new()
        };
        // Adjacent exactly when nothing was dropped between where the head stopped and where the
        // tail begins; otherwise the tail is a separate run that no carried prefix can reach into.
        let resumed = if tail_start > head_end.max(offset) {
            Some(tail)
        } else {
            continued.push_str(&tail);
            None
        };
        Some(RetainedRead {
            continued,
            resumed,
            next_offset: self.total_bytes,
        })
    }

    /// Returns the raw retained bytes as a continuous vector.
    #[must_use]
    pub fn to_retained_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.retained_bytes());
        out.extend_from_slice(&self.head);
        out.extend(self.tail.iter().copied());
        out
    }
}
