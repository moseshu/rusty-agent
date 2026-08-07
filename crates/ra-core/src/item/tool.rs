//! Tool calls, outputs, and approval control items.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CallId;
use crate::compat::{SchemaVersion, Unknown};

/// Current tool-item schema version.
pub const TOOL_ITEM_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// A provider-neutral tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    schema_version: SchemaVersion,
    call_id: CallId,
    name: String,
    arguments: Value,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolCall {
    /// Creates a tool call.
    #[must_use]
    pub fn new(call_id: CallId, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            schema_version: TOOL_ITEM_SCHEMA_VERSION,
            call_id,
            name: name.into(),
            arguments,
            unknown: Unknown::new(),
        }
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

    /// Tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Parsed argument JSON.
    #[must_use]
    pub const fn arguments(&self) -> &Value {
        &self.arguments
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A provider-neutral tool-call output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallOutput {
    schema_version: SchemaVersion,
    call_id: CallId,
    output: Value,
    #[serde(default)]
    is_error: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolCallOutput {
    /// Creates a successful output.
    #[must_use]
    pub fn new(call_id: CallId, output: Value) -> Self {
        Self {
            schema_version: TOOL_ITEM_SCHEMA_VERSION,
            call_id,
            output,
            is_error: false,
            unknown: Unknown::new(),
        }
    }

    /// Marks this as a tool failure observation that can be replayed to the model, rather than a
    /// framework error.
    #[must_use]
    pub const fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// ID that pairs this output with its call.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// Output JSON. R1-2/R2-7 will add a typed multimodal `ToolResult` representation.
    #[must_use]
    pub const fn output(&self) -> &Value {
        &self.output
    }

    /// Whether this is a tool failure observation.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        self.is_error
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A tool call awaiting host approval.
///
/// This is a control-plane record. It may be stored in a session or carried by an interruption,
/// but it must never be sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolApproval {
    schema_version: SchemaVersion,
    call_id: CallId,
    tool_name: String,
    arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolApproval {
    /// Creates an approval item.
    #[must_use]
    pub fn new(call_id: CallId, tool_name: impl Into<String>, arguments: Value) -> Self {
        Self {
            schema_version: TOOL_ITEM_SCHEMA_VERSION,
            call_id,
            tool_name: tool_name.into(),
            arguments,
            namespace: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the tool namespace.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
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

    /// Tool name.
    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Call arguments.
    #[must_use]
    pub const fn arguments(&self) -> &Value {
        &self.arguments
    }

    /// Tool namespace.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
