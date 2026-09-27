//! Rollout files: one JSONL file per rollout, one line per run segment.
//!
//! A port of the file half of the reference's `sandbox/memory/rollouts.py`, and of the rollout id
//! rules in `sandbox/memory/manager.py`. Building a segment from a run's result belongs with the
//! runner hooks that call it.

use std::sync::Arc;

use ra_core::{
    error::{Error, Result},
    sandbox::{ErrorCode, PosixPath, SandboxSession},
};
use serde::{Deserialize, Serialize};

use super::json::dumps_compact;
use crate::sandbox::sandbox_error;

/// How a run segment ended.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutTerminalState {
    /// The run produced its final output.
    Completed,
    /// The run stopped for approval.
    Interrupted,
    /// The run was cancelled.
    Cancelled,
    /// The run failed.
    Failed,
    /// The run hit its turn limit.
    MaxTurnsExceeded,
    /// A guardrail stopped the run.
    GuardrailTripped,
}

impl RolloutTerminalState {
    /// The reference's name for it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::MaxTurnsExceeded => "max_turns_exceeded",
            Self::GuardrailTripped => "guardrail_tripped",
        }
    }
}

/// How a run segment ended, as its rollout line records it.
///
/// The reference's `RolloutTerminalMetadata`; every field is written, `null` included, in its
/// order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutTerminalMetadata {
    terminal_state: RolloutTerminalState,
    exception_type: Option<String>,
    exception_message: Option<String>,
    has_final_output: bool,
}

impl RolloutTerminalMetadata {
    /// A segment that ended as `terminal_state`, without an exception.
    #[must_use]
    pub const fn new(terminal_state: RolloutTerminalState, has_final_output: bool) -> Self {
        Self {
            terminal_state,
            exception_type: None,
            exception_message: None,
            has_final_output,
        }
    }

    /// Records the exception the segment ended with: its type name, and its message unless empty.
    #[must_use]
    pub fn with_exception(
        mut self,
        exception_type: impl Into<String>,
        exception_message: impl Into<String>,
    ) -> Self {
        let message = exception_message.into();
        self.exception_type = Some(exception_type.into());
        self.exception_message = (!message.is_empty()).then_some(message);
        self
    }

    /// How the segment ended.
    #[must_use]
    pub const fn terminal_state(&self) -> RolloutTerminalState {
        self.terminal_state
    }

    /// The type of the exception the segment ended with, if any.
    #[must_use]
    pub fn exception_type(&self) -> Option<&str> {
        self.exception_type.as_deref()
    }

    /// The message of the exception the segment ended with, if it had one.
    #[must_use]
    pub fn exception_message(&self) -> Option<&str> {
        self.exception_message.as_deref()
    }

    /// Whether the segment produced a final output.
    #[must_use]
    pub const fn has_final_output(&self) -> bool {
        self.has_final_output
    }
}

/// One JSON line, newline included, as the reference's `dump_rollout_json` writes it: no spaces,
/// non-ASCII escaped, fields in the value's own order.
///
/// # Errors
///
/// Returns the serialization error when the value cannot be represented as JSON.
pub fn dump_rollout_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    let json = dumps_compact(value).map_err(|error| {
        Error::config("rollout_contents must be valid JSON text").with_source(error)
    })?;
    Ok(format!("{json}\n"))
}

/// Whether `rollout_id` is file-safe: a letter or digit, then up to 127 letters, digits, `.`, `_`
/// or `-`.
fn is_file_safe_id(rollout_id: &str) -> bool {
    let mut characters = rollout_id.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && rollout_id.len() <= 128
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The rollout id, trimmed, having checked that it is file-safe.
///
/// # Errors
///
/// Returns a configuration error, worded as the reference's, for an id that is not.
pub fn validate_rollout_id(rollout_id: &str) -> Result<String> {
    let normalized = rollout_id.trim();
    if !is_file_safe_id(normalized) {
        return Err(Error::config(
            "Sandbox memory rollout ID must be a file-safe ID containing only letters, numbers, \
             '.', '_', or '-'.",
        ));
    }
    Ok(normalized.to_owned())
}

/// The file a rollout's segments are appended to: `<rollout_id>.jsonl`.
///
/// # Errors
///
/// As [`validate_rollout_id`].
pub fn rollout_file_name_for_rollout_id(rollout_id: &str) -> Result<String> {
    Ok(format!("{}.jsonl", validate_rollout_id(rollout_id)?))
}

/// The reference's check on a directory it keeps rollouts in: relative, inside the root, and
/// naming something.
fn validate_relative_path(name: &str, path: &PosixPath) -> Result<()> {
    if path.is_absolute() {
        return Err(Error::config(format!(
            "{name} must be relative to the sandbox workspace root, got: {path}"
        )));
    }
    if path.parts().contains(&"..") {
        return Err(Error::config(format!(
            "{name} must not escape root, got: {path}"
        )));
    }
    if path.parts().is_empty() {
        return Err(Error::config(format!("{name} must be non-empty")));
    }
    Ok(())
}

/// Reads a file, or `None` when it is not there.
async fn read_existing_bytes(
    session: &Arc<dyn SandboxSession>,
    path: &PosixPath,
) -> Result<Option<Vec<u8>>> {
    match session.read(path.as_str(), None).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => Ok(None),
        Err(error) => Err(sandbox_error(error)),
    }
}

/// Appends one segment to a rollout file under `rollouts_path`, and answers the file's path
/// relative to the workspace root.
///
/// The file is `file_name` when given; otherwise a new `<uuid>.jsonl` that does not exist yet.
///
/// The reference takes the segment as JSON text and re-serializes it; here it takes the segment
/// itself, so the order of its fields is the one it serializes in. A segment that cannot be
/// written as JSON is refused before anything is written.
///
/// `rollouts_path` and `file_name` are read as the session reads text, with a backslash as a
/// separator, so they are checked as the path the file will actually land at, and the path
/// answered is that one.
///
/// # Errors
///
/// Returns a configuration error, worded as the reference's, for a `rollouts_path` that is
/// absolute, climbs out or names nothing, for a `file_name` that is not a plain `.jsonl` name, and
/// when ten fresh names were all taken; and the session's failure.
pub async fn write_rollout<T: Serialize + ?Sized>(
    session: &Arc<dyn SandboxSession>,
    rollout_contents: &T,
    rollouts_path: &str,
    file_name: Option<&str>,
) -> Result<PosixPath> {
    let rollouts_dir = PosixPath::coerce(rollouts_path);
    validate_relative_path("rollouts_path", &rollouts_dir)?;
    let line = dump_rollout_json(rollout_contents)?;

    let destination = if let Some(file_name) = file_name {
        let requested = PosixPath::coerce(file_name.trim());
        let single_name = requested.parts().len() == 1 && !requested.is_absolute();
        // Case-sensitive, as the reference's `endswith` is.
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        let is_jsonl = requested.as_str().ends_with(".jsonl");
        if !single_name || !is_jsonl {
            return Err(Error::config("file_name must be a simple .jsonl filename"));
        }
        rollouts_dir.join(requested.as_str())
    } else {
        let mut allocated = None;
        for _ in 0..10 {
            let candidate = rollouts_dir.join(&format!("{}.jsonl", uuid::Uuid::new_v4()));
            if read_existing_bytes(session, &candidate).await?.is_none() {
                allocated = Some(candidate);
                break;
            }
        }
        allocated.ok_or_else(|| {
            Error::config(format!(
                "failed to allocate a unique rollout id under: {rollouts_dir}"
            ))
        })?
    };

    let parent = destination.join("..").normalized();
    session
        .mkdir(parent.as_str(), true, None)
        .await
        .map_err(sandbox_error)?;
    let mut contents = read_existing_bytes(session, &destination)
        .await?
        .unwrap_or_default();
    contents.extend_from_slice(line.as_bytes());
    session
        .write(destination.as_str(), contents, None)
        .await
        .map_err(sandbox_error)?;
    Ok(destination)
}
