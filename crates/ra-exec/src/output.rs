//! Execution output collection, eviction reasons, and summary metadata.

use std::time::Duration;

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
