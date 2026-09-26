//! Running something interactively, and what comes back while it runs.
//!
//! An interactive process outlives the call that started it. The call returns what has been
//! produced so far plus a handle, and a later call writes input, waits again, and returns more. The
//! handle is what ties the two together.

use std::collections::HashSet;
use std::hash::BuildHasher;

use serde::{Deserialize, Serialize};

use super::session::ShellInvocation;
use super::token_truncation::formatted_truncate_text_with_token_count;
use super::types::User;

/// The shortest wait for output a terminal call makes, in milliseconds.
pub const PTY_YIELD_TIME_MS_MIN: u64 = 250;
/// The shortest wait a call that sends nothing makes, in milliseconds.
pub const PTY_EMPTY_YIELD_TIME_MS_MIN: u64 = 5_000;
/// The longest wait for output a terminal call makes, in milliseconds.
pub const PTY_YIELD_TIME_MS_MAX: u64 = 30_000;

/// How many interactive processes one session keeps before it ends one to make room.
pub const PTY_PROCESSES_MAX: usize = 64;
/// The process count at which a session starts warning that it is approaching the limit.
pub const PTY_PROCESSES_WARNING: usize = 60;
/// How many of the most recently used processes are never ended to make room.
pub const PTY_PROCESSES_PROTECTED_RECENT: usize = 8;

/// The smallest process id a session hands out.
pub const PTY_PROCESS_ID_MIN: i64 = 1_000;
/// One past the largest process id a session hands out.
pub const PTY_PROCESS_ID_MAX_EXCLUSIVE: i64 = 100_000;

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
    ///
    /// Carried because the reference's signature carries it; the local backend ignores it, as the
    /// reference's does, since an interactive process ends when it is told to rather than on a
    /// clock.
    pub timeout_s: Option<f64>,
    /// Whether a shell sits between the session and the command. The default is the login shell,
    /// as for a one-shot command.
    pub shell: ShellInvocation,
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

    /// Puts a different shell, or none, between the session and the command.
    #[must_use]
    pub fn with_shell(mut self, shell: ShellInvocation) -> Self {
        self.shell = shell;
        self
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

/// Holds a requested wait within the bounds every terminal call observes.
#[must_use]
pub fn clamp_pty_yield_time_ms(yield_time_ms: u64) -> u64 {
    yield_time_ms.clamp(PTY_YIELD_TIME_MS_MIN, PTY_YIELD_TIME_MS_MAX)
}

/// The wait a write makes: clamped, and never shorter than [`PTY_EMPTY_YIELD_TIME_MS_MIN`] when it
/// sends nothing.
#[must_use]
pub fn resolve_pty_write_yield_time_ms(yield_time_ms: u64, input_empty: bool) -> u64 {
    let normalized = clamp_pty_yield_time_ms(yield_time_ms);
    if input_empty {
        normalized.max(PTY_EMPTY_YIELD_TIME_MS_MIN)
    } else {
        normalized
    }
}

/// Picks an unused process id at random from the session's range.
///
/// Random rather than sequential, as the reference's is. The randomness is the version-4
/// identifier generator's, which this crate already uses, rather than a second source.
#[must_use]
pub fn allocate_pty_process_id<S: BuildHasher>(
    used_process_ids: &HashSet<PtyProcessId, S>,
) -> PtyProcessId {
    let span = u128::try_from(PTY_PROCESS_ID_MAX_EXCLUSIVE - PTY_PROCESS_ID_MIN).unwrap_or(1);
    loop {
        let offset = uuid::Uuid::new_v4().as_u128() % span;
        let offset = i64::try_from(offset).unwrap_or_default();
        let process_id = PtyProcessId(PTY_PROCESS_ID_MIN + offset);
        if !used_process_ids.contains(&process_id) {
            return process_id;
        }
    }
}

/// What a session knows about one interactive process when choosing which to end.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtyProcessMeta<T> {
    process_id: PtyProcessId,
    last_used: T,
    exited: bool,
}

impl<T: Copy> PtyProcessMeta<T> {
    /// Describes one process: which it is, when it was last used — larger is more recent — and
    /// whether it has already exited.
    #[must_use]
    pub const fn new(process_id: PtyProcessId, last_used: T, exited: bool) -> Self {
        Self {
            process_id,
            last_used,
            exited,
        }
    }

    /// The process.
    #[must_use]
    pub const fn process_id(&self) -> PtyProcessId {
        self.process_id
    }

    /// When it was last used.
    #[must_use]
    pub const fn last_used(&self) -> T {
        self.last_used
    }

    /// Whether it has already exited.
    #[must_use]
    pub const fn exited(&self) -> bool {
        self.exited
    }
}

/// Chooses which interactive process to end when a session is full.
///
/// The [`PTY_PROCESSES_PROTECTED_RECENT`] most recently used are never chosen. Among the rest, the
/// least recently used one that has already exited goes first, and failing that the least recently
/// used one still running. `None` only when every process is protected.
#[must_use]
pub fn process_id_to_prune_from_meta<T: PartialOrd + Copy>(
    meta: &[PtyProcessMeta<T>],
) -> Option<PtyProcessId> {
    if meta.is_empty() {
        return None;
    }

    let mut by_recency = meta.to_vec();
    by_recency.sort_by(|left, right| {
        right
            .last_used
            .partial_cmp(&left.last_used)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let protected: HashSet<PtyProcessId> = by_recency
        .iter()
        .take(PTY_PROCESSES_PROTECTED_RECENT)
        .map(|entry| entry.process_id)
        .collect();

    let mut least_recent = meta.to_vec();
    least_recent.sort_by(|left, right| {
        left.last_used
            .partial_cmp(&right.last_used)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    least_recent
        .iter()
        .find(|entry| entry.exited && !protected.contains(&entry.process_id))
        .or_else(|| {
            least_recent
                .iter()
                .find(|entry| !protected.contains(&entry.process_id))
        })
        .map(|entry| entry.process_id)
}

/// Cuts terminal output to `max_output_tokens`, reporting the original size when it was cut.
#[must_use]
pub fn truncate_text_by_tokens(
    text: &str,
    max_output_tokens: Option<u64>,
) -> (String, Option<u64>) {
    formatted_truncate_text_with_token_count(text, max_output_tokens)
}
