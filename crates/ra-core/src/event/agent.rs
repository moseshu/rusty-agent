//! Multi-agent coordination and lifecycle events.

use core::fmt;
use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    compat::{SchemaVersion, Unknown},
    item::AgentId,
    state::RunId,
};

/// Current schema version of the multi-agent event payloads.
///
/// One version for the family, on the same terms as
/// [`EXEC_EVENT_SCHEMA_VERSION`](super::exec::EXEC_EVENT_SCHEMA_VERSION).
pub const AGENT_EVENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn default_schema_version() -> SchemaVersion {
    AGENT_EVENT_SCHEMA_VERSION
}

/// Strongly typed operation identifier for agent handoffs and sub-tasks.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentOperationId(String);

impl AgentOperationId {
    /// Creates an operation identifier from a string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentOperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for AgentOperationId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for AgentOperationId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Execution status of an agent.
///
/// # An open label, and why
///
/// A status is something a host renders and a trace records; nothing in the framework routes
/// control flow on it. So a name this build has no variant for is kept verbatim as
/// [`Self::Custom`] instead of being rejected. The alternative is worse than it looks: a status
/// enum that fails on an unknown name fails the *whole* event around it, so one new label written
/// by a newer build costs an older reader the sequence number, the run attribution and the
/// timestamp as well.
///
/// A value the framework does route on grows the opposite way — by `#[non_exhaustive]` plus an
/// explicit failure on unknown wire values — because there a wrong guess silently reroutes.
///
/// # The wire form is a bare string in both directions
///
/// [`Self::Custom`] serializes as its name with no wrapper, so it is indistinguishable on the wire
/// from a variant this build happens to know. That is the point rather than a leak: a name that
/// becomes a real variant in a later version reads back as that variant, which is what lets a
/// value survive being written by a new build, read and rewritten by an old one, and read again by
/// the new one — unchanged.
///
/// A consequence worth naming: a custom name that a later version turns into a real variant reads
/// back as that variant, so building [`Self::Custom`] with a name this build already knows is a
/// value that does not survive its own round trip. [`Self::custom`] and the [`From`] impls all
/// normalize, which is why they exist rather than leaving callers to construct the variant.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Agent is idle and awaiting instructions.
    Idle,
    /// Agent is executing turns.
    Running,
    /// Agent is waiting for user or external tool input.
    WaitingForInput,
    /// Agent completed its goal successfully.
    Completed,
    /// Agent encountered an unrecoverable failure.
    Failed,
    /// A status this build has no variant for, kept verbatim.
    Custom(Cow<'static, str>),
}

impl AgentStatus {
    /// The status named `name`, as the variant this build has for it or as [`Self::Custom`].
    ///
    /// A `&'static str` reaches [`Self::Custom`] without allocating, which is the reason for the
    /// [`Cow`]: a host's own status names are compile-time constants, and they are re-created on
    /// every status change.
    #[must_use]
    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self {
        let name = name.into();
        Self::known(&name).unwrap_or(Self::Custom(name))
    }

    /// The name of this status, byte-identical to its serialized form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingForInput => "waiting_for_input",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Custom(name) => name,
        }
    }

    /// The variant this build has for `name`, if it has one.
    fn known(name: &str) -> Option<Self> {
        match name {
            "idle" => Some(Self::Idle),
            "running" => Some(Self::Running),
            "waiting_for_input" => Some(Self::WaitingForInput),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

impl From<&str> for AgentStatus {
    fn from(name: &str) -> Self {
        Self::known(name).unwrap_or_else(|| Self::Custom(Cow::Owned(name.to_owned())))
    }
}

impl From<String> for AgentStatus {
    fn from(name: String) -> Self {
        match Self::known(&name) {
            Some(known) => known,
            None => Self::Custom(Cow::Owned(name)),
        }
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for AgentStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from(String::deserialize(deserializer)?))
    }
}

/// Emitted when a child or peer agent is spawned.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpawnedEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    child_agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<AgentOperationId>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl AgentSpawnedEvent {
    /// Creates a new agent spawned event.
    #[must_use]
    pub fn new(child_agent_id: AgentId) -> Self {
        Self {
            schema_version: AGENT_EVENT_SCHEMA_VERSION,
            child_agent_id,
            child_run_id: None,
            operation_id: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the child run identity.
    #[must_use]
    pub fn with_child_run_id(mut self, run_id: RunId) -> Self {
        self.child_run_id = Some(run_id);
        self
    }

    /// Sets the operation identifier.
    #[must_use]
    pub fn with_operation_id(mut self, op_id: impl Into<AgentOperationId>) -> Self {
        self.operation_id = Some(op_id.into());
        self
    }

    /// Child agent identity.
    #[must_use]
    pub const fn child_agent_id(&self) -> &AgentId {
        &self.child_agent_id
    }

    /// Child run identity, if started.
    #[must_use]
    pub const fn child_run_id(&self) -> Option<&RunId> {
        self.child_run_id.as_ref()
    }

    /// Operation identifier.
    #[must_use]
    pub const fn operation_id(&self) -> Option<&AgentOperationId> {
        self.operation_id.as_ref()
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

/// Emitted when a message is routed between agents.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageSentEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    from_agent: AgentId,
    to_agent: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    preview: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl AgentMessageSentEvent {
    /// Creates a new agent message sent event.
    #[must_use]
    pub fn new(from_agent: AgentId, to_agent: AgentId, preview: impl Into<String>) -> Self {
        Self {
            schema_version: AGENT_EVENT_SCHEMA_VERSION,
            from_agent,
            to_agent,
            message_id: None,
            preview: preview.into(),
            unknown: Unknown::new(),
        }
    }

    /// Sets the message identifier.
    #[must_use]
    pub fn with_message_id(mut self, id: impl Into<String>) -> Self {
        self.message_id = Some(id.into());
        self
    }

    /// Source agent.
    #[must_use]
    pub const fn from_agent(&self) -> &AgentId {
        &self.from_agent
    }

    /// Destination agent.
    #[must_use]
    pub const fn to_agent(&self) -> &AgentId {
        &self.to_agent
    }

    /// Message identifier.
    #[must_use]
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// Content preview.
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
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

/// Emitted when an agent changes its execution status.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatusChangedEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    previous_status: AgentStatus,
    new_status: AgentStatus,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl AgentStatusChangedEvent {
    /// Creates a new status changed event.
    #[must_use]
    pub fn new(previous_status: AgentStatus, new_status: AgentStatus) -> Self {
        Self {
            schema_version: AGENT_EVENT_SCHEMA_VERSION,
            previous_status,
            new_status,
            unknown: Unknown::new(),
        }
    }

    /// Previous status.
    #[must_use]
    pub const fn previous_status(&self) -> &AgentStatus {
        &self.previous_status
    }

    /// New status.
    #[must_use]
    pub const fn new_status(&self) -> &AgentStatus {
        &self.new_status
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

/// Emitted when an agent completes its task.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCompletedEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    outcome: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl AgentCompletedEvent {
    /// Creates a new agent completed event.
    #[must_use]
    pub fn new(outcome: impl Into<String>) -> Self {
        Self {
            schema_version: AGENT_EVENT_SCHEMA_VERSION,
            outcome: outcome.into(),
            unknown: Unknown::new(),
        }
    }

    /// Task outcome.
    #[must_use]
    pub fn outcome(&self) -> &str {
        &self.outcome
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

/// Emitted when an agent is closed or disposed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentClosedEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    reason: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl AgentClosedEvent {
    /// Creates a new agent closed event.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            schema_version: AGENT_EVENT_SCHEMA_VERSION,
            reason: reason.into(),
            unknown: Unknown::new(),
        }
    }

    /// Closure reason.
    #[must_use]
    pub fn reason(&self) -> &str {
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

/// The family of multi-agent orchestration events.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// Agent spawned.
    ///
    /// The spawning side is the envelope's [`agent_id`](super::HostEvent::agent_id), not a field
    /// on the payload. A parent field here would be a second place the same fact is written, free
    /// to disagree with the first; the envelope already carries exactly one attribution and every
    /// family answers "who did this" from it.
    Spawned(AgentSpawnedEvent),
    /// Inter-agent message sent.
    MessageSent(AgentMessageSentEvent),
    /// Status updated.
    StatusChanged(AgentStatusChangedEvent),
    /// Agent finished execution.
    Completed(AgentCompletedEvent),
    /// Agent resource closed.
    Closed(AgentClosedEvent),
    /// Forward-compatible unknown agent event kind.
    Unknown(serde_json::Value),
}

impl Serialize for AgentEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Known<'a> {
            Spawned(&'a AgentSpawnedEvent),
            MessageSent(&'a AgentMessageSentEvent),
            StatusChanged(&'a AgentStatusChangedEvent),
            Completed(&'a AgentCompletedEvent),
            Closed(&'a AgentClosedEvent),
        }

        match self {
            Self::Spawned(e) => Known::Spawned(e).serialize(serializer),
            Self::MessageSent(e) => Known::MessageSent(e).serialize(serializer),
            Self::StatusChanged(e) => Known::StatusChanged(e).serialize(serializer),
            Self::Completed(e) => Known::Completed(e).serialize(serializer),
            Self::Closed(e) => Known::Closed(e).serialize(serializer),
            Self::Unknown(val) => val.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for AgentEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut val = serde_json::Value::deserialize(deserializer)?;
        let kind_val = val.as_object_mut().and_then(|map| map.remove("kind"));
        match kind_val {
            Some(serde_json::Value::String(ref s)) if s == "spawned" => serde_json::from_value(val)
                .map(Self::Spawned)
                .map_err(serde::de::Error::custom),
            Some(serde_json::Value::String(ref s)) if s == "message_sent" => {
                serde_json::from_value(val)
                    .map(Self::MessageSent)
                    .map_err(serde::de::Error::custom)
            }
            Some(serde_json::Value::String(ref s)) if s == "status_changed" => {
                serde_json::from_value(val)
                    .map(Self::StatusChanged)
                    .map_err(serde::de::Error::custom)
            }
            Some(serde_json::Value::String(ref s)) if s == "completed" => {
                serde_json::from_value(val)
                    .map(Self::Completed)
                    .map_err(serde::de::Error::custom)
            }
            Some(serde_json::Value::String(ref s)) if s == "closed" => serde_json::from_value(val)
                .map(Self::Closed)
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
