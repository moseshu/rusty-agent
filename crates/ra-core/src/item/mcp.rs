//! MCP tool discovery and approval items.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compat::{SchemaVersion, Unknown};

/// Current MCP-item schema version.
pub const MCP_ITEM_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Summary of a tool reported by an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    schema_version: SchemaVersion,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl McpTool {
    /// Creates a tool summary.
    #[must_use]
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            schema_version: MCP_ITEM_SCHEMA_VERSION,
            name: name.into(),
            description: None,
            input_schema,
            unknown: Unknown::new(),
        }
    }

    /// Sets the description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Description.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Input JSON schema.
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Normalized result of an MCP `list_tools` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpListTools {
    schema_version: SchemaVersion,
    server: String,
    #[serde(default)]
    tools: Vec<McpTool>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl McpListTools {
    /// Creates a tool list.
    #[must_use]
    pub fn new(server: impl Into<String>, tools: Vec<McpTool>) -> Self {
        Self {
            schema_version: MCP_ITEM_SCHEMA_VERSION,
            server: server.into(),
            tools,
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Registered server name.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Tool summaries.
    #[must_use]
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Approval request issued before an MCP tool is executed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpApprovalRequest {
    schema_version: SchemaVersion,
    request_id: String,
    server: String,
    tool_name: String,
    arguments: Value,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl McpApprovalRequest {
    /// Creates an approval request.
    #[must_use]
    pub fn new(
        request_id: impl Into<String>,
        server: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self {
            schema_version: MCP_ITEM_SCHEMA_VERSION,
            request_id: request_id.into(),
            server: server.into(),
            tool_name: tool_name.into(),
            arguments,
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Approval request ID.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Registered server name.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
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

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Host decision for an MCP approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpApprovalResponse {
    schema_version: SchemaVersion,
    request_id: String,
    approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl McpApprovalResponse {
    /// Creates an approval decision.
    #[must_use]
    pub fn new(request_id: impl Into<String>, approved: bool) -> Self {
        Self {
            schema_version: MCP_ITEM_SCHEMA_VERSION,
            request_id: request_id.into(),
            approved,
            reason: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the decision reason.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Approval request ID.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Whether the request was approved.
    #[must_use]
    pub const fn approved(&self) -> bool {
        self.approved
    }

    /// Decision reason.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
