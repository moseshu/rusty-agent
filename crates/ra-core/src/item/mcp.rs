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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    annotations: Option<Value>,
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
            title: None,
            annotations: None,
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

    /// Sets the explicit display title; annotation fallback is resolved by the metadata collector.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Retains the protocol annotations without interpreting provider-specific fields.
    #[must_use]
    pub fn with_annotations(mut self, annotations: Value) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Explicit display title, before annotations fallback.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Uninterpreted MCP tool annotations.
    #[must_use]
    pub const fn annotations(&self) -> Option<&Value> {
        self.annotations.as_ref()
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
///
/// Hosted catalogs retain complete `provider_data` for lossless replay, including declarations
/// that cannot be represented as typed tool summaries. Metadata collectors validate that data
/// independently without rewriting the provider's catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpListTools {
    schema_version: SchemaVersion,
    server: String,
    #[serde(default)]
    tools: Vec<McpTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_data: Option<Value>,
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
            provider_data: None,
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

    /// Sets the complete provider catalog used for replay and metadata collection.
    #[must_use]
    pub fn with_provider_data(mut self, data: Value) -> Self {
        self.provider_data = Some(data);
        self
    }

    /// Complete provider catalog, retained without metadata validation or normalization.
    #[must_use]
    pub const fn provider_data(&self) -> Option<&Value> {
        self.provider_data.as_ref()
    }

    /// Typed tool summaries. Hosted catalogs may instead carry their tools in `provider_data`.
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

/// An MCP invocation executed remotely by the provider, rather than a local function tool.
/// Its output and error belong to this same record; it owes no local tool-call output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpCall {
    schema_version: SchemaVersion,
    id: String,
    server: String,
    name: String,
    arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl McpCall {
    /// Schema version of this normalized item.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }
    /// Creates a provider-executed invocation. Arguments retain their protocol string verbatim.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        server: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: MCP_ITEM_SCHEMA_VERSION,
            id: id.into(),
            server: server.into(),
            name: name.into(),
            arguments: arguments.into(),
            output: None,
            error: None,
            unknown: Unknown::new(),
        }
    }
    /// Provider-assigned invocation identifier, distinct from local function call IDs.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Original server label.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
    }
    /// Original protocol tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Arguments as the provider supplied them; the client does not execute or decode them.
    #[must_use]
    pub fn arguments(&self) -> &str {
        &self.arguments
    }
    /// Output supplied by the provider.
    #[must_use]
    pub fn output(&self) -> Option<&str> {
        self.output.as_deref()
    }
    /// Error supplied by the provider.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    /// Unknown protocol fields retained through persistence and replay.
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
