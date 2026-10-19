//! MCP status wire types ported from Codex's app-server protocol.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current connection state, independent of inventory discovery.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpServerConnectionStatus {
    /// Configured but not connected yet.
    NotStarted,
    /// Connection is in progress.
    Starting,
    /// An initialized session is available.
    Connected,
    /// The server requires authentication.
    AuthenticationRequired,
    /// The latest operation failed.
    Failed,
    /// Connection was cancelled.
    Cancelled,
    /// Disabled by product configuration.
    Disabled,
}

/// Inventory detail requested by a client; omission means full inventory.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpServerStatusDetail {
    /// Tools, resources and resource templates.
    Full,
    /// Tools and authentication only.
    ToolsAndAuthOnly,
}

/// Codex's MCP status listing parameters.
#[non_exhaustive]
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMcpServerStatusParams {
    pub(crate) cursor: Option<String>,
    pub(crate) limit: Option<u32>,
    pub(crate) detail: Option<McpServerStatusDetail>,
    pub(crate) thread_id: Option<String>,
    pub(crate) server_name: Option<String>,
}

impl ListMcpServerStatusParams {
    /// Lists all configured servers, with full inventory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Continues from a previous response's cursor.
    #[must_use]
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
    /// Limits a page; zero selects one item, following Codex.
    #[must_use]
    pub const fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
    /// Selects inventory detail.
    #[must_use]
    pub const fn with_detail(mut self, detail: McpServerStatusDetail) -> Self {
        self.detail = Some(detail);
        self
    }
    /// Requests a thread's effective configuration, when the host supports thread scopes.
    #[must_use]
    pub fn with_thread_id(mut self, id: impl Into<String>) -> Self {
        self.thread_id = Some(id.into());
        self
    }
    /// Limits discovery to one registry name.
    #[must_use]
    pub fn with_server_name(mut self, name: impl Into<String>) -> Self {
        self.server_name = Some(name.into());
        self
    }
}

/// One server's detached status. Protocol inventory stays wire-shaped JSON, as in Codex.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    name: String,
    runtime_status: Option<McpServerConnectionStatus>,
    plugin_id: Option<String>,
    http_origin: Option<String>,
    server_info: Option<Value>,
    server_capabilities: Option<Value>,
    tools: BTreeMap<String, Value>,
    tools_error: Option<String>,
    resources: Vec<Value>,
    resource_templates: Vec<Value>,
    auth_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    lifecycle_phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lifecycle_error_code: Option<String>,
    #[serde(skip)]
    startup_error: Option<String>,
}

impl McpServerStatus {
    /// Creates an inventory-free status for a host-owned connection.
    #[must_use]
    pub fn new(name: impl Into<String>, status: McpServerConnectionStatus) -> Self {
        Self {
            name: name.into(),
            runtime_status: Some(status),
            plugin_id: None,
            http_origin: None,
            server_info: None,
            server_capabilities: None,
            tools: BTreeMap::new(),
            tools_error: None,
            resources: Vec::new(),
            resource_templates: Vec::new(),
            auth_status: "unsupported".into(),
            lifecycle_phase: None,
            lifecycle_error_code: None,
            startup_error: None,
        }
    }

    /// The configured registry identity.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Current runtime state.
    #[must_use]
    pub const fn runtime_status(&self) -> Option<McpServerConnectionStatus> {
        self.runtime_status
    }

    /// User-facing diagnostic carried to the startup notification, outside inventory JSON.
    #[must_use]
    pub fn startup_error(&self) -> Option<&str> {
        self.startup_error.as_deref()
    }

    /// Supplies the formatted startup error. The host retains the original error separately.
    #[must_use]
    pub fn with_startup_error(mut self, error: impl Into<String>) -> Self {
        self.startup_error = Some(error.into());
        self
    }

    /// Adds credential-safe lifecycle diagnostics, an explicit product extension.
    #[must_use]
    pub fn with_lifecycle_failure(
        mut self,
        phase: impl Into<String>,
        code: impl Into<String>,
    ) -> Self {
        self.lifecycle_phase = Some(phase.into());
        self.lifecycle_error_code = Some(code.into());
        self
    }

    /// Adds initialized server identity and capabilities.
    #[must_use]
    pub fn with_server_info(mut self, info: Value, capabilities: Value) -> Self {
        self.server_info = Some(info);
        self.server_capabilities = Some(capabilities);
        self
    }

    /// Adds a complete tool catalog or an explicit discovery error.
    #[must_use]
    pub fn with_tools(mut self, tools: BTreeMap<String, Value>, error: Option<String>) -> Self {
        self.tools = tools;
        self.tools_error = error;
        self
    }

    /// Adds complete resource and template catalogs.
    #[must_use]
    pub fn with_resources(mut self, resources: Vec<Value>, templates: Vec<Value>) -> Self {
        self.resources = resources;
        self.resource_templates = templates;
        self
    }
}

/// A page of server statuses, sorted by name.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMcpServerStatusResponse {
    pub(crate) data: Vec<McpServerStatus>,
    pub(crate) next_cursor: Option<String>,
}

impl ListMcpServerStatusResponse {
    /// Statuses in this page.
    #[must_use]
    pub fn data(&self) -> &[McpServerStatus] {
        &self.data
    }
    /// Cursor for the next page, or none at the end.
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }
}

/// Parameters for the explicitly selected product reconnect extension.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerReconnectParams {
    pub(crate) name: Option<String>,
    #[serde(default = "default_failed_only")]
    pub(crate) failed_only: bool,
}
const fn default_failed_only() -> bool {
    true
}

/// Parameters for the explicitly selected product toggle extension.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerToggleParams {
    pub(crate) name: String,
    pub(crate) enabled: bool,
}

impl Default for McpServerReconnectParams {
    fn default() -> Self {
        Self {
            name: None,
            failed_only: true,
        }
    }
}

impl McpServerReconnectParams {
    /// Retries failures across all enabled servers by default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Selects a single registered server.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    /// Selects failed-only or full reconnection.
    #[must_use]
    pub const fn with_failed_only(mut self, failed_only: bool) -> Self {
        self.failed_only = failed_only;
        self
    }
}

impl McpServerToggleParams {
    /// Selects the desired enabled state of a registered server.
    #[must_use]
    pub fn new(name: impl Into<String>, enabled: bool) -> Self {
        Self {
            name: name.into(),
            enabled,
        }
    }
}
