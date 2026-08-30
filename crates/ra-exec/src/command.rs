//! Command execution requests, limits, and streaming cursors.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use ra_core::{
    compat::{SchemaVersion, Unknown},
    event::exec::ExecStreamKind,
};
use serde::{Deserialize, Serialize};

use crate::EXEC_SCHEMA_VERSION;

const fn default_schema_version() -> SchemaVersion {
    EXEC_SCHEMA_VERSION
}

const fn default_initial_yield_timeout_ms() -> u64 {
    10_000
}

const fn default_max_capture_bytes() -> usize {
    1024 * 1024
}

#[allow(clippy::unnecessary_wraps)]
const fn default_idle_timeout_ms() -> Option<u64> {
    Some(300_000)
}

const fn default_max_sessions() -> usize {
    64
}

/// The stricter of two optional limits, where `None` means no limit at all.
const fn tighter(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(if left < right { left } else { right }),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
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

/// Resource and timing limits applied to a command execution session.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecLimits {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default = "default_initial_yield_timeout_ms")]
    initial_yield_timeout_ms: u64,
    #[serde(default = "default_max_capture_bytes")]
    max_capture_bytes: usize,
    #[serde(
        default = "default_idle_timeout_ms",
        skip_serializing_if = "Option::is_none"
    )]
    idle_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_timeout_ms: Option<u64>,
    #[serde(default = "default_max_sessions")]
    max_sessions: usize,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Default for ExecLimits {
    fn default() -> Self {
        Self {
            schema_version: EXEC_SCHEMA_VERSION,
            initial_yield_timeout_ms: default_initial_yield_timeout_ms(),
            max_capture_bytes: default_max_capture_bytes(), // 1 MiB
            idle_timeout_ms: default_idle_timeout_ms(),
            total_timeout_ms: None,
            max_sessions: default_max_sessions(),
            unknown: Unknown::new(),
        }
    }
}

impl ExecLimits {
    /// Creates default execution limits.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns these limits with every field taken down to whichever value `ceiling` allows.
    ///
    /// **Only tightening, never loosening.** These are two different parties: a host configures a
    /// bound on what any command may consume, and a caller asks for what one command needs. If a
    /// request could raise the bound it was measured against, the bound would mean nothing — any
    /// caller wanting more would simply ask for more. This is the same rule the cancellation
    /// contract applies to deadlines, and for the same reason.
    ///
    /// `None` on a timeout means "no limit", so it loses to any limit the other side names.
    #[must_use]
    pub fn tightened_by(&self, ceiling: &Self) -> Self {
        let mut tightened = self.clone();
        tightened.initial_yield_timeout_ms = self
            .initial_yield_timeout_ms
            .min(ceiling.initial_yield_timeout_ms);
        tightened.max_capture_bytes = self.max_capture_bytes.min(ceiling.max_capture_bytes);
        tightened.idle_timeout_ms = tighter(self.idle_timeout_ms, ceiling.idle_timeout_ms);
        tightened.total_timeout_ms = tighter(self.total_timeout_ms, ceiling.total_timeout_ms);
        tightened.max_sessions = self.max_sessions.min(ceiling.max_sessions);
        tightened
    }

    /// Sets the initial yield timeout before backgrounding.
    #[must_use]
    pub const fn with_initial_yield_timeout(mut self, timeout: Duration) -> Self {
        self.initial_yield_timeout_ms = duration_to_millis_clamped(timeout);
        self
    }

    /// Sets the maximum raw capture bytes per stream.
    #[must_use]
    pub const fn with_max_capture_bytes(mut self, bytes: usize) -> Self {
        self.max_capture_bytes = bytes;
        self
    }

    /// Sets the inactivity idle timeout.
    #[must_use]
    pub const fn with_idle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.idle_timeout_ms = match timeout {
            Some(t) => Some(duration_to_millis_clamped(t)),
            None => None,
        };
        self
    }

    /// Sets the hard total lifetime timeout.
    #[must_use]
    pub const fn with_total_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.total_timeout_ms = match timeout {
            Some(t) => Some(duration_to_millis_clamped(t)),
            None => None,
        };
        self
    }

    /// Sets the maximum concurrent sessions allowed.
    #[must_use]
    pub const fn with_max_sessions(mut self, max_sessions: usize) -> Self {
        self.max_sessions = max_sessions;
        self
    }

    /// Schema version of these execution limits.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Initial yield timeout before backgrounding.
    #[must_use]
    pub const fn initial_yield_timeout(&self) -> Duration {
        Duration::from_millis(self.initial_yield_timeout_ms)
    }

    /// Maximum raw capture bytes per stream.
    #[must_use]
    pub const fn max_capture_bytes(&self) -> usize {
        self.max_capture_bytes
    }

    /// Inactivity idle timeout.
    #[must_use]
    pub const fn idle_timeout(&self) -> Option<Duration> {
        match self.idle_timeout_ms {
            Some(ms) => Some(Duration::from_millis(ms)),
            None => None,
        }
    }

    /// Hard total lifetime timeout.
    #[must_use]
    pub const fn total_timeout(&self) -> Option<Duration> {
        match self.total_timeout_ms {
            Some(ms) => Some(Duration::from_millis(ms)),
            None => None,
        }
    }

    /// Maximum concurrent sessions.
    #[must_use]
    pub const fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A fully formed command execution request.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shell: Option<PathBuf>,
    #[serde(default)]
    login: bool,
    #[serde(default)]
    limits: ExecLimits,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ExecRequest {
    /// Creates a standard execution request for `command` with default limits.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            schema_version: EXEC_SCHEMA_VERSION,
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            shell: None,
            login: false,
            limits: ExecLimits::default(),
            unknown: Unknown::new(),
        }
    }

    /// Sets the shell program that interprets [`Self::command`].
    ///
    /// `None` leaves the choice to the executor's default.
    ///
    /// **A bare name is looked up on `PATH`; an absolute path is used as given.** That is the
    /// ordinary way a program is named, and it is what makes `bash` work as a request without the
    /// caller having to know where this machine keeps it. It is not weakened by `PATH`: the command
    /// string this shell is about to interpret is arbitrary already, so a `PATH` that could
    /// substitute the shell could equally substitute anything the command runs.
    ///
    /// The place that stops being true is a host that vets the command but not the shell. Whatever
    /// admission policy eventually reads a request has to read this field too, and if it wants to
    /// require an absolute path it says so there — the check belongs with the policy that needs it,
    /// not here, where it would only refuse `bash` on behalf of hosts that never asked.
    #[must_use]
    pub fn with_shell(mut self, shell: Option<impl Into<PathBuf>>) -> Self {
        self.shell = shell.map(Into::into);
        self
    }

    /// Runs the shell as a login shell.
    #[must_use]
    pub const fn with_login(mut self, login: bool) -> Self {
        self.login = login;
        self
    }

    /// Appends arguments to the command request.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Injects an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.env.insert(key.into(), val.into());
        self
    }

    /// Sets explicit execution limits.
    #[must_use]
    pub fn with_limits(mut self, limits: ExecLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Command string.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Command arguments.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Working directory.
    #[must_use]
    pub fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }

    /// Environment variables.
    #[must_use]
    pub const fn env(&self) -> &HashMap<String, String> {
        &self.env
    }

    /// Shell program that interprets the command, when the caller named one.
    #[must_use]
    pub fn shell(&self) -> Option<&Path> {
        self.shell.as_deref()
    }

    /// Whether the shell runs as a login shell.
    #[must_use]
    pub const fn login(&self) -> bool {
        self.login
    }

    /// Schema version of this execution request.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Execution limits.
    #[must_use]
    pub const fn limits(&self) -> &ExecLimits {
        &self.limits
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A cursor pointing to an incremental position in an output stream.
///
/// # Deliberately not serializable
///
/// This is a live read position, not a record: it is handed out with a chunk and handed back on the
/// next read, and nothing persists it. It therefore carries neither a `schema_version` nor an
/// `unknown` bag, and deriving `Serialize` without them would be the worse half of both — a public
/// wire form that silently drops every field a newer build adds, which is exactly what
/// [`ra_core::compat`] exists to prevent.
///
/// Staying out of serde is also what keeps it [`Copy`], which [`Self::advance`] and the `const`
/// constructors are built on. If something does need to persist a cursor later, the two go in
/// together and `Copy` is what it costs.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExecCursor {
    offset: u64,
    stream: ExecStreamKind,
}

impl ExecCursor {
    /// Creates a cursor at the given stream offset.
    #[must_use]
    pub const fn new(stream: ExecStreamKind, offset: u64) -> Self {
        Self { offset, stream }
    }

    /// Creates a cursor at the start of stdout.
    #[must_use]
    pub const fn stdout_start() -> Self {
        Self::new(ExecStreamKind::Stdout, 0)
    }

    /// Current byte offset.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Stream kind.
    #[must_use]
    pub const fn stream(self) -> ExecStreamKind {
        self.stream
    }

    /// Advances the cursor by `bytes`, saturating at [`u64::MAX`].
    ///
    /// Saturating rather than wrapping is the point: a wrapped offset moves the cursor *backwards*,
    /// and a reader that resumes from it re-delivers output it already delivered — silently, and
    /// as far as the caller can tell those bytes really were produced twice. A saturated cursor
    /// stops advancing instead, which is visible as a stream that stopped rather than a stream
    /// that lied. Plain `+` had the same two behaviours split across profiles: a panic in debug
    /// and the wrap in release, so the case never showed up where it was tested.
    ///
    /// Saturating rather than returning an error, in turn, is because the condition needs a single
    /// stream to have produced sixteen exabytes; making every advance fallible would put error
    /// handling on the hot path of every chunk for a state that also has no recovery.
    #[must_use]
    pub const fn advance(self, bytes: u64) -> Self {
        Self {
            offset: self.offset.saturating_add(bytes),
            stream: self.stream,
        }
    }
}
