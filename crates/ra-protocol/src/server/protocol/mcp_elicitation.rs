// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The form-mode wire request emitted by product MCP tool approval.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Correlation and form contents of `mcpServer/elicitation/request`.
/// The producer emits the source's empty approval schema; arbitrary server-originated forms
/// and account authentication elicitations are separate transport capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "camelCase")]
pub struct McpServerElicitationRequestParams {
    thread_id: String,
    turn_id: Option<String>,
    server_name: String,
    #[serde(flatten)]
    request: McpServerElicitationRequest,
}

/// The source form envelope. The schema stays JSON at this product-generated boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum McpServerElicitationRequest {
    /// Requests approval through a standard MCP form.
    #[serde(rename_all = "camelCase")]
    Form {
        /// Privileged product approval metadata.
        #[serde(rename = "_meta")]
        meta: Option<Value>,
        /// Question shown to the client.
        message: String,
        /// The source's empty object schema for an approval form.
        requested_schema: Value,
    },
}

impl McpServerElicitationRequestParams {
    /// Correlates a product-generated approval form with its thread, turn and MCP server.
    #[must_use]
    pub fn new(
        thread_id: String,
        turn_id: Option<String>,
        server_name: String,
        request: McpServerElicitationRequest,
    ) -> Self {
        Self {
            thread_id,
            turn_id,
            server_name,
            request,
        }
    }
    /// The product-generated form, including approval scope metadata.
    #[must_use]
    pub const fn request(&self) -> &McpServerElicitationRequest {
        &self.request
    }
    /// The thread that owns the request.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
    /// The correlated turn, when the producer has one.
    #[must_use]
    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }
    /// The original MCP server identity.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

/// Client decision for an MCP elicitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "camelCase")]
pub enum McpServerElicitationAction {
    /// Accept the request.
    Accept,
    /// Refuse this invocation.
    Decline,
    /// Cancel the turn.
    Cancel,
}

/// The source response fields; decline and cancel normally carry null content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "camelCase")]
pub struct McpServerElicitationRequestResponse {
    pub(crate) action: McpServerElicitationAction,
    pub(crate) content: Option<Value>,
    #[serde(rename = "_meta")]
    pub(crate) meta: Option<Value>,
}

impl McpServerElicitationRequestResponse {
    /// Constructs a decision with null content and metadata, as in the source wire record.
    #[must_use]
    pub const fn new(action: McpServerElicitationAction) -> Self {
        Self {
            action,
            content: None,
            meta: None,
        }
    }
    /// Supplies structured form input.
    #[must_use]
    pub fn with_content(mut self, content: Value) -> Self {
        self.content = Some(content);
        self
    }
    /// Supplies optional action metadata such as the requested persistence scope.
    #[must_use]
    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = Some(meta);
        self
    }
    /// The client's decision.
    #[must_use]
    pub const fn action(&self) -> McpServerElicitationAction {
        self.action
    }
    /// Structured form input, if supplied.
    #[must_use]
    pub fn content(&self) -> Option<&Value> {
        self.content.as_ref()
    }
    /// Action metadata, if supplied.
    #[must_use]
    pub fn meta(&self) -> Option<&Value> {
        self.meta.as_ref()
    }
}
