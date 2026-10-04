//! Rollout files: one JSONL file per rollout, one line per run segment.
//!
//! A port of the reference's `sandbox/memory/rollouts.py` — the rollout files, and the segment a
//! run's result becomes — and of the rollout id rules in `sandbox/memory/manager.py`.

use std::sync::Arc;
use std::time::SystemTime;

use ra_core::{
    error::{Error, Result},
    finish::FinishReason,
    item::{MessageRole, ModelInputItem, RunItem, RunItemKind},
    sandbox::{ErrorCode, PosixPath, SandboxSession, SessionPath},
};
use serde::{Deserialize, Serialize};

use super::json::{dumps_compact, utc_isoformat};
use crate::runner::{RunOutcome, RunResult};
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
    match session.read(SessionPath::Posix(path), None).await {
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
/// `rollouts_path` and `file_name` are read as paths, as the reference's `Path(...)` reads them: a
/// backslash is part of a name, so `nested\chat.jsonl` is one file name, and the session is handed
/// the path that was checked.
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
    let rollouts_dir = PosixPath::new(rollouts_path);
    validate_relative_path("rollouts_path", &rollouts_dir)?;
    let line = dump_rollout_json(rollout_contents)?;

    let destination = if let Some(file_name) = file_name {
        let requested = PosixPath::new(file_name.trim());
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
        .mkdir(SessionPath::Posix(&parent), true, None)
        .await
        .map_err(sandbox_error)?;
    let mut contents = read_existing_bytes(session, &destination)
        .await?
        .unwrap_or_default();
    contents.extend_from_slice(line.as_bytes());
    session
        .write(SessionPath::Posix(&destination), contents, None)
        .await
        .map_err(sandbox_error)?;
    Ok(destination)
}

/// How a finished run's segment ended.
///
/// The reference's `terminal_metadata_for_result`. A run that delivered an answer completed and one
/// waiting on approvals was interrupted. The reference reports its other endings as exceptions;
/// here a run can also stop softly with a result, and the reason it stopped says which of the
/// reference's states it is: the turn cap, a cancellation, a tripped guardrail, and otherwise a
/// failure.
#[must_use]
pub fn terminal_metadata_for_result(result: &RunResult) -> RolloutTerminalMetadata {
    let reason = result.outcome().finish_reason();
    if result.final_message().is_some() || reason.is_some_and(FinishReason::is_complete) {
        return RolloutTerminalMetadata::new(RolloutTerminalState::Completed, true);
    }
    if matches!(result.outcome(), RunOutcome::Interrupted { .. }) {
        return RolloutTerminalMetadata::new(RolloutTerminalState::Interrupted, false);
    }
    let state = match reason {
        Some(FinishReason::MaxTurns) => RolloutTerminalState::MaxTurnsExceeded,
        Some(FinishReason::Cancelled) => RolloutTerminalState::Cancelled,
        Some(FinishReason::GuardrailTripped) => RolloutTerminalState::GuardrailTripped,
        _ => RolloutTerminalState::Failed,
    };
    RolloutTerminalMetadata::new(state, false)
}

/// How a run that failed ended.
///
/// The reference's `terminal_metadata_for_exception`, which classifies by the exception's class
/// name; here the error's variant says the same, and its stable code stands in for the name.
#[must_use]
pub fn terminal_metadata_for_error(error: &Error) -> RolloutTerminalMetadata {
    // A retained checkpoint does not change how the run ended: classify the original error.
    if let Error::Run { error, .. } = error {
        return terminal_metadata_for_error(error);
    }
    let state = match error {
        Error::Budget {
            kind: ra_core::error::BudgetKind::MaxTurns,
            ..
        } => RolloutTerminalState::MaxTurnsExceeded,
        Error::Guardrail { .. } => RolloutTerminalState::GuardrailTripped,
        Error::Cancelled { .. } => RolloutTerminalState::Cancelled,
        _ => RolloutTerminalState::Failed,
    };
    RolloutTerminalMetadata::new(state, false).with_exception(error.code(), error.to_string())
}

/// Whether an item is kept in a rollout segment.
///
/// The reference's `_should_include_memory_item`: conversation messages, tool calls and their
/// outputs, and MCP approvals are kept; instructions, reasoning, compaction and tool listings are
/// not. A handoff is a tool call and its output on the reference, and is kept with them.
fn is_memory_item(item: &ModelInputItem) -> bool {
    match item {
        ModelInputItem::Message(message) => message.role() != MessageRole::System,
        ModelInputItem::ToolCall(_)
        | ModelInputItem::ToolCallOutput(_)
        | ModelInputItem::HandoffCall(_)
        | ModelInputItem::HandoffOutput(_)
        | ModelInputItem::McpApprovalRequest(_)
        | ModelInputItem::McpApprovalResponse(_) => true,
        _ => false,
    }
}

/// One run segment as its rollout line records it.
///
/// The reference builds this as a dictionary; its fields are written in the reference's order,
/// with `rollout_id` second once the generation manager has set it, `interruptions` only when there
/// are some, and `final_output` only when the run delivered one. Items are written in the
/// framework's own item form.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RolloutPayload {
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    rollout_id: Option<String>,
    input: Vec<ModelInputItem>,
    generated_items: Vec<ModelInputItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    interruptions: Vec<RunItemKind>,
    terminal_metadata: RolloutTerminalMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_output: Option<String>,
}

impl RolloutPayload {
    /// The same segment, recorded under `rollout_id`.
    #[must_use]
    pub fn with_rollout_id(mut self, rollout_id: impl Into<String>) -> Self {
        self.rollout_id = Some(rollout_id.into());
        self
    }

    /// When the segment was recorded, as the reference's `isoformat` writes it.
    #[must_use]
    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }

    /// The rollout the segment is recorded under, once one is set.
    #[must_use]
    pub fn rollout_id(&self) -> Option<&str> {
        self.rollout_id.as_deref()
    }

    /// The run's input, without what memory does not keep.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// What the run produced, without what memory does not keep.
    #[must_use]
    pub fn generated_items(&self) -> &[ModelInputItem] {
        &self.generated_items
    }

    /// The approvals the run stopped for.
    #[must_use]
    pub fn interruptions(&self) -> &[RunItemKind] {
        &self.interruptions
    }

    /// How the run ended.
    #[must_use]
    pub const fn terminal_metadata(&self) -> &RolloutTerminalMetadata {
        &self.terminal_metadata
    }

    /// The text the run delivered, if it delivered any.
    #[must_use]
    pub fn final_output(&self) -> Option<&str> {
        self.final_output.as_deref()
    }
}

/// A run segment for memory: its input and generated items without what memory does not keep, the
/// approvals it stopped for, how it ended and what it delivered.
///
/// The reference's `build_rollout_payload`, stamped with the current time.
#[must_use]
pub fn build_rollout_payload(
    input: &[ModelInputItem],
    new_items: &[RunItem],
    final_output: Option<String>,
    interruptions: &[RunItem],
    terminal_metadata: RolloutTerminalMetadata,
) -> RolloutPayload {
    RolloutPayload {
        updated_at: utc_isoformat(SystemTime::now()),
        rollout_id: None,
        input: input
            .iter()
            .filter(|item| is_memory_item(item))
            .cloned()
            .collect(),
        generated_items: new_items
            .iter()
            .filter_map(RunItem::to_model_input)
            .filter(is_memory_item)
            .collect(),
        interruptions: interruptions
            .iter()
            .map(|item| item.kind().clone())
            .collect(),
        terminal_metadata,
        final_output,
    }
}

/// The segment a finished run becomes, with `input_override` as its input when given and the
/// input the run started from otherwise.
///
/// The reference's `build_rollout_payload_from_result`.
#[must_use]
pub fn build_rollout_payload_from_result(
    result: &RunResult,
    input_override: Option<&[ModelInputItem]>,
) -> RolloutPayload {
    build_rollout_payload(
        input_override.unwrap_or_else(|| result.original_input()),
        result.new_items(),
        result
            .final_message()
            .map(ra_core::item::Message::text_content),
        result.outcome().interruptions(),
        terminal_metadata_for_result(result),
    )
}
