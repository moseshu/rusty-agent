// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Connection values supplied by the trusted environment owner.
#![allow(
    unreachable_pub,
    reason = "Private implementation items are exposed only by the feature-gated test facade."
)]
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use http::HeaderMap;
use ra_http::HttpClientFactory;
use std::{collections::HashMap, path::PathBuf, time::Duration};
use tokio::sync::watch;
pub const DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecServerClientConnectOptions {
    pub client_name: String,
    pub initialize_timeout: Duration,
    pub resume_session_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteExecServerConnectArgs {
    pub websocket_url: String,
    pub client_name: String,
    pub connect_timeout: Duration,
    pub initialize_timeout: Duration,
    pub resume_session_id: Option<String>,
    pub http_client_factory: HttpClientFactory,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioExecServerConnectArgs {
    pub command: StdioExecServerCommand,
    pub client_name: String,
    pub initialize_timeout: Duration,
    pub resume_session_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioExecServerCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
}
pub type DeferredEnvironmentReadiness = watch::Receiver<Option<Result<(), String>>>;
#[derive(Clone, Debug)]
pub struct Deferred<T> {
    pub readiness: DeferredEnvironmentReadiness,
    pub transport: T,
}
#[derive(Clone)]
pub enum ExecServerTransportParams {
    Deferred(Box<Deferred<ExecServerTransportParams>>),
    WebSocketUrl {
        websocket_url: String,
        connect_timeout: Duration,
        initialize_timeout: Duration,
        http_headers: HeaderMap,
    },
    StdioCommand {
        command: StdioExecServerCommand,
        initialize_timeout: Duration,
    },
}
impl std::fmt::Debug for ExecServerTransportParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deferred(value) => f
                .debug_struct("Deferred")
                .field("transport", &value.transport)
                .finish_non_exhaustive(),
            Self::WebSocketUrl {
                websocket_url,
                connect_timeout,
                initialize_timeout,
                ..
            } => f
                .debug_struct("WebSocketUrl")
                .field("websocket_url", websocket_url)
                .field("connect_timeout", connect_timeout)
                .field("initialize_timeout", initialize_timeout)
                .field("http_headers", &"<redacted>")
                .finish(),
            Self::StdioCommand {
                command,
                initialize_timeout,
            } => f
                .debug_struct("StdioCommand")
                .field("command", command)
                .field("initialize_timeout", initialize_timeout)
                .finish(),
        }
    }
}
impl ExecServerTransportParams {
    #[must_use]
    pub fn websocket_url(websocket_url: String, connect_timeout: Duration) -> Self {
        Self::WebSocketUrl {
            websocket_url,
            connect_timeout,
            initialize_timeout: DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT,
            http_headers: HeaderMap::new(),
        }
    }
}

impl Default for ExecServerClientConnectOptions {
    fn default() -> Self {
        Self {
            client_name: "codex-core".to_string(),
            initialize_timeout: DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT,
            resume_session_id: None,
        }
    }
}

impl From<RemoteExecServerConnectArgs> for ExecServerClientConnectOptions {
    fn from(value: RemoteExecServerConnectArgs) -> Self {
        Self {
            client_name: value.client_name,
            initialize_timeout: value.initialize_timeout,
            resume_session_id: value.resume_session_id,
        }
    }
}

impl From<StdioExecServerConnectArgs> for ExecServerClientConnectOptions {
    fn from(value: StdioExecServerConnectArgs) -> Self {
        Self {
            client_name: value.client_name,
            initialize_timeout: value.initialize_timeout,
            resume_session_id: value.resume_session_id,
        }
    }
}

impl RemoteExecServerConnectArgs {
    #[must_use]
    pub fn new(
        websocket_url: String,
        client_name: String,
        http_client_factory: HttpClientFactory,
    ) -> Self {
        Self {
            websocket_url,
            client_name,
            connect_timeout: DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
            initialize_timeout: DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT,
            resume_session_id: None,
            http_client_factory,
        }
    }
}
