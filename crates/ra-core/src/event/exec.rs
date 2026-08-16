//! Process execution events, session identity, and eviction reasons.

use core::fmt;
use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::compat::{SchemaVersion, Unknown};

/// Current schema version of the execution event payloads.
///
/// One version for the family rather than one per payload: these structs are edited together and a
/// reader that understands one understands the rest, so a single number is the one a migration has
/// to reason about. The envelope carrying them versions itself separately — an envelope field can
/// change without any payload changing, and the reverse.
pub const EXEC_EVENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn default_schema_version() -> SchemaVersion {
    EXEC_EVENT_SCHEMA_VERSION
}

/// An opaque identifier for a command execution or PTY session.
///
/// This is distinct from conversation session history identifiers (`SessionId`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecSessionId(String);

impl ExecSessionId {
    /// Creates a session identifier from a string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Generates a new unique session identifier using a UUID v7.
    #[must_use]
    pub fn generate() -> Self {
        Self(format!("exec-{}", uuid::Uuid::now_v7()))
    }

    /// Returns the string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExecSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ExecSessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ExecSessionId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Reasons why an execution session was evicted by resource manager policy.
///
/// An **open label**, on the same terms as [`AgentStatus`](super::agent::AgentStatus): a host
/// policy grows new reasons to evict long before this enum learns them, and a reason is read and
/// rendered rather than routed on. An unknown name is kept as [`Self::Custom`] and written back
/// verbatim; rejecting it would cost the reader the whole event around it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEvictionReason {
    /// Maximum concurrent session capacity exceeded.
    CapacityExceeded,
    /// Session was idle without interaction past the idle timeout threshold.
    IdleTimeout,
    /// Total allowed process lifetime exceeded.
    TotalTimeout,
    /// Host or process manager shutdown.
    HostShutdown,
    /// A reason this build has no variant for, kept verbatim.
    Custom(Cow<'static, str>),
}

impl ExecEvictionReason {
    /// The reason named `name`, as the variant this build has for it or as [`Self::Custom`].
    ///
    /// A `&'static str` reaches [`Self::Custom`] without allocating.
    #[must_use]
    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self {
        let name = name.into();
        Self::known(&name).unwrap_or(Self::Custom(name))
    }

    /// The name of this reason, byte-identical to its serialized form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::CapacityExceeded => "capacity_exceeded",
            Self::IdleTimeout => "idle_timeout",
            Self::TotalTimeout => "total_timeout",
            Self::HostShutdown => "host_shutdown",
            Self::Custom(name) => name,
        }
    }

    /// The variant this build has for `name`, if it has one.
    fn known(name: &str) -> Option<Self> {
        match name {
            "capacity_exceeded" => Some(Self::CapacityExceeded),
            "idle_timeout" => Some(Self::IdleTimeout),
            "total_timeout" => Some(Self::TotalTimeout),
            "host_shutdown" => Some(Self::HostShutdown),
            _ => None,
        }
    }
}

impl From<&str> for ExecEvictionReason {
    fn from(name: &str) -> Self {
        Self::known(name).unwrap_or_else(|| Self::Custom(Cow::Owned(name.to_owned())))
    }
}

impl From<String> for ExecEvictionReason {
    fn from(name: String) -> Self {
        match Self::known(&name) {
            Some(known) => known,
            None => Self::Custom(Cow::Owned(name)),
        }
    }
}

impl fmt::Display for ExecEvictionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ExecEvictionReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ExecEvictionReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from(String::deserialize(deserializer)?))
    }
}

/// The stream source of an execution output chunk.
///
/// **Deliberately closed, unlike the other labels in this module.** A process has exactly these
/// three sources, so there is no newer build to be forward-compatible with; and this value is
/// routed on rather than rendered — it selects which buffer a chunk appends to and which cursor
/// advances, so a fourth name that fell into a catch-all would silently misfile output. Keeping it
/// closed is also what keeps it `Copy`, which the execution-layer cursor that embeds it and that
/// cursor's `const` constructors rely on. It grows by a new variant under `#[non_exhaustive]`, and
/// an unknown wire value fails loudly.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecStreamKind {
    /// Standard output stream.
    Stdout,
    /// Standard error stream.
    Stderr,
    /// Merged stdout and stderr stream (e.g. from PTY).
    Combined,
}

/// Why an execution yielded control back to the runner before process exit.
///
/// An **open label**, on the same terms as [`ExecEvictionReason`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecYieldReason {
    /// Initial yield timeout expired while the process was still running.
    InitialTimeout,
    /// Wait timeout expired during a poll or wait call.
    WaitTimeout,
    /// Backgrounding was requested explicitly.
    ExplicitYield,
    /// Output buffer capacity limit reached.
    BufferExceeded,
    /// A reason this build has no variant for, kept verbatim.
    Custom(Cow<'static, str>),
}

impl ExecYieldReason {
    /// The reason named `name`, as the variant this build has for it or as [`Self::Custom`].
    ///
    /// A `&'static str` reaches [`Self::Custom`] without allocating.
    #[must_use]
    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self {
        let name = name.into();
        Self::known(&name).unwrap_or(Self::Custom(name))
    }

    /// The name of this reason, byte-identical to its serialized form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::InitialTimeout => "initial_timeout",
            Self::WaitTimeout => "wait_timeout",
            Self::ExplicitYield => "explicit_yield",
            Self::BufferExceeded => "buffer_exceeded",
            Self::Custom(name) => name,
        }
    }

    /// The variant this build has for `name`, if it has one.
    fn known(name: &str) -> Option<Self> {
        match name {
            "initial_timeout" => Some(Self::InitialTimeout),
            "wait_timeout" => Some(Self::WaitTimeout),
            "explicit_yield" => Some(Self::ExplicitYield),
            "buffer_exceeded" => Some(Self::BufferExceeded),
            _ => None,
        }
    }
}

impl From<&str> for ExecYieldReason {
    fn from(name: &str) -> Self {
        Self::known(name).unwrap_or_else(|| Self::Custom(Cow::Owned(name.to_owned())))
    }
}

impl From<String> for ExecYieldReason {
    fn from(name: String) -> Self {
        match Self::known(&name) {
            Some(known) => known,
            None => Self::Custom(Cow::Owned(name)),
        }
    }
}

impl fmt::Display for ExecYieldReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ExecYieldReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ExecYieldReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from(String::deserialize(deserializer)?))
    }
}

/// Emitted when a command execution or PTY session begins.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecStartedEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    session_id: ExecSessionId,
    command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    pty: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ExecStartedEvent {
    /// Creates a new execution started event.
    #[must_use]
    pub fn new(session_id: impl Into<ExecSessionId>, command: impl Into<String>) -> Self {
        Self {
            schema_version: EXEC_EVENT_SCHEMA_VERSION,
            session_id: session_id.into(),
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            pty: false,
            pid: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets arguments for the started command.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets whether PTY was allocated.
    #[must_use]
    pub const fn with_pty(mut self, pty: bool) -> Self {
        self.pty = pty;
        self
    }

    /// Sets the operating system process identifier.
    #[must_use]
    pub const fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    /// Opaque session identifier.
    #[must_use]
    pub const fn session_id(&self) -> &ExecSessionId {
        &self.session_id
    }

    /// Command executed.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Arguments passed.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Working directory, if set.
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Whether PTY mode was enabled.
    #[must_use]
    pub const fn pty(&self) -> bool {
        self.pty
    }

    /// Process identifier, if known.
    #[must_use]
    pub const fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Schema version of this event payload.
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

/// Emitted when output is captured from a running process.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutputEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    session_id: ExecSessionId,
    stream: ExecStreamKind,
    offset: u64,
    bytes: usize,
    text: String,
    is_truncated: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ExecOutputEvent {
    /// Creates a new execution output event.
    #[must_use]
    pub fn new(
        session_id: impl Into<ExecSessionId>,
        stream: ExecStreamKind,
        offset: u64,
        bytes: usize,
        text: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: EXEC_EVENT_SCHEMA_VERSION,
            session_id: session_id.into(),
            stream,
            offset,
            bytes,
            text: text.into(),
            is_truncated: false,
            unknown: Unknown::new(),
        }
    }

    /// Sets whether this output was truncated.
    #[must_use]
    pub const fn with_truncated(mut self, truncated: bool) -> Self {
        self.is_truncated = truncated;
        self
    }

    /// Opaque session identifier.
    #[must_use]
    pub const fn session_id(&self) -> &ExecSessionId {
        &self.session_id
    }

    /// Stream kind (stdout, stderr, or combined).
    #[must_use]
    pub const fn stream(&self) -> ExecStreamKind {
        self.stream
    }

    /// Cumulative stream offset in bytes.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Number of bytes in this chunk.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Captured text content.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether this chunk was truncated.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.is_truncated
    }

    /// Schema version of this event payload.
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

/// Emitted when a running command yields before process completion.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecYieldedEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    session_id: ExecSessionId,
    duration_ms: u64,
    total_captured_bytes: usize,
    reason: ExecYieldReason,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ExecYieldedEvent {
    /// Creates a new execution yielded event.
    #[must_use]
    pub fn new(
        session_id: impl Into<ExecSessionId>,
        duration_ms: u64,
        total_captured_bytes: usize,
        reason: ExecYieldReason,
    ) -> Self {
        Self {
            schema_version: EXEC_EVENT_SCHEMA_VERSION,
            session_id: session_id.into(),
            duration_ms,
            total_captured_bytes,
            reason,
            unknown: Unknown::new(),
        }
    }

    /// Opaque session identifier.
    #[must_use]
    pub const fn session_id(&self) -> &ExecSessionId {
        &self.session_id
    }

    /// Cumulative execution duration in milliseconds.
    #[must_use]
    pub const fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// Total bytes captured across streams so far.
    #[must_use]
    pub const fn total_captured_bytes(&self) -> usize {
        self.total_captured_bytes
    }

    /// Yield reason.
    #[must_use]
    pub const fn reason(&self) -> &ExecYieldReason {
        &self.reason
    }

    /// Schema version of this event payload.
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

/// Emitted when interactive input is delivered to a session.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInteractionEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    session_id: ExecSessionId,
    input_bytes: usize,
    is_interrupt: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl TerminalInteractionEvent {
    /// Creates a new terminal interaction event.
    #[must_use]
    pub fn new(
        session_id: impl Into<ExecSessionId>,
        input_bytes: usize,
        is_interrupt: bool,
    ) -> Self {
        Self {
            schema_version: EXEC_EVENT_SCHEMA_VERSION,
            session_id: session_id.into(),
            input_bytes,
            is_interrupt,
            unknown: Unknown::new(),
        }
    }

    /// Opaque session identifier.
    #[must_use]
    pub const fn session_id(&self) -> &ExecSessionId {
        &self.session_id
    }

    /// Number of bytes sent.
    #[must_use]
    pub const fn input_bytes(&self) -> usize {
        self.input_bytes
    }

    /// Whether this interaction was an interrupt signal.
    #[must_use]
    pub const fn is_interrupt(&self) -> bool {
        self.is_interrupt
    }

    /// Schema version of this event payload.
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

/// Emitted when a process finishes and exits.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecExitedEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    session_id: ExecSessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    duration_ms: u64,
    stdout_bytes: usize,
    stderr_bytes: usize,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ExecExitedEvent {
    /// Creates a new execution exited event.
    #[must_use]
    pub fn new(
        session_id: impl Into<ExecSessionId>,
        duration_ms: u64,
        stdout_bytes: usize,
        stderr_bytes: usize,
    ) -> Self {
        Self {
            schema_version: EXEC_EVENT_SCHEMA_VERSION,
            session_id: session_id.into(),
            exit_code: None,
            duration_ms,
            stdout_bytes,
            stderr_bytes,
            unknown: Unknown::new(),
        }
    }

    /// Sets the exit code.
    #[must_use]
    pub const fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    /// Opaque session identifier.
    #[must_use]
    pub const fn session_id(&self) -> &ExecSessionId {
        &self.session_id
    }

    /// Exit status code.
    #[must_use]
    pub const fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Total duration in milliseconds.
    #[must_use]
    pub const fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// Total stdout bytes captured.
    #[must_use]
    pub const fn stdout_bytes(&self) -> usize {
        self.stdout_bytes
    }

    /// Total stderr bytes captured.
    #[must_use]
    pub const fn stderr_bytes(&self) -> usize {
        self.stderr_bytes
    }

    /// Schema version of this event payload.
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

/// Emitted when a background session is evicted by policy.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecEvictedEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    session_id: ExecSessionId,
    reason: ExecEvictionReason,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ExecEvictedEvent {
    /// Creates a new execution evicted event.
    #[must_use]
    pub fn new(session_id: impl Into<ExecSessionId>, reason: ExecEvictionReason) -> Self {
        Self {
            schema_version: EXEC_EVENT_SCHEMA_VERSION,
            session_id: session_id.into(),
            reason,
            unknown: Unknown::new(),
        }
    }

    /// Opaque session identifier.
    #[must_use]
    pub const fn session_id(&self) -> &ExecSessionId {
        &self.session_id
    }

    /// Reason for eviction.
    #[must_use]
    pub const fn reason(&self) -> &ExecEvictionReason {
        &self.reason
    }

    /// Schema version of this event payload.
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

/// The family of process execution events.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEvent {
    /// Command execution started.
    Started(ExecStartedEvent),
    /// Output produced.
    Output(ExecOutputEvent),
    /// Process yielded while remaining alive in the background.
    Yielded(ExecYieldedEvent),
    /// Terminal or standard input interaction.
    TerminalInteraction(TerminalInteractionEvent),
    /// Process exited.
    Exited(ExecExitedEvent),
    /// Session evicted or killed by policy.
    Evicted(ExecEvictedEvent),
    /// Forward-compatible unknown execution event kind.
    Unknown(serde_json::Value),
}

impl Serialize for ExecEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Known<'a> {
            Started(&'a ExecStartedEvent),
            Output(&'a ExecOutputEvent),
            Yielded(&'a ExecYieldedEvent),
            TerminalInteraction(&'a TerminalInteractionEvent),
            Exited(&'a ExecExitedEvent),
            Evicted(&'a ExecEvictedEvent),
        }

        match self {
            Self::Started(e) => Known::Started(e).serialize(serializer),
            Self::Output(e) => Known::Output(e).serialize(serializer),
            Self::Yielded(e) => Known::Yielded(e).serialize(serializer),
            Self::TerminalInteraction(e) => Known::TerminalInteraction(e).serialize(serializer),
            Self::Exited(e) => Known::Exited(e).serialize(serializer),
            Self::Evicted(e) => Known::Evicted(e).serialize(serializer),
            Self::Unknown(val) => val.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ExecEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut val = serde_json::Value::deserialize(deserializer)?;
        let kind_val = val.as_object_mut().and_then(|map| map.remove("kind"));
        match kind_val {
            Some(serde_json::Value::String(ref s)) if s == "started" => serde_json::from_value(val)
                .map(Self::Started)
                .map_err(serde::de::Error::custom),
            Some(serde_json::Value::String(ref s)) if s == "output" => serde_json::from_value(val)
                .map(Self::Output)
                .map_err(serde::de::Error::custom),
            Some(serde_json::Value::String(ref s)) if s == "yielded" => serde_json::from_value(val)
                .map(Self::Yielded)
                .map_err(serde::de::Error::custom),
            Some(serde_json::Value::String(ref s)) if s == "terminal_interaction" => {
                serde_json::from_value(val)
                    .map(Self::TerminalInteraction)
                    .map_err(serde::de::Error::custom)
            }
            Some(serde_json::Value::String(ref s)) if s == "exited" => serde_json::from_value(val)
                .map(Self::Exited)
                .map_err(serde::de::Error::custom),
            Some(serde_json::Value::String(ref s)) if s == "evicted" => serde_json::from_value(val)
                .map(Self::Evicted)
                .map_err(serde::de::Error::custom),
            other => {
                if let Some(kv) = other
                    && let Some(map) = val.as_object_mut()
                {
                    map.insert("kind".to_owned(), kv);
                }
                Ok(Self::Unknown(val))
            }
        }
    }
}
