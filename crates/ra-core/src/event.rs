//! Unified host event envelope, emission, and observability taxonomy.
//!
//! # Two Orthogonal Event Channels
//!
//! The framework enforces a strict architectural boundary between two distinct event channels:
//!
//! 1. **Model-Visible Channel ([`ToolOutput`](crate::tool::ToolOutput))**:
//!    Returned to the model to drive the next turn of the execution loop. Structured metadata
//!    ([`ObservationMetadata`](crate::tool::ObservationMetadata)) is formatted cleanly into text
//!    for model consumption.
//! 2. **Host-Visible Channel ([`HostEvent`])**:
//!    Emitted synchronously to host observability infrastructure for UI streaming, telemetry,
//!    session logging, and rollout persistence. Never projected into the LLM context.
//!
//! # Sequence Allocation and Run Binding
//!
//! Every [`HostEvent`] receives a sequence number `seq` allocated monotonically per [`RunId`]
//! via [`EventSeqAllocator`]. Callers cannot invent or inject unverified sequence numbers;
//! events are allocated and dispatched through [`HostEventEmitter`].

pub mod agent;
pub mod exec;
pub mod sink;
pub mod timestamp;

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use self::{
    agent::{AgentEvent, AgentOperationId},
    exec::ExecEvent,
    sink::{FnHostEventSink, HostEventSink, InMemoryHostEventSink, NoopHostEventSink},
    timestamp::EventTimestamp,
};
use crate::{
    compat::{SchemaVersion, Unknown},
    error::Result,
    item::AgentId,
    state::{EventSeqAllocator, RunId},
};

/// The current schema version for host event envelopes.
pub const HOST_EVENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn default_host_event_schema_version() -> SchemaVersion {
    HOST_EVENT_SCHEMA_VERSION
}

/// The body payload of a host event envelope.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEventBody {
    /// Process execution event.
    Exec(ExecEvent),
    /// Multi-agent orchestration event.
    Agent(AgentEvent),
    /// Forward-compatible unknown event family.
    Unknown {
        /// Family identifier tag.
        family: String,
        /// Raw payload value.
        data: serde_json::Value,
    },
}

impl From<ExecEvent> for HostEventBody {
    fn from(e: ExecEvent) -> Self {
        Self::Exec(e)
    }
}

impl From<AgentEvent> for HostEventBody {
    fn from(a: AgentEvent) -> Self {
        Self::Agent(a)
    }
}

impl Serialize for HostEventBody {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Envelope<'a> {
            family: &'a str,
            data: &'a serde_json::Value,
        }

        match self {
            Self::Exec(exec) => {
                let data = serde_json::to_value(exec).map_err(serde::ser::Error::custom)?;
                let envelope = Envelope {
                    family: "exec",
                    data: &data,
                };
                envelope.serialize(serializer)
            }
            Self::Agent(agent) => {
                let data = serde_json::to_value(agent).map_err(serde::ser::Error::custom)?;
                let envelope = Envelope {
                    family: "agent",
                    data: &data,
                };
                envelope.serialize(serializer)
            }
            Self::Unknown { family, data } => {
                let envelope = Envelope { family, data };
                envelope.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for HostEventBody {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(deserializer)?;
        let family = val.get("family").and_then(|f| f.as_str()).unwrap_or("");
        let data = val.get("data").cloned().unwrap_or(serde_json::Value::Null);

        match family {
            "exec" => {
                let exec: ExecEvent =
                    serde_json::from_value(data).map_err(serde::de::Error::custom)?;
                Ok(Self::Exec(exec))
            }
            "agent" => {
                let agent: AgentEvent =
                    serde_json::from_value(data).map_err(serde::de::Error::custom)?;
                Ok(Self::Agent(agent))
            }
            other => Ok(Self::Unknown {
                family: other.to_owned(),
                data,
            }),
        }
    }
}

/// A unified event envelope for all host-level telemetry and lifecycle observations.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEvent {
    #[serde(default = "default_host_event_schema_version")]
    schema_version: SchemaVersion,
    seq: u64,
    run_id: RunId,
    agent_id: AgentId,
    at: EventTimestamp,
    body: HostEventBody,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl HostEvent {
    /// Allocates a new host event envelope bound to the given run allocator and agent identity.
    ///
    /// This is the primary constructor for live event generation. The sequence number is
    /// allocated atomically from `allocator`, ensuring intra-run monotonicity and run identity
    /// integrity.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if sequence number allocation fails.
    pub fn allocate(
        allocator: &EventSeqAllocator,
        agent_id: AgentId,
        body: HostEventBody,
    ) -> Result<Self> {
        let seq = allocator.allocate()?;
        Ok(Self {
            schema_version: HOST_EVENT_SCHEMA_VERSION,
            seq,
            run_id: allocator.run_id().clone(),
            agent_id,
            at: EventTimestamp::now(),
            body,
            unknown: Unknown::new(),
        })
    }

    /// Explicitly overrides the timestamp of this event (primarily for deterministic test fixtures).
    #[must_use]
    pub const fn with_timestamp(mut self, at: EventTimestamp) -> Self {
        self.at = at;
        self
    }

    /// Protocol schema version of the event envelope.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Monotonic sequence number within the run.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Run identity.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Agent identity.
    #[must_use]
    pub const fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// Creation timestamp.
    #[must_use]
    pub const fn at(&self) -> EventTimestamp {
        self.at
    }

    /// Payload body.
    #[must_use]
    pub const fn body(&self) -> &HostEventBody {
        &self.body
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A bound event emitter that coordinates sequence allocation and dispatch to a host sink.
#[derive(Clone)]
pub struct HostEventEmitter {
    agent_id: AgentId,
    allocator: EventSeqAllocator,
    sink: Arc<dyn HostEventSink>,
}

impl HostEventEmitter {
    /// Creates a new emitter bound to the agent, sequence allocator, and sink.
    #[must_use]
    pub fn new(
        agent_id: AgentId,
        allocator: EventSeqAllocator,
        sink: Arc<dyn HostEventSink>,
    ) -> Self {
        Self {
            agent_id,
            allocator,
            sink,
        }
    }

    /// Run identity derived from the bound sequence allocator.
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        self.allocator.run_id()
    }

    /// Bound agent identity.
    #[must_use]
    pub const fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// Bound sequence allocator.
    #[must_use]
    pub const fn allocator(&self) -> &EventSeqAllocator {
        &self.allocator
    }

    /// Bound host event sink.
    #[must_use]
    pub const fn sink(&self) -> &Arc<dyn HostEventSink> {
        &self.sink
    }

    /// Allocates the next sequence number, wraps the body in an authentic envelope, and dispatches it.
    ///
    /// Returns the allocated sequence number.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if sequence allocation is exhausted.
    pub fn emit(&self, body: impl Into<HostEventBody>) -> Result<u64> {
        let envelope = HostEvent::allocate(&self.allocator, self.agent_id.clone(), body.into())?;
        let seq = envelope.seq();
        self.sink.emit(envelope);
        Ok(seq)
    }

    /// Emits a process execution event.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if sequence allocation is exhausted.
    pub fn emit_exec(&self, event: ExecEvent) -> Result<u64> {
        self.emit(HostEventBody::Exec(event))
    }

    /// Emits a multi-agent orchestration event.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if sequence allocation is exhausted.
    pub fn emit_agent(&self, event: AgentEvent) -> Result<u64> {
        self.emit(HostEventBody::Agent(event))
    }
}
