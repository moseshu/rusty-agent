//! The requests the server sends the client, and the notification that one no longer needs an
//! answer: Codex's `ServerRequest` and `ServerRequestPayload` (`protocol/common.rs`) and
//! `ServerRequestResolvedNotification` (`protocol/v2/notification.rs`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    item::{
        CommandExecutionRequestApprovalParams, DynamicToolCallParams,
        FileChangeRequestApprovalParams, ToolRequestUserInputParams,
    },
    permissions::PermissionsRequestApprovalParams,
};
use crate::frame::{JsonRpcRequest, RequestId};

/// The method of [`ServerRequestResolvedNotification`].
pub const SERVER_REQUEST_RESOLVED_METHOD: &str = "serverRequest/resolved";

/// A request from the server to the client: Codex's `ServerRequest`.
///
/// It is read from a [`JsonRpcRequest`] by its method, as a client reads what the server sends.
/// Codex's server also asks for MCP elicitations, which come with the MCP methods; for its own
/// account tokens, attestations and the time of an external clock, which belong to its account
/// and product machinery; and for the approvals of its deprecated v1 turns. None is ported.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum ServerRequest {
    /// Asks to approve a command.
    #[serde(rename = "item/commandExecution/requestApproval")]
    CommandExecutionRequestApproval {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The command.
        params: CommandExecutionRequestApprovalParams,
    },
    /// Asks to approve file changes.
    #[serde(rename = "item/fileChange/requestApproval")]
    FileChangeRequestApproval {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The changes.
        params: FileChangeRequestApprovalParams,
    },
    /// Asks the user questions for a tool.
    #[serde(rename = "item/tool/requestUserInput")]
    ToolRequestUserInput {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The questions.
        params: ToolRequestUserInputParams,
    },
    /// Asks to grant permissions.
    #[serde(rename = "item/permissions/requestApproval")]
    PermissionsRequestApproval {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The permissions.
        params: PermissionsRequestApprovalParams,
    },
    /// Calls a tool the client provides.
    #[serde(rename = "item/tool/call")]
    DynamicToolCall {
        /// The id its answer will carry.
        #[serde(rename = "id")]
        request_id: RequestId,
        /// The call.
        params: DynamicToolCallParams,
    },
}

impl ServerRequest {
    /// The id its answer will carry.
    #[must_use]
    pub const fn id(&self) -> &RequestId {
        match self {
            Self::CommandExecutionRequestApproval { request_id, .. }
            | Self::FileChangeRequestApproval { request_id, .. }
            | Self::ToolRequestUserInput { request_id, .. }
            | Self::PermissionsRequestApproval { request_id, .. }
            | Self::DynamicToolCall { request_id, .. } => request_id,
        }
    }

    /// The method called.
    #[must_use]
    pub const fn method_name(&self) -> &'static str {
        match self {
            Self::CommandExecutionRequestApproval { .. } => "item/commandExecution/requestApproval",
            Self::FileChangeRequestApproval { .. } => "item/fileChange/requestApproval",
            Self::ToolRequestUserInput { .. } => "item/tool/requestUserInput",
            Self::PermissionsRequestApproval { .. } => "item/permissions/requestApproval",
            Self::DynamicToolCall { .. } => "item/tool/call",
        }
    }
}

impl TryFrom<JsonRpcRequest> for ServerRequest {
    type Error = serde_json::Error;

    /// Reads the request by its method, as Codex's `TryFrom<JSONRPCRequest>` does: an unknown
    /// method or parameters that do not fit fail with serde's description of the mismatch.
    fn try_from(request: JsonRpcRequest) -> Result<Self, Self::Error> {
        let mut object = serde_json::Map::new();
        object.insert("id".to_owned(), serde_json::to_value(request.id())?);
        object.insert(
            "method".to_owned(),
            Value::String(request.method().to_owned()),
        );
        if let Some(params) = request.into_params() {
            object.insert("params".to_owned(), params);
        }
        serde_json::from_value(Value::Object(object))
    }
}

impl TryFrom<ServerRequest> for JsonRpcRequest {
    type Error = serde_json::Error;

    /// Writes the request as a JSON-RPC request. It fails only where a parameter cannot be
    /// written as JSON, such as a path that is not valid UTF-8.
    fn try_from(request: ServerRequest) -> Result<Self, Self::Error> {
        let method = request.method_name();
        let (request_id, params) = match request {
            ServerRequest::CommandExecutionRequestApproval { request_id, params } => {
                (request_id, serde_json::to_value(params)?)
            }
            ServerRequest::FileChangeRequestApproval { request_id, params } => {
                (request_id, serde_json::to_value(params)?)
            }
            ServerRequest::ToolRequestUserInput { request_id, params } => {
                (request_id, serde_json::to_value(params)?)
            }
            ServerRequest::PermissionsRequestApproval { request_id, params } => {
                (request_id, serde_json::to_value(params)?)
            }
            ServerRequest::DynamicToolCall { request_id, params } => {
                (request_id, serde_json::to_value(params)?)
            }
        };
        Ok(Self::new(request_id, method).with_params(params))
    }
}

/// A request the server is about to send, before it has an id: Codex's `ServerRequestPayload`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerRequestPayload {
    /// Asks to approve a command.
    CommandExecutionRequestApproval(CommandExecutionRequestApprovalParams),
    /// Asks to approve file changes.
    FileChangeRequestApproval(FileChangeRequestApprovalParams),
    /// Asks the user questions for a tool.
    ToolRequestUserInput(ToolRequestUserInputParams),
    /// Asks to grant permissions.
    PermissionsRequestApproval(PermissionsRequestApprovalParams),
    /// Calls a tool the client provides.
    DynamicToolCall(DynamicToolCallParams),
}

impl ServerRequestPayload {
    /// The request with the id `request_id`.
    #[must_use]
    pub fn request_with_id(self, request_id: RequestId) -> ServerRequest {
        match self {
            Self::CommandExecutionRequestApproval(params) => {
                ServerRequest::CommandExecutionRequestApproval { request_id, params }
            }
            Self::FileChangeRequestApproval(params) => {
                ServerRequest::FileChangeRequestApproval { request_id, params }
            }
            Self::ToolRequestUserInput(params) => {
                ServerRequest::ToolRequestUserInput { request_id, params }
            }
            Self::PermissionsRequestApproval(params) => {
                ServerRequest::PermissionsRequestApproval { request_id, params }
            }
            Self::DynamicToolCall(params) => ServerRequest::DynamicToolCall { request_id, params },
        }
    }
}

/// The parameters of `serverRequest/resolved`, which tells the connections of a thread that a
/// request the server sent no longer needs an answer: Codex's `ServerRequestResolvedNotification`.
///
/// It follows a request once its answer has arrived, or once the server has withdrawn it because
/// the turn it belonged to moved on, so that every client showing the request can dismiss it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolvedNotification {
    thread_id: String,
    request_id: RequestId,
}

impl ServerRequestResolvedNotification {
    /// The request `request_id` of the thread `thread_id` no longer needs an answer.
    #[must_use]
    pub fn new(thread_id: impl Into<String>, request_id: RequestId) -> Self {
        Self {
            thread_id: thread_id.into(),
            request_id,
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The request.
    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }
}
