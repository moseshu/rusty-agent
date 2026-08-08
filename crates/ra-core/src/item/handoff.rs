//! Agent handoff calls and completion records.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AgentId, CallId};
use crate::compat::{SchemaVersion, Unknown};

/// Current handoff-item schema version.
pub const HANDOFF_ITEM_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// A model request to transfer control to another agent.
///
/// `tool_name` records the name the model actually called. A handoff reaches the wire as an
/// ordinary function call, so replaying this item on a later turn needs that name — and the agent
/// that receives control normally no longer advertises the handoff that led to it, which makes a
/// reverse lookup by [`AgentId`] unavailable exactly when replay matters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffCall {
    schema_version: SchemaVersion,
    call_id: CallId,
    target_agent: AgentId,
    arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl HandoffCall {
    /// Creates a handoff call.
    #[must_use]
    pub fn new(call_id: CallId, target_agent: AgentId, arguments: Value) -> Self {
        Self {
            schema_version: HANDOFF_ITEM_SCHEMA_VERSION,
            call_id,
            target_agent,
            arguments,
            tool_name: None,
            unknown: Unknown::new(),
        }
    }

    /// Records the tool name the model used to request this handoff.
    #[must_use]
    pub fn with_tool_name(mut self, tool_name: impl Into<String>) -> Self {
        self.tool_name = Some(tool_name.into());
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Call ID.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Target agent.
    #[must_use]
    pub const fn target_agent(&self) -> &AgentId {
        &self.target_agent
    }

    /// Handoff arguments.
    #[must_use]
    pub const fn arguments(&self) -> &Value {
        &self.arguments
    }

    /// Tool name the model called, when the producing adapter recorded it.
    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        self.tool_name.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A historical record of a completed handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffOutput {
    schema_version: SchemaVersion,
    call_id: CallId,
    source_agent: AgentId,
    target_agent: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl HandoffOutput {
    /// Creates a handoff completion record.
    #[must_use]
    pub fn new(call_id: CallId, source_agent: AgentId, target_agent: AgentId) -> Self {
        Self {
            schema_version: HANDOFF_ITEM_SCHEMA_VERSION,
            call_id,
            source_agent,
            target_agent,
            note: None,
            unknown: Unknown::new(),
        }
    }

    /// Adds a short note for the next agent.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Call ID.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Source agent.
    #[must_use]
    pub const fn source_agent(&self) -> &AgentId {
        &self.source_agent
    }

    /// Target agent.
    #[must_use]
    pub const fn target_agent(&self) -> &AgentId {
        &self.target_agent
    }

    /// Handoff note.
    #[must_use]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
