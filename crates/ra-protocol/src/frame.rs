//! The messages host and agent exchange, and the correlation of a request with its answer: Codex's
//! app-server wire format (`app-server-protocol/src/rpc.rs`, `app-server/src/error_code.rs`).
//!
//! Every message is a JSON-RPC 2.0 request, notification, response or error, without the
//! `"jsonrpc": "2.0"` member: as Codex's, the server neither sends nor expects it, and ignores it
//! when a client sends it. Requests travel both ways — the host calls the agent's methods, and the
//! agent asks the host for approvals and tool calls with requests of its own (server requests),
//! whose answers [`PendingRequests`] routes back to whoever asked.
//!
//! As in Codex there is no general cancellation of a request in flight: a turn is stopped with its
//! own method, and a server request that no longer needs an answer is withdrawn by the agent, which
//! tells the host so with a notification.
//!
//! Parameters and results are carried as JSON values, so whatever a newer peer adds to them reaches
//! the method that reads them untouched. Members an envelope does not define, such as `jsonrpc`,
//! are ignored, as Codex ignores them.

mod pending;

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use pending::{PendingRequests, RequestResult};

/// The JSON-RPC version the wire format follows: Codex's `JSONRPC_VERSION`. It is not sent.
pub const JSONRPC_VERSION: &str = "2.0";

/// The error codes the agent answers with: Codex's `error_code` constants.
pub mod error_code {
    /// The request is not valid now, such as a method called before `initialize`.
    pub const INVALID_REQUEST: i64 = -32600;
    /// No such method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The parameters do not fit the method.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The agent failed while handling the request.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// The agent's queue of incoming requests is full; the client may retry later.
    pub const OVERLOADED: i64 = -32001;
}

/// What pairs a request with its response: Codex's `RequestId`, a string or an integer.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A string id.
    String(String),
    /// An integer id.
    Integer(i64),
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(value) => formatter.write_str(value),
            Self::Integer(value) => write!(formatter, "{value}"),
        }
    }
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

/// W3C Trace Context a request may carry for distributed tracing: Codex's `W3cTraceContext`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct W3cTraceContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    traceparent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tracestate: Option<String>,
}

impl W3cTraceContext {
    /// A trace context with neither header.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the `traceparent` header.
    #[must_use]
    pub fn with_traceparent(mut self, traceparent: impl Into<String>) -> Self {
        self.traceparent = Some(traceparent.into());
        self
    }

    /// Sets the `tracestate` header.
    #[must_use]
    pub fn with_tracestate(mut self, tracestate: impl Into<String>) -> Self {
        self.tracestate = Some(tracestate.into());
        self
    }

    /// The `traceparent` header.
    #[must_use]
    pub fn traceparent(&self) -> Option<&str> {
        self.traceparent.as_deref()
    }

    /// The `tracestate` header.
    #[must_use]
    pub fn tracestate(&self) -> Option<&str> {
        self.tracestate.as_deref()
    }
}

/// Any message that can be read off the wire or written to it: Codex's `JSONRPCMessage`.
///
/// A message is read as the first of request, notification, response and error whose required
/// members it has, as Codex reads it: a request has `id` and `method`, a notification `method`
/// alone, a response `id` and `result`, an error `id` and `error`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    /// A request, which expects a response or an error.
    Request(JsonRpcRequest),
    /// A notification, which expects nothing back.
    Notification(JsonRpcNotification),
    /// A successful answer to a request.
    Response(JsonRpcResponse),
    /// A failed answer to a request.
    Error(JsonRpcError),
}

impl From<JsonRpcRequest> for JsonRpcMessage {
    fn from(request: JsonRpcRequest) -> Self {
        Self::Request(request)
    }
}

impl From<JsonRpcNotification> for JsonRpcMessage {
    fn from(notification: JsonRpcNotification) -> Self {
        Self::Notification(notification)
    }
}

impl From<JsonRpcResponse> for JsonRpcMessage {
    fn from(response: JsonRpcResponse) -> Self {
        Self::Response(response)
    }
}

impl From<JsonRpcError> for JsonRpcMessage {
    fn from(error: JsonRpcError) -> Self {
        Self::Error(error)
    }
}

/// A request that expects a response: Codex's `JSONRPCRequest`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    id: RequestId,
    method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trace: Option<W3cTraceContext>,
}

impl JsonRpcRequest {
    /// A request for `method` under `id`, without parameters.
    #[must_use]
    pub fn new(id: impl Into<RequestId>, method: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            method: method.into(),
            params: None,
            trace: None,
        }
    }

    /// Sets the parameters.
    #[must_use]
    pub fn with_params(mut self, params: Value) -> Self {
        self.params = Some(params);
        self
    }

    /// Sets the trace context.
    #[must_use]
    pub fn with_trace(mut self, trace: W3cTraceContext) -> Self {
        self.trace = Some(trace);
        self
    }

    /// The id its answer will carry.
    #[must_use]
    pub const fn id(&self) -> &RequestId {
        &self.id
    }

    /// The method called.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The parameters, if any were sent.
    #[must_use]
    pub const fn params(&self) -> Option<&Value> {
        self.params.as_ref()
    }

    /// The trace context, if one was sent.
    #[must_use]
    pub const fn trace(&self) -> Option<&W3cTraceContext> {
        self.trace.as_ref()
    }

    /// Takes the parameters.
    #[must_use]
    pub fn into_params(self) -> Option<Value> {
        self.params
    }
}

/// A message that expects nothing back: Codex's `JSONRPCNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

impl JsonRpcNotification {
    /// A notification of `method`, without parameters.
    #[must_use]
    pub fn new(method: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            params: None,
        }
    }

    /// Sets the parameters.
    #[must_use]
    pub fn with_params(mut self, params: Value) -> Self {
        self.params = Some(params);
        self
    }

    /// The method notified.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The parameters, if any were sent.
    #[must_use]
    pub const fn params(&self) -> Option<&Value> {
        self.params.as_ref()
    }

    /// Takes the parameters.
    #[must_use]
    pub fn into_params(self) -> Option<Value> {
        self.params
    }
}

/// A successful answer to the request `id`: Codex's `JSONRPCResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    id: RequestId,
    result: Value,
}

impl JsonRpcResponse {
    /// The answer `result` to the request `id`.
    #[must_use]
    pub fn new(id: impl Into<RequestId>, result: Value) -> Self {
        Self {
            id: id.into(),
            result,
        }
    }

    /// The request answered.
    #[must_use]
    pub const fn id(&self) -> &RequestId {
        &self.id
    }

    /// The result.
    #[must_use]
    pub const fn result(&self) -> &Value {
        &self.result
    }

    /// Takes the result.
    #[must_use]
    pub fn into_result(self) -> Value {
        self.result
    }
}

/// A failed answer to the request `id`: Codex's `JSONRPCError`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    error: JsonRpcErrorObject,
    id: RequestId,
}

impl JsonRpcError {
    /// The failure `error` of the request `id`.
    #[must_use]
    pub fn new(id: impl Into<RequestId>, error: JsonRpcErrorObject) -> Self {
        Self {
            error,
            id: id.into(),
        }
    }

    /// The request answered.
    #[must_use]
    pub const fn id(&self) -> &RequestId {
        &self.id
    }

    /// What went wrong.
    #[must_use]
    pub const fn error(&self) -> &JsonRpcErrorObject {
        &self.error
    }

    /// Takes what went wrong.
    #[must_use]
    pub fn into_error(self) -> JsonRpcErrorObject {
        self.error
    }
}

/// What went wrong with a request: Codex's `JSONRPCErrorError`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    code: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    message: String,
}

impl JsonRpcErrorObject {
    /// An error of `code` saying `message`.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            data: None,
            message: message.into(),
        }
    }

    /// [`error_code::INVALID_REQUEST`]: Codex's `invalid_request`.
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(error_code::INVALID_REQUEST, message)
    }

    /// [`error_code::METHOD_NOT_FOUND`]: Codex's `method_not_found`.
    #[must_use]
    pub fn method_not_found(message: impl Into<String>) -> Self {
        Self::new(error_code::METHOD_NOT_FOUND, message)
    }

    /// [`error_code::INVALID_PARAMS`]: Codex's `invalid_params`.
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(error_code::INVALID_PARAMS, message)
    }

    /// [`error_code::INTERNAL_ERROR`]: Codex's `internal_error`.
    #[must_use]
    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(error_code::INTERNAL_ERROR, message)
    }

    /// [`error_code::OVERLOADED`], with Codex's message for a full queue of incoming requests.
    #[must_use]
    pub fn overloaded() -> Self {
        Self::new(error_code::OVERLOADED, "Server overloaded; retry later.")
    }

    /// Sets the structured detail.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// The error code.
    #[must_use]
    pub const fn code(&self) -> i64 {
        self.code
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The structured detail, if any.
    #[must_use]
    pub const fn data(&self) -> Option<&Value> {
        self.data.as_ref()
    }
}

impl fmt::Display for JsonRpcErrorObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}
