// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Typed executor connection and RPC failures.
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use std::{sync::Arc, time::Duration};
#[derive(Debug, thiserror::Error)]
pub enum ExecServerError {
    #[error("failed to spawn exec-server: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("timed out connecting to exec-server websocket `{url}` after {timeout:?}")]
    WebSocketConnectTimeout { url: String, timeout: Duration },
    #[error("failed to connect to exec-server websocket `{url}`: {source}")]
    WebSocketConnect {
        url: String,
        #[source]
        source: tokio_tungstenite::tungstenite::Error,
    },
    #[error("failed to configure exec-server websocket: {0}")]
    WebSocketConfiguration(String),
    #[error("timed out waiting for exec-server initialize handshake after {timeout:?}")]
    InitializeTimedOut { timeout: Duration },
    #[error(
        "exec-server protocol error: timed out waiting for exec-server `{method}` response after {timeout:?}"
    )]
    RpcTimedOut { method: String, timeout: Duration },
    #[error(transparent)]
    ApplicationNetworkPolicy(#[from] ra_http::NetworkPolicyDenied),
    #[error("exec-server transport closed")]
    Closed,
    #[error("{0}")]
    Disconnected(String),
    #[error("environment unavailable: {0}")]
    ProvisioningFailed(String),
    #[error("failed to serialize or deserialize exec-server JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HTTP request failed: {0}")]
    HttpRequest(String),
    #[error("authentication required: {0}")]
    AuthenticationRequired(String),
    #[error("exec-server protocol error: {0}")]
    Protocol(String),
    #[error(
        "environment `{environment_id}` is already registered with a different provisioning mode"
    )]
    ProvisioningModeConflict { environment_id: String },
    #[error("exec-server rejected request ({code}): {message}")]
    Server { code: i64, message: String },
    #[error("environment registry request failed ({status}{code_suffix}): {message}", code_suffix = .code.as_ref().map(|code| format!(", {code}")).unwrap_or_default())]
    EnvironmentRegistryHttp {
        status: http::StatusCode,
        code: Option<String>,
        message: String,
    },
    #[error("environment registry configuration error: {0}")]
    EnvironmentRegistryConfig(String),
    #[error("environment registry authentication error: {0}")]
    EnvironmentRegistryAuth(String),
    #[error("environment registry request failed: {0}")]
    EnvironmentRegistryRequest(#[from] ra_http::RouteAwareRequestError),
    #[error("exec-server connection attempt failed: {0}")]
    ConnectionAttempt(#[source] Arc<ExecServerError>),
}

// Classifies typed executor failures without treating error text as a retry signal.

use std::io::ErrorKind;

use http::StatusCode;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::error::ProtocolError;

use crate::client::is_retryable_registry_error;
use crate::rpc::SESSION_ALREADY_ATTACHED_ERROR_CODE;

impl ExecServerError {
    /// Identifies transient failures that read-only preparation can retry.
    ///
    /// Includes transient handshake responses and connection EOFs, including wrapped
    /// failures. Authentication, TLS, and malformed protocol errors are excluded.
    /// This is public because callers in `codex-core` also classify capability discovery.
    #[must_use]
    pub fn is_retryable_preparation_error(&self) -> bool {
        let mut error = self;
        while let Self::ConnectionAttempt(source) = error {
            error = source;
        }
        if is_retryable_registry_error(error) {
            return true;
        }
        match error {
            Self::Closed
            | Self::Disconnected(_)
            | Self::WebSocketConnectTimeout { .. }
            | Self::RpcTimedOut { .. }
            | Self::InitializeTimedOut { .. } => true,
            Self::WebSocketConnect { source, .. } => match source {
                WebSocketError::ConnectionClosed
                | WebSocketError::AlreadyClosed
                | WebSocketError::Protocol(
                    ProtocolError::HandshakeIncomplete
                    | ProtocolError::ResetWithoutClosingHandshake,
                ) => true,
                WebSocketError::Http(response) => {
                    response.status().is_server_error()
                        || matches!(
                            response.status(),
                            StatusCode::REQUEST_TIMEOUT
                                | StatusCode::CONFLICT
                                | StatusCode::TOO_MANY_REQUESTS
                        )
                }
                WebSocketError::Io(error) => matches!(
                    error.kind(),
                    ErrorKind::ConnectionRefused
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::NetworkDown
                        | ErrorKind::NetworkUnreachable
                        | ErrorKind::HostUnreachable
                        | ErrorKind::BrokenPipe
                        | ErrorKind::NotConnected
                        | ErrorKind::UnexpectedEof
                        | ErrorKind::TimedOut
                ),
                _ => false,
            },
            Self::Server { code, .. } if *code == SESSION_ALREADY_ATTACHED_ERROR_CODE => true,
            _ => false,
        }
    }
}
