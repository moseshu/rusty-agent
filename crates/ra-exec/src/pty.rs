//! Interactive PTY and standard input command semantics.

use ra_core::compat::{SchemaVersion, Unknown};
use serde::{Deserialize, Serialize};

use crate::{EXEC_SCHEMA_VERSION, session::ExecSessionId};

const fn default_schema_version() -> SchemaVersion {
    EXEC_SCHEMA_VERSION
}

/// Terminal execution mode for a session.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    /// Attached to a pseudo-terminal (PTY) supporting raw character stream interactions.
    Pty,
    /// Standard non-interactive pipe (pipes stdout/stderr; only poll or interrupt permitted).
    Pipe,
}

/// An interactive stdin or poll command directed to a session.
///
/// # Precise Stdin Semantics
///
/// - **No Implicit Newlines**: Characters are written verbatim as raw bytes. No trailing `\n` is
///   ever automatically appended.
/// - **Empty Poll**: When `chars` is `None` or empty and `is_interrupt` is `false`, the command
///   acts as a read/poll without writing to stdin.
/// - **Interrupt**: When `is_interrupt` is `true`, a Ctrl-C / `\x03` signal is delivered.
/// - **Non-TTY Rejection**: A non-TTY (piped) session rejects non-empty character input.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StdinCommand {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    session_id: ExecSessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chars: Option<String>,
    #[serde(default)]
    is_interrupt: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl StdinCommand {
    /// Creates a write command with verbatim character input.
    #[must_use]
    pub fn write(session_id: impl Into<ExecSessionId>, chars: impl Into<String>) -> Self {
        Self {
            schema_version: EXEC_SCHEMA_VERSION,
            session_id: session_id.into(),
            chars: Some(chars.into()),
            is_interrupt: false,
            unknown: Unknown::new(),
        }
    }

    /// Creates a read/poll command that sends no input to stdin.
    #[must_use]
    pub fn poll(session_id: impl Into<ExecSessionId>) -> Self {
        Self {
            schema_version: EXEC_SCHEMA_VERSION,
            session_id: session_id.into(),
            chars: None,
            is_interrupt: false,
            unknown: Unknown::new(),
        }
    }

    /// Creates an interrupt command (Ctrl-C / SIGINT).
    #[must_use]
    pub fn interrupt(session_id: impl Into<ExecSessionId>) -> Self {
        Self {
            schema_version: EXEC_SCHEMA_VERSION,
            session_id: session_id.into(),
            chars: None,
            is_interrupt: true,
            unknown: Unknown::new(),
        }
    }

    /// Schema version of this command.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Target session identifier.
    #[must_use]
    pub const fn session_id(&self) -> &ExecSessionId {
        &self.session_id
    }

    /// Characters to write to stdin, if present.
    #[must_use]
    pub fn chars(&self) -> Option<&str> {
        self.chars.as_deref()
    }

    /// Whether this is an interrupt command.
    #[must_use]
    pub const fn is_interrupt(&self) -> bool {
        self.is_interrupt
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Validates whether this command is admissible for the given terminal mode.
    ///
    /// # Errors
    ///
    /// Returns [`StdinValidationError::NonTtyInputRejected`] if character input is sent to a
    /// non-TTY pipe session.
    pub fn validate(&self, mode: TerminalMode) -> Result<(), StdinValidationError> {
        if mode == TerminalMode::Pipe
            && let Some(chars) = &self.chars
            && !chars.is_empty()
        {
            return Err(StdinValidationError::NonTtyInputRejected);
        }
        Ok(())
    }
}

/// Errors occurring during stdin command validation.
#[non_exhaustive]
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum StdinValidationError {
    /// Non-TTY session rejected character input.
    #[error(
        "non-TTY execution session does not accept character input (only poll or interrupt allowed)"
    )]
    NonTtyInputRejected,
}
