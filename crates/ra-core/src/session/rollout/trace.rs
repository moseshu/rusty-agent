// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Local observations carried by the rollout port, separate from ordinary span fields.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Host selection for local observations. Payload capture requires explicit opt-in.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RolloutTraceMode {
    /// Do not construct local trace observations.
    #[default]
    Disabled,
    /// Retain topology and stable metadata only.
    Metadata,
    /// Include model and conversation payloads in explicit local records.
    Payloads,
}

/// An observation before a recorder assigns its persistent envelope.
///
/// Kinds emitted by the runtime are `model_request`, `model_response`, `model_error`, and
/// `next_step`. Payloads belong here, never in ordinary tracing fields. `fields` contains only
/// identities, counts, stable decisions and error codes; error messages are not metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutTraceEvent {
    /// Stable event kind, with unknown kinds preserved by readers.
    kind: String,
    /// Content-free event metadata as a JSON object.
    fields: Value,
    /// Explicitly selected content, absent when payload capture is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
}

/// Append-only local trace envelope, persisted by the existing rollout writer.
///
/// IDs are export identities, not tracing registry IDs, which may be reused after a span closes.
/// Sequence numbers are local to one capturing recorder and start at one. The payload is inline,
/// so a persisted event never references a payload file that has not been written yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutTraceRecord {
    /// Envelope schema version; currently one.
    schema_version: u32,
    /// Capture stream identity, allowing a session to contain several independently captured runs.
    capture_id: String,
    /// Monotonic recorder sequence number.
    seq: u64,
    /// Observation time in milliseconds since the Unix epoch.
    at_unix_ms: u64,
    /// Stable trace identity shared by descendants of a captured root span.
    trace_id: String,
    /// Stable span identity, absent for run boundaries outside a span.
    span_id: Option<String>,
    /// Nearest captured parent, absent for root spans.
    parent_id: Option<String>,
    /// Observation and optional content.
    event: RolloutTraceEvent,
}

impl RolloutTraceEvent {
    /// Creates an observation. Metadata must be a JSON object; the replay reader validates it.
    /// A null payload represents absent content and is normalized to `None`.
    #[must_use]
    pub fn new(kind: impl Into<String>, fields: Value, payload: Option<Value>) -> Self {
        Self {
            kind: kind.into(),
            fields,
            payload: payload.filter(|value| !value.is_null()),
        }
    }

    /// Stable event kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Content-free metadata.
    #[must_use]
    pub const fn fields(&self) -> &Value {
        &self.fields
    }

    /// Explicit content, if captured.
    #[must_use]
    pub const fn payload(&self) -> Option<&Value> {
        self.payload.as_ref()
    }

    /// Removes content when exporting through a metadata-only recorder.
    #[must_use]
    pub fn without_payload(mut self) -> Self {
        self.payload = None;
        self
    }
}

impl RolloutTraceRecord {
    /// Creates a version-one envelope with recorder-assigned identities and sequence.
    #[must_use]
    pub const fn new(
        capture_id: String,
        seq: u64,
        at_unix_ms: u64,
        trace_id: String,
        span_id: Option<String>,
        parent_id: Option<String>,
        event: RolloutTraceEvent,
    ) -> Self {
        Self {
            schema_version: 1,
            capture_id,
            seq,
            at_unix_ms,
            trace_id,
            span_id,
            parent_id,
            event,
        }
    }

    /// Envelope schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }
    /// Capture stream identity.
    #[must_use]
    pub fn capture_id(&self) -> &str {
        &self.capture_id
    }
    /// Monotonic sequence within this capture.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }
    /// Observation time in Unix milliseconds.
    #[must_use]
    pub const fn at_unix_ms(&self) -> u64 {
        self.at_unix_ms
    }
    /// Stable trace identity.
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }
    /// Stable span identity, if attributed.
    #[must_use]
    pub fn span_id(&self) -> Option<&str> {
        self.span_id.as_deref()
    }
    /// Nearest captured parent identity.
    #[must_use]
    pub fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }
    /// Observation and explicitly captured content.
    #[must_use]
    pub const fn event(&self) -> &RolloutTraceEvent {
        &self.event
    }
}
