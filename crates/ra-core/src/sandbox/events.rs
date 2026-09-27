//! The audit events a session emits around its operations, and how much of each one a sink sees.
//!
//! Ported from the reference's `session/events.py` and the event half of `session/utils.py`. Each
//! instrumented operation produces two records sharing one span id: a start event before the
//! operation runs and a finish event after it, with the outcome, the duration and, for a failure,
//! what it failed with. The service crate emits them and delivers them to sinks; this module only
//! says what they are.
//!
//! # The policy knows which of its fields were set
//!
//! Delivery merges three policies — the instrumentation's default, a per-operation one, and the
//! sink's own — and a later one overrides an earlier one **only in the fields it set**. The
//! reference reads that from its model's set-field tracking; here every field is an `Option`, and
//! `None` is "not set, use the default". A policy that sets nothing therefore changes nothing,
//! which is what makes a per-sink policy that only raises the output limit leave the operation's
//! decision about including output alone.
//!
//! # Output travels raw until a sink's policy has decided
//!
//! A finish event for a command carries what it wrote as bytes. Decoding happens per sink, after
//! that sink's policy is known, because one global decision would either truncate what a sink
//! entitled to more wanted or keep what a sink that asked for less must not see. The raw bytes are
//! never serialized: a record that leaves the process holds at most the decoded, truncated text.
//!
//! # Serialized form
//!
//! [`event_to_json_line`] is the reference's line format byte for byte — keys sorted, no spaces,
//! non-ASCII escaped — so a file one side appends to reads the same to the other. The timestamp is
//! written as the reference writes a UTC time: seconds, and microseconds when there are any, then
//! `Z`.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::error::{ErrorCode, OpName, SandboxError};

/// The version every event is written with.
pub const SANDBOX_SESSION_EVENT_VERSION: u32 = 1;

/// How many characters of standard output a sink sees by default, when it sees any.
pub const DEFAULT_MAX_STDOUT_CHARS: usize = 8_000;

/// How many characters of standard error a sink sees by default, when it sees any.
pub const DEFAULT_MAX_STDERR_CHARS: usize = 8_000;

/// Which half of an operation an event records.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventPhase {
    /// Before the operation runs.
    Start,
    /// After it returned or failed.
    Finish,
}

impl EventPhase {
    /// The phase's wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Finish => "finish",
        }
    }
}

/// How much potentially sensitive or large data an event carries to a sink.
///
/// Every field is optional so that merging can tell "set to the default" from "not set"; the
/// accessors answer with the default for a field that was not set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventPayloadPolicy {
    include_exec_output: Option<bool>,
    max_stdout_chars: Option<usize>,
    max_stderr_chars: Option<usize>,
    include_write_len: Option<bool>,
}

impl EventPayloadPolicy {
    /// A policy that sets nothing, and so answers with every default.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            include_exec_output: None,
            max_stdout_chars: None,
            max_stderr_chars: None,
            include_write_len: None,
        }
    }

    /// Whether finish events for commands carry what the command wrote. Off by default: command
    /// output is noisy and often sensitive.
    #[must_use]
    pub const fn include_exec_output(&self) -> bool {
        match self.include_exec_output {
            Some(value) => value,
            None => false,
        }
    }

    /// At most how many characters of standard output are kept, when output is included.
    #[must_use]
    pub const fn max_stdout_chars(&self) -> usize {
        match self.max_stdout_chars {
            Some(value) => value,
            None => DEFAULT_MAX_STDOUT_CHARS,
        }
    }

    /// At most how many characters of standard error are kept, when output is included.
    #[must_use]
    pub const fn max_stderr_chars(&self) -> usize {
        match self.max_stderr_chars {
            Some(value) => value,
            None => DEFAULT_MAX_STDERR_CHARS,
        }
    }

    /// Whether events keep the byte count of what was written. On by default; the bytes themselves
    /// are never in an event.
    #[must_use]
    pub const fn include_write_len(&self) -> bool {
        match self.include_write_len {
            Some(value) => value,
            None => true,
        }
    }

    /// Sets [`Self::include_exec_output`].
    #[must_use]
    pub const fn with_include_exec_output(mut self, value: bool) -> Self {
        self.include_exec_output = Some(value);
        self
    }

    /// Sets [`Self::max_stdout_chars`].
    #[must_use]
    pub const fn with_max_stdout_chars(mut self, value: usize) -> Self {
        self.max_stdout_chars = Some(value);
        self
    }

    /// Sets [`Self::max_stderr_chars`].
    #[must_use]
    pub const fn with_max_stderr_chars(mut self, value: usize) -> Self {
        self.max_stderr_chars = Some(value);
        self
    }

    /// Sets [`Self::include_write_len`].
    #[must_use]
    pub const fn with_include_write_len(mut self, value: bool) -> Self {
        self.include_write_len = Some(value);
        self
    }

    /// This policy with every field `overrides` set replaced by the value it set there.
    ///
    /// Fields `overrides` left unset keep whatever this policy says, set or not.
    #[must_use]
    pub const fn overridden_by(&self, overrides: &Self) -> Self {
        Self {
            include_exec_output: match overrides.include_exec_output {
                Some(value) => Some(value),
                None => self.include_exec_output,
            },
            max_stdout_chars: match overrides.max_stdout_chars {
                Some(value) => Some(value),
                None => self.max_stdout_chars,
            },
            max_stderr_chars: match overrides.max_stderr_chars {
                Some(value) => Some(value),
                None => self.max_stderr_chars,
            },
            include_write_len: match overrides.include_write_len {
                Some(value) => Some(value),
                None => self.include_write_len,
            },
        }
    }
}

/// What every event records, whichever phase it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxSessionEventBase {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default = "Uuid::new_v4")]
    event_id: Uuid,
    #[serde(
        default = "SystemTime::now",
        serialize_with = "serialize_timestamp",
        deserialize_with = "deserialize_timestamp"
    )]
    ts: SystemTime,
    session_id: Uuid,
    seq: u64,
    op: OpName,
    span_id: String,
    #[serde(default)]
    parent_span_id: Option<String>,
    #[serde(default)]
    trace_id: Option<String>,
    #[serde(default)]
    data: Map<String, Value>,
}

const fn default_version() -> u32 {
    SANDBOX_SESSION_EVENT_VERSION
}

impl SandboxSessionEventBase {
    /// A record for `op` on `session_id`, with a fresh identity and the current time.
    #[must_use]
    pub fn new(session_id: Uuid, seq: u64, op: OpName, span_id: impl Into<String>) -> Self {
        Self {
            version: SANDBOX_SESSION_EVENT_VERSION,
            event_id: Uuid::new_v4(),
            ts: SystemTime::now(),
            session_id,
            seq,
            op,
            span_id: span_id.into(),
            parent_span_id: None,
            trace_id: None,
            data: Map::new(),
        }
    }

    /// Replaces the operation metadata.
    #[must_use]
    pub fn with_data(mut self, data: Map<String, Value>) -> Self {
        self.data = data;
        self
    }

    /// Places the record under a trace span.
    #[must_use]
    pub fn with_trace(mut self, trace_id: Option<String>, parent_span_id: Option<String>) -> Self {
        self.trace_id = trace_id;
        self.parent_span_id = parent_span_id;
        self
    }

    /// Replaces the record's identity.
    #[must_use]
    pub const fn with_event_id(mut self, event_id: Uuid) -> Self {
        self.event_id = event_id;
        self
    }

    /// Replaces when the record was made.
    #[must_use]
    pub const fn with_ts(mut self, ts: SystemTime) -> Self {
        self.ts = ts;
        self
    }

    /// The event format's version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// This record's own identity.
    #[must_use]
    pub const fn event_id(&self) -> Uuid {
        self.event_id
    }

    /// When the record was made.
    #[must_use]
    pub const fn ts(&self) -> SystemTime {
        self.ts
    }

    /// The session the operation ran on.
    #[must_use]
    pub const fn session_id(&self) -> Uuid {
        self.session_id
    }

    /// The record's position among everything this session emitted, from 1.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// What was attempted.
    #[must_use]
    pub const fn op(&self) -> OpName {
        self.op
    }

    /// Pairs the start and finish records of one operation.
    ///
    /// Also the trace span's id when the operation ran under a trace that assigns them; otherwise
    /// an audit id of its own, `sandbox_op_` and a random hex string.
    #[must_use]
    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    /// The enclosing trace span, when there is one.
    #[must_use]
    pub fn parent_span_id(&self) -> Option<&str> {
        self.parent_span_id.as_deref()
    }

    /// The enclosing trace, when there is one.
    #[must_use]
    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.as_deref()
    }

    /// Operation-specific metadata: paths, argument vectors, sizes, exit codes.
    #[must_use]
    pub const fn data(&self) -> &Map<String, Value> {
        &self.data
    }

    /// The operation metadata, to change.
    pub const fn data_mut(&mut self) -> &mut Map<String, Value> {
        &mut self.data
    }
}

// Single-variant types enforce the concrete models' literal phase fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum StartPhase {
    #[default]
    #[serde(rename = "start")]
    Start,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum FinishPhase {
    #[default]
    #[serde(rename = "finish")]
    Finish,
}

/// The record made before an operation runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxSessionStartEvent {
    #[serde(default)]
    phase: StartPhase,
    #[serde(flatten)]
    base: SandboxSessionEventBase,
}

impl SandboxSessionStartEvent {
    /// A start record for `op` on `session_id`.
    #[must_use]
    pub fn new(session_id: Uuid, seq: u64, op: OpName, span_id: impl Into<String>) -> Self {
        Self::from_base(SandboxSessionEventBase::new(session_id, seq, op, span_id))
    }

    /// A start record carrying `base`.
    #[must_use]
    pub const fn from_base(base: SandboxSessionEventBase) -> Self {
        Self {
            base,
            phase: StartPhase::Start,
        }
    }

    /// What every record carries.
    #[must_use]
    pub const fn base(&self) -> &SandboxSessionEventBase {
        &self.base
    }

    /// What every record carries, to change.
    pub const fn base_mut(&mut self) -> &mut SandboxSessionEventBase {
        &mut self.base
    }
}

/// The record made after an operation returned or failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxSessionFinishEvent {
    #[serde(default)]
    phase: FinishPhase,
    #[serde(flatten)]
    base: SandboxSessionEventBase,
    ok: bool,
    duration_ms: f64,
    #[serde(default)]
    error_code: Option<ErrorCode>,
    #[serde(default)]
    error_type: Option<String>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    error_retryable: Option<bool>,
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
    #[serde(skip)]
    stdout_bytes: Option<Vec<u8>>,
    #[serde(skip)]
    stderr_bytes: Option<Vec<u8>>,
}

impl SandboxSessionFinishEvent {
    /// A finish record for `op` on `session_id`.
    #[must_use]
    pub fn new(
        session_id: Uuid,
        seq: u64,
        op: OpName,
        span_id: impl Into<String>,
        ok: bool,
        duration_ms: f64,
    ) -> Self {
        Self::from_base(
            SandboxSessionEventBase::new(session_id, seq, op, span_id),
            ok,
            duration_ms,
        )
    }

    /// A finish record carrying `base`.
    #[must_use]
    pub const fn from_base(base: SandboxSessionEventBase, ok: bool, duration_ms: f64) -> Self {
        Self {
            base,
            ok,
            duration_ms,
            phase: FinishPhase::Finish,
            error_code: None,
            error_type: None,
            error_message: None,
            error_retryable: None,
            stdout: None,
            stderr: None,
            stdout_bytes: None,
            stderr_bytes: None,
        }
    }

    /// Records what the operation failed with, as the reference records it: the code when the
    /// reference publishes one, the name of the class it raises, the message and the retryability.
    #[must_use]
    pub fn with_failure(mut self, error: &SandboxError) -> Self {
        self.error_code = error.error_code().reference_code();
        self.error_type = Some(error.error_code().reference_type_name().to_owned());
        self.error_message = Some(error.to_string());
        self.error_retryable = error.retryable();
        self
    }

    /// Attaches a command's raw output, for each sink's policy to decide on.
    #[must_use]
    pub fn with_output(mut self, stdout: Option<Vec<u8>>, stderr: Option<Vec<u8>>) -> Self {
        self.stdout_bytes = stdout;
        self.stderr_bytes = stderr;
        self
    }

    /// What every record carries.
    #[must_use]
    pub const fn base(&self) -> &SandboxSessionEventBase {
        &self.base
    }

    /// What every record carries, to change.
    pub const fn base_mut(&mut self) -> &mut SandboxSessionEventBase {
        &mut self.base
    }

    /// Whether the operation succeeded. A command that ran and exited non-zero did not.
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.ok
    }

    /// How long the operation took, in milliseconds.
    #[must_use]
    pub const fn duration_ms(&self) -> f64 {
        self.duration_ms
    }

    /// The failure's code, when it has one the reference publishes.
    #[must_use]
    pub const fn error_code(&self) -> Option<ErrorCode> {
        self.error_code
    }

    /// The failure's type, by the name of the class the reference raises for it.
    #[must_use]
    pub fn error_type(&self) -> Option<&str> {
        self.error_type.as_deref()
    }

    /// The failure's message.
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    /// Whether trying again is expected to help, when that is known.
    #[must_use]
    pub const fn error_retryable(&self) -> Option<bool> {
        self.error_retryable
    }

    /// Standard output, decoded and truncated, when the sink's policy includes it.
    #[must_use]
    pub fn stdout(&self) -> Option<&str> {
        self.stdout.as_deref()
    }

    /// Standard error, decoded and truncated, when the sink's policy includes it.
    #[must_use]
    pub fn stderr(&self) -> Option<&str> {
        self.stderr.as_deref()
    }

    /// Standard output as the command wrote it, until a sink's policy decides what becomes of it.
    /// Never serialized.
    #[must_use]
    pub fn stdout_bytes(&self) -> Option<&[u8]> {
        self.stdout_bytes.as_deref()
    }

    /// Standard error as the command wrote it, until a sink's policy decides what becomes of it.
    /// Never serialized.
    #[must_use]
    pub fn stderr_bytes(&self) -> Option<&[u8]> {
        self.stderr_bytes.as_deref()
    }
}

/// One audit record, of either phase. Serialized with its phase as the discriminator.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SandboxSessionEvent {
    /// Before the operation runs.
    Start(SandboxSessionStartEvent),
    /// After it returned or failed.
    Finish(SandboxSessionFinishEvent),
}

impl<'de> Deserialize<'de> for SandboxSessionEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The union requires a discriminator even though each concrete model defaults its phase.
        #[derive(Deserialize)]
        #[serde(tag = "phase", rename_all = "snake_case")]
        enum TaggedEvent {
            Start(SandboxSessionStartEvent),
            Finish(SandboxSessionFinishEvent),
        }
        match TaggedEvent::deserialize(deserializer)? {
            TaggedEvent::Start(event) => Ok(Self::Start(event)),
            TaggedEvent::Finish(event) => Ok(Self::Finish(event)),
        }
    }
}

impl SandboxSessionEvent {
    /// What every record carries.
    #[must_use]
    pub const fn base(&self) -> &SandboxSessionEventBase {
        match self {
            Self::Start(event) => &event.base,
            Self::Finish(event) => &event.base,
        }
    }

    /// What every record carries, to change.
    pub const fn base_mut(&mut self) -> &mut SandboxSessionEventBase {
        match self {
            Self::Start(event) => &mut event.base,
            Self::Finish(event) => &mut event.base,
        }
    }

    /// Which half of the operation this records.
    #[must_use]
    pub const fn phase(&self) -> EventPhase {
        match self {
            Self::Start(_) => EventPhase::Start,
            Self::Finish(_) => EventPhase::Finish,
        }
    }

    /// What was attempted.
    #[must_use]
    pub const fn op(&self) -> OpName {
        self.base().op
    }

    /// This record's identity.
    #[must_use]
    pub const fn event_id(&self) -> Uuid {
        self.base().event_id
    }

    /// The record's position in its session's sequence.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.base().seq
    }

    /// The operation metadata.
    #[must_use]
    pub const fn data(&self) -> &Map<String, Value> {
        &self.base().data
    }

    /// The finish record, when this is one.
    #[must_use]
    pub const fn as_finish(&self) -> Option<&SandboxSessionFinishEvent> {
        match self {
            Self::Finish(event) => Some(event),
            Self::Start(_) => None,
        }
    }

    /// This record as a sink governed by `policy` may see it.
    ///
    /// The reference's `_apply_policy`: the byte count of a write is dropped unless the policy keeps
    /// it; a finish record's command output is dropped — text and raw bytes alike — unless the
    /// policy includes it, and otherwise decoded and truncated to the policy's limits. The input is
    /// left as it was, so each sink gets its own copy.
    #[must_use]
    pub fn with_policy_applied(&self, policy: &EventPayloadPolicy) -> Self {
        let mut out = self.clone();
        if !policy.include_write_len() {
            out.base_mut().data.remove("bytes");
        }
        if let Self::Finish(event) = &mut out {
            if policy.include_exec_output() {
                if let Some(bytes) = &event.stdout_bytes {
                    event.stdout = Some(safe_decode(bytes, policy.max_stdout_chars()));
                }
                if let Some(bytes) = &event.stderr_bytes {
                    event.stderr = Some(safe_decode(bytes, policy.max_stderr_chars()));
                }
            } else {
                event.stdout = None;
                event.stderr = None;
                event.stdout_bytes = None;
                event.stderr_bytes = None;
            }
        }
        out
    }
}

impl From<SandboxSessionStartEvent> for SandboxSessionEvent {
    fn from(event: SandboxSessionStartEvent) -> Self {
        Self::Start(event)
    }
}

impl From<SandboxSessionFinishEvent> for SandboxSessionEvent {
    fn from(event: SandboxSessionFinishEvent) -> Self {
        Self::Finish(event)
    }
}

/// Parses an event payload — from JSON, say — into the record of the phase it names.
///
/// # Errors
///
/// Returns the parse failure: an unknown or missing phase, or a field of the wrong shape.
pub fn validate_sandbox_session_event(
    value: Value,
) -> Result<SandboxSessionEvent, serde_json::Error> {
    serde_json::from_value(value)
}

/// Decodes command output as UTF-8, replacing what is not, and keeps at most `max_chars`
/// characters of it.
///
/// The limit is on decoded characters rather than bytes, as the reference's is, and a cut is
/// marked with `…`.
#[must_use]
pub fn safe_decode(bytes: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => {
            let mut kept = text[..cut].to_owned();
            kept.push('…');
            kept
        }
        None => text.into_owned(),
    }
}

/// One event as a line of the reference's JSONL format, newline included.
///
/// Keys are sorted at every level, separators carry no spaces, and every character outside
/// printable ASCII is escaped, which is what the reference's `json.dumps(..., sort_keys=True)`
/// writes. The raw output bytes are not part of it.
///
/// # Panics
///
/// Never: an event always serializes.
#[must_use]
pub fn event_to_json_line(event: &SandboxSessionEvent) -> String {
    let value = serde_json::to_value(event).unwrap_or(Value::Null);
    let mut line = String::new();
    write_canonical_json(&value, &mut line);
    line.push('\n');
    line
}

/// Writes `value` the way the reference's `json.dumps(sort_keys=True, separators=(",", ":"))`
/// does with its default ASCII escaping.
fn write_canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            let _ = write!(out, "{number}");
        }
        Value::String(text) => write_ascii_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_ascii_string(key, out);
                out.push(':');
                write_canonical_json(&fields[key], out);
            }
            out.push('}');
        }
    }
}

/// A JSON string with everything outside the printable ASCII range escaped, surrogate pairs for
/// characters beyond the basic plane.
fn write_ascii_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' => out.push(character),
            other => {
                let mut units = [0_u16; 2];
                for unit in other.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

// --- timestamps ----------------------------------------------------------------------------------

/// Writes a time as the reference writes a UTC time: `YYYY-MM-DDTHH:MM:SS`, then `.ffffff` when
/// there are microseconds, then `Z`. Times before the epoch are written as the epoch.
#[must_use]
pub fn format_event_timestamp(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs();
    let micros = since_epoch.subsec_micros();
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let mut out = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        of_day / 3_600,
        (of_day % 3_600) / 60,
        of_day % 60
    );
    if micros != 0 {
        let _ = write!(out, ".{micros:06}");
    }
    out.push('Z');
    out
}

/// Reads a time written as RFC 3339 — a `Z` or a numeric offset, any number of fractional digits
/// down to microseconds — or as seconds since the epoch.
///
/// # Errors
///
/// Returns a description of what did not parse.
pub fn parse_event_timestamp(value: &Value) -> Result<SystemTime, String> {
    match value {
        Value::Number(number) => {
            let seconds = number
                .as_f64()
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .ok_or_else(|| format!("invalid timestamp {number}"))?;
            let duration = Duration::try_from_secs_f64(seconds)
                .map_err(|_| format!("invalid timestamp {number}"))?;
            UNIX_EPOCH
                .checked_add(duration)
                .ok_or_else(|| format!("invalid timestamp {number}"))
        }
        Value::String(text) => {
            parse_rfc3339(text).ok_or_else(|| format!("invalid timestamp {text:?}"))
        }
        other => Err(format!("invalid timestamp {other}")),
    }
}

fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = bytes.get(range)?;
        if slice.is_empty() || !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse().ok()
    };
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    let hour = digits(11..13)?;
    let minute = digits(14..16)?;
    let second = digits(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut index = 19;
    let mut micros: i64 = 0;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let fraction = &text[start..index];
        if fraction.is_empty() {
            return None;
        }
        let kept: String = fraction
            .chars()
            .chain(std::iter::repeat('0'))
            .take(6)
            .collect();
        micros = kept.parse().ok()?;
    }

    let offset_seconds = match bytes.get(index) {
        Some(b'Z' | b'z') if index + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if index + 6 == bytes.len() && bytes[index + 3] == b':' => {
            let hours = digits(index + 1..index + 3)?;
            let minutes = digits(index + 4..index + 6)?;
            let magnitude = hours * 3_600 + minutes * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let total = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds;
    let total = u64::try_from(total).ok()?;
    let micros = u64::try_from(micros).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(total) + Duration::from_micros(micros))
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date `days` after 1970-01-01.
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

fn serialize_timestamp<S: Serializer>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format_event_timestamp(*time))
}

fn deserialize_timestamp<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<SystemTime, D::Error> {
    let value = Value::deserialize(deserializer)?;
    parse_event_timestamp(&value).map_err(serde::de::Error::custom)
}
