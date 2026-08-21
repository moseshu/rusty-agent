//! Events emitted by a model adapter while streaming.
//!
//! # This is the model channel, not the run channel
//!
//! Two streams exist in an agent framework and they carry different authority:
//!
//! | Channel | Producer | Knows about |
//! | --- | --- | --- |
//! | model | a provider adapter | one wire call: raw provider events, and the items lifted from them |
//! | run | the runner | turns, handoffs, which public agent is now speaking |
//!
//! [`ModelStreamEvent`] is the first one. It deliberately cannot express a run-level fact such as
//! "the public agent changed after a handoff": an adapter has no knowledge of agents or handoffs,
//! so a type that let it emit one would permit a state that can never be valid. The reference
//! implementation keeps the same split — `Model.stream_response` yields provider events, while the
//! consumer-facing union adds the run-level variants on top.
//!
//! The run channel lives in `ra-runtime`, wrapping this enum rather than extending it. Keeping
//! them apart also means adding a run event later does not change the return type of every
//! provider adapter.
//!
//! Terminal backfill is on this channel rather than above it: the adapter is the only participant
//! that can assemble it, because usage, the transport identifiers and the output ordering are facts
//! about the whole wire call and not about any frame. Aggregating deltas into a rendered view stays
//! a consumer's job.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ProviderKey;
use crate::{
    compat::{SchemaVersion, Unknown},
    item::{ModelResponse, RunItem},
};

/// Current model-stream-event schema version.
pub const MODEL_STREAM_EVENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// One event from a streaming model call.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    /// Provider event preserved for diagnostics and protocol-aware consumers.
    RawResponse(RawResponseEvent),
    /// A normalized item became available. Lifting belongs to the adapter, so this is still the
    /// model channel.
    RunItem(RunItemStreamEvent),
    /// The terminal facts of this call, assembled once the stream said it was finished.
    ///
    /// **A successful stream ends with exactly one of these, and it is the last event.** Deltas
    /// alone are not a response: usage arrives on a frame of its own or not at all, the transport
    /// identifiers are not in the body, and the ordering of output items is a property of the whole
    /// call rather than of any one frame. A consumer that folded the deltas itself would be
    /// re-deriving all of that from a vocabulary this crate does not even promise to keep stable,
    /// once per protocol.
    ///
    /// Its absence is therefore meaningful: a stream that stops without one did not produce a turn,
    /// whatever it emitted along the way.
    ///
    /// Boxed because it is the largest variant by far and the deltas are what actually flow. It is
    /// carried directly rather than wrapped in an event struct like its neighbours, because
    /// [`ModelResponse`] already versions itself and already retains unknown fields — a wrapper
    /// would add a second copy of both.
    Completed(Box<ModelResponse>),
}

/// Raw provider event isolated behind the provider registration key.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawResponseEvent {
    schema_version: SchemaVersion,
    provider: ProviderKey,
    event_type: String,
    payload: Value,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RawResponseEvent {
    /// Creates a raw provider event.
    #[must_use]
    pub fn new(provider: ProviderKey, event_type: impl Into<String>, payload: Value) -> Self {
        Self {
            schema_version: MODEL_STREAM_EVENT_SCHEMA_VERSION,
            provider,
            event_type: event_type.into(),
            payload,
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Provider registration identity.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    /// Provider event type.
    #[must_use]
    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    /// Untouched provider payload.
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A named normalized run-item event.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunItemStreamEvent {
    schema_version: SchemaVersion,
    name: String,
    item: Box<RunItem>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RunItemStreamEvent {
    /// Creates a normalized run-item event.
    #[must_use]
    pub fn new(name: impl Into<String>, item: RunItem) -> Self {
        Self {
            schema_version: MODEL_STREAM_EVENT_SCHEMA_VERSION,
            name: name.into(),
            item: Box::new(item),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Semantic event name.
    ///
    /// Open string rather than a closed enum, for now. The vocabulary that drives a UI —
    /// `message_output_created`, `tool_called`, `tool_output`, `reasoning_item_created`, the
    /// `mcp_*` and `handoff_*` families — is produced by the runner mapping step items, which does
    /// not exist yet. Closing the set here would mean guessing it from the reference
    /// implementation instead of from the code that emits it, and a wrong closed set is harder to
    /// correct than an open one. A future milestone closes it; until then a typo yields an event
    /// no consumer handles, which is the known cost.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Normalized run item.
    #[must_use]
    pub fn item(&self) -> &RunItem {
        self.item.as_ref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
