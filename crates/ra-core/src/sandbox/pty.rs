//! Running something interactively, and what comes back while it runs.
//!
//! An interactive process outlives the call that started it. The call returns what has been
//! produced so far plus a handle, and a later call writes input, waits again, and returns more. The
//! handle is what ties the two together.

use serde::{Deserialize, Serialize};

use super::types::User;

/// A running interactive process.
///
/// An opaque number rather than an operating-system process id: it identifies a process *to this
/// session*, and a session that reused a pid it did not own would address someone else's work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PtyProcessId(pub i64);

impl std::fmt::Display for PtyProcessId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// What an interactive call produced before it gave the turn back.
///
/// A missing exit code means the process is still running, which is the ordinary case: the call
/// returned because it had waited long enough, not because anything finished.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PtyExecUpdate {
    /// The process this output came from, absent when it has already finished.
    pub process_id: Option<PtyProcessId>,
    /// Output produced since the last update.
    pub output: Vec<u8>,
    /// The exit status, once there is one.
    pub exit_code: Option<i32>,
    /// How much output there was before truncation, when any was dropped.
    ///
    /// Present only when the output was cut short, so a caller can tell a short command from a
    /// truncated one rather than guessing from the length.
    pub original_token_count: Option<u64>,
}

impl PtyExecUpdate {
    /// Reports output from a process that is still running.
    #[must_use]
    pub fn running(process_id: PtyProcessId, output: Vec<u8>) -> Self {
        Self {
            process_id: Some(process_id),
            output,
            exit_code: None,
            original_token_count: None,
        }
    }

    /// Reports the last output from a process that has finished.
    #[must_use]
    pub fn finished(output: Vec<u8>, exit_code: i32) -> Self {
        Self {
            process_id: None,
            output,
            exit_code: Some(exit_code),
            original_token_count: None,
        }
    }

    /// Records how much output there was before it was cut short.
    #[must_use]
    pub const fn with_original_token_count(mut self, count: u64) -> Self {
        self.original_token_count = Some(count);
        self
    }

    /// Whether the process is still running.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.exit_code.is_none()
    }
}

/// What a caller is asking a session to start interactively.
#[derive(Debug, Clone, Default)]
pub struct PtyStartRequest {
    /// The command and its arguments.
    pub command: Vec<String>,
    /// How long to wait for the whole command, in seconds.
    pub timeout_s: Option<f64>,
    /// Whether a terminal is allocated, as opposed to plain pipes.
    pub tty: bool,
    /// The account to run as, or the session's default.
    pub user: Option<User>,
    /// How long to wait for output before giving the turn back, in seconds.
    pub yield_time_s: Option<f64>,
    /// The ceiling on returned output, in tokens.
    pub max_output_tokens: Option<u64>,
}

impl PtyStartRequest {
    /// Asks to start a command interactively.
    #[must_use]
    pub fn new(command: impl IntoIterator<Item = String>) -> Self {
        Self {
            command: command.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Allocates a terminal for the command.
    #[must_use]
    pub const fn with_tty(mut self, tty: bool) -> Self {
        self.tty = tty;
        self
    }

    /// Gives the turn back after `yield_time_s` seconds without waiting for the command.
    #[must_use]
    pub const fn with_yield_time_s(mut self, yield_time_s: f64) -> Self {
        self.yield_time_s = Some(yield_time_s);
        self
    }

    /// Caps the returned output.
    #[must_use]
    pub const fn with_max_output_tokens(mut self, max_output_tokens: u64) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    /// Runs as `user` instead of the session's default.
    #[must_use]
    pub fn as_user(mut self, user: User) -> Self {
        self.user = Some(user);
        self
    }
}

/// What a caller is sending to a running interactive process.
#[derive(Debug, Clone)]
pub struct PtyWriteRequest {
    /// The process to write to.
    pub process_id: PtyProcessId,
    /// The characters to send, which may be empty.
    ///
    /// Empty is not a no-op: it is how a caller waits for more output without sending anything.
    pub chars: String,
    /// How long to wait for output before giving the turn back, in seconds.
    pub yield_time_s: Option<f64>,
    /// The ceiling on returned output, in tokens.
    pub max_output_tokens: Option<u64>,
}

impl PtyWriteRequest {
    /// Sends characters to a running process.
    #[must_use]
    pub fn new(process_id: PtyProcessId, chars: impl Into<String>) -> Self {
        Self {
            process_id,
            chars: chars.into(),
            yield_time_s: None,
            max_output_tokens: None,
        }
    }

    /// Waits for output without sending anything.
    #[must_use]
    pub const fn poll(process_id: PtyProcessId) -> Self {
        Self {
            process_id,
            chars: String::new(),
            yield_time_s: None,
            max_output_tokens: None,
        }
    }

    /// Gives the turn back after `yield_time_s` seconds.
    #[must_use]
    pub const fn with_yield_time_s(mut self, yield_time_s: f64) -> Self {
        self.yield_time_s = Some(yield_time_s);
        self
    }

    /// Caps the returned output.
    #[must_use]
    pub const fn with_max_output_tokens(mut self, max_output_tokens: u64) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }
}
