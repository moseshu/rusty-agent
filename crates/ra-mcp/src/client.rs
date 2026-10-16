// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Shared initialized MCP session, bounded discovery, and cancellable calls.

use ra_core::error::{Error, Result, ToolErrorKind};
use rmcp::{
    RoleClient,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientConfig, ClientRequest,
        ErrorCode, JsonObject, PaginatedRequestParams, RequestMetaObject, ServerResult, Tool,
    },
    service::{Peer, PeerRequestOptions, RunningService, ServiceError},
    transport::IntoTransport,
};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "sse")]
pub mod sse;
#[cfg(feature = "stdio")]
pub mod stdio;

/// Default per-request deadline, Python's `client_session_timeout_seconds = 5`.
pub const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Default overall deadline for every page of one catalog listing, matching Codex.
pub const DEFAULT_CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum number of tools collected from one server.
pub const MAX_MCP_CATALOG_ITEMS: usize = 2_048;

/// Timeouts shared by all transports.
///
/// As in Python, an enabled session timeout bounds every request: initialize, each tools/list
/// page and tools/call. Codex's overall pagination deadline bounds a whole listing, and its
/// per-server tool timeout can replace the session timeout for calls.
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)] // Each timeout bounds a different protocol scope.
pub struct McpClientOptions {
    session_timeout: Option<Duration>,
    catalog_timeout: Duration,
    tool_timeout: Option<Duration>,
}
impl Default for McpClientOptions {
    fn default() -> Self {
        Self {
            session_timeout: Some(DEFAULT_SESSION_TIMEOUT),
            catalog_timeout: DEFAULT_CATALOG_TIMEOUT,
            tool_timeout: None,
        }
    }
}
impl McpClientOptions {
    /// Sets the per-request deadline, Python's `client_session_timeout_seconds`.
    /// `None` or zero disables it; the independent catalog deadline still applies.
    #[must_use]
    pub fn with_session_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.session_timeout = timeout.into().filter(|timeout| !timeout.is_zero());
        self
    }
    /// Sets the overall tools/list deadline, including all pages.
    #[must_use]
    pub const fn with_catalog_timeout(mut self, timeout: Duration) -> Self {
        self.catalog_timeout = timeout;
        self
    }
    /// Sets a tools/call deadline that replaces the session timeout, as Codex's
    /// `tool_timeout_sec`.
    #[must_use]
    pub const fn with_tool_timeout(mut self, timeout: Duration) -> Self {
        self.tool_timeout = Some(timeout);
        self
    }
    /// Per-request deadline, or `None` when disabled.
    #[must_use]
    pub const fn session_timeout(&self) -> Option<Duration> {
        self.session_timeout
    }
    /// Deadline applied to tools/call, or `None` when disabled.
    #[must_use]
    pub fn tool_timeout(&self) -> Option<Duration> {
        self.tool_timeout.or(self.session_timeout)
    }
}

struct ClientSessionServer {
    cleanup_unconfirmed: AtomicBool,
    instructions: Option<String>,
    service: Mutex<Option<RunningService<RoleClient, ClientConfig>>>,
    options: McpClientOptions,
}

/// A connected server. Clones and registered tools retain the same session until cleanup.
#[derive(Clone)]
pub struct McpClient {
    server_name: Arc<str>,
    session: Arc<ClientSessionServer>,
}

impl McpClient {
    /// Initializes an SDK transport and sends its initialized notification.
    /// The transport owns its I/O resources; no raw protocol implementation is duplicated here.
    pub async fn connect<T, E, A>(
        server_name: impl Into<String>,
        transport: T,
        options: McpClientOptions,
    ) -> Result<Self>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let name = server_name.into();
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(Error::config(
                "MCP server name must be nonempty and contain no control characters",
            ));
        }
        let initialize = rmcp::service::serve_client(ClientConfig::default(), transport);
        let service = match options.session_timeout {
            Some(timeout) => tokio::time::timeout(timeout, initialize)
                .await
                .map_err(|_| safe_error(ToolErrorKind::Timeout, "initialize timed out"))?,
            None => initialize.await,
        }
        .map_err(|_| safe_error(ToolErrorKind::ExecutionFailed, "initialize failed"))?;
        let instructions = service
            .peer()
            .peer_info()
            .and_then(|info| info.instructions.clone());
        Ok(Self {
            server_name: name.into(),
            session: Arc::new(ClientSessionServer {
                instructions,
                service: Mutex::new(Some(service)),
                cleanup_unconfirmed: AtomicBool::new(false),
                options,
            }),
        })
    }
    /// Host-assigned raw server identity.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
    /// Instructions the server returned from initialize.
    #[must_use]
    pub fn server_instructions(&self) -> Option<&str> {
        self.session.instructions.as_deref()
    }
    /// Session timeouts captured when connecting.
    #[must_use]
    pub fn options(&self) -> &McpClientOptions {
        &self.session.options
    }
    async fn peer(&self) -> Result<Peer<RoleClient>> {
        self.session
            .service
            .lock()
            .await
            .as_ref()
            .map(|service| service.peer().clone())
            .ok_or_else(|| safe_error(ToolErrorKind::ExecutionFailed, "server is closed"))
    }
    /// Lists every page, with Codex's item, page, cursor, and overall deadline limits.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        let peer = self.peer().await?;
        let collect = async {
            let mut tools = Vec::new();
            let mut cursor = None;
            let mut seen = HashSet::new();
            for _ in 0..100 {
                let params = cursor
                    .take()
                    .map(|next| PaginatedRequestParams::default().with_cursor(Some(next)));
                let request = params.map_or_else(
                    rmcp::model::ListToolsRequest::default,
                    rmcp::model::ListToolsRequest::with_param,
                );
                let response = send_request(
                    peer.clone(),
                    ClientRequest::ListToolsRequest(request),
                    self.options().session_timeout,
                    None,
                )
                .await?;
                let ServerResult::ListToolsResult(page) = response else {
                    return Err(safe_error(
                        ToolErrorKind::ExecutionFailed,
                        "tools/list returned an unexpected response",
                    ));
                };
                if page.tools.len() > MAX_MCP_CATALOG_ITEMS.saturating_sub(tools.len()) {
                    return Err(safe_error(
                        ToolErrorKind::ExecutionFailed,
                        "tools/list exceeded 2048 items",
                    ));
                }
                tools.extend(page.tools);
                let Some(next) = page.next_cursor else {
                    return Ok(tools);
                };
                if next.len() > 64 * 1024 {
                    return Err(safe_error(
                        ToolErrorKind::ExecutionFailed,
                        "tools/list cursor exceeded 65536 bytes",
                    ));
                }
                if !seen.insert(next.clone()) {
                    return Err(safe_error(
                        ToolErrorKind::ExecutionFailed,
                        "tools/list returned a repeated pagination cursor",
                    ));
                }
                cursor = Some(next);
            }
            Err(safe_error(
                ToolErrorKind::ExecutionFailed,
                "tools/list exceeded 100 pages",
            ))
        };
        tokio::time::timeout(self.options().catalog_timeout, collect)
            .await
            .map_err(|_| safe_error(ToolErrorKind::Timeout, "tools/list timed out"))?
    }
    /// Calls the original protocol tool name. Arguments may be omitted or a JSON object; `meta`
    /// becomes the request's `_meta`, as Python's `call_tool(..., meta=...)`.
    /// Timeout and dropped invocation futures send notifications/cancelled through the SDK.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: impl Into<Option<Value>>,
        meta: Option<JsonObject>,
    ) -> Result<CallToolResult> {
        let arguments = match arguments.into() {
            Some(Value::Object(arguments)) => Some(arguments),
            None => None,
            Some(_) => {
                return Err(safe_error(
                    ToolErrorKind::InvalidInput,
                    "MCP tool arguments must be a JSON object",
                ));
            }
        };
        let peer = self.peer().await?;
        let mut params = CallToolRequestParams::new(name.to_owned());
        params.arguments = arguments;
        let result = send_request(
            peer,
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            self.options().tool_timeout(),
            meta,
        )
        .await?;
        match result {
            ServerResult::CallToolResult(result) => Ok(result),
            _ => Err(safe_error(
                ToolErrorKind::ExecutionFailed,
                "tools/call returned an unexpected response",
            )),
        }
    }
    /// Closes this session for all clones. Repeated cleanup succeeds.
    pub async fn cleanup(&self) -> Result<()> {
        let mut slot = self.session.service.lock().await;
        if let Some(mut service) = slot.take() {
            self.session
                .cleanup_unconfirmed
                .store(true, Ordering::Release);
            match service.close_with_timeout(Duration::from_secs(5)).await {
                Ok(Some(rmcp::service::QuitReason::JoinError(_))) | Err(_) => {
                    return Err(safe_error(
                        ToolErrorKind::ExecutionFailed,
                        "MCP cleanup failed",
                    ));
                }
                Ok(Some(_)) => self
                    .session
                    .cleanup_unconfirmed
                    .store(false, Ordering::Release),
                Ok(None) => {
                    return Err(safe_error(ToolErrorKind::Timeout, "MCP cleanup timed out"));
                }
            }
        } else if self.session.cleanup_unconfirmed.load(Ordering::Acquire) {
            return Err(safe_error(
                ToolErrorKind::ExecutionFailed,
                "MCP cleanup has not been confirmed",
            ));
        }
        Ok(())
    }
}

async fn send_request(
    peer: Peer<RoleClient>,
    request: ClientRequest,
    timeout: Option<Duration>,
    meta: Option<JsonObject>,
) -> Result<ServerResult> {
    let mut options = PeerRequestOptions::no_options();
    options.timeout = timeout;
    options.meta = meta.map(RequestMetaObject::from);
    let handle = peer
        .send_cancellable_request(request, options)
        .await
        .map_err(service_error)?;
    let mut guard = CancelOnDrop(Some((peer, handle.id.clone())));
    let result = handle.await_response().await;
    guard.0 = None;
    result.map_err(service_error)
}

// The runner cancels tools by dropping futures. Keep the remote request cancellable too.
struct CancelOnDrop(Option<(Peer<RoleClient>, rmcp::model::RequestId)>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some((peer, id)) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let mut params = rmcp::model::CancelledNotificationParam::new(Some(id), None);
                params.reason = Some("tool invocation cancelled".to_owned());
                let _ = tokio::time::timeout(Duration::from_secs(2), peer.notify_cancelled(params))
                    .await;
            });
        }
    }
}

// A JSON-RPC error is the server's answer, which Python passes on as the MCP error message.
// Transport errors may contain URL userinfo, query tokens and headers; their raw source is never
// retained in the framework error or its Debug representation.
fn service_error(error: ServiceError) -> Error {
    match error {
        ServiceError::McpError(error) => {
            let kind = if error.code == ErrorCode::INVALID_PARAMS {
                ToolErrorKind::InvalidInput
            } else {
                ToolErrorKind::ExecutionFailed
            };
            Error::tool(
                kind,
                "mcp",
                format!("MCP server error {}: {}", error.code.0, error.message),
            )
        }
        ServiceError::Timeout { .. } => safe_error(ToolErrorKind::Timeout, "MCP request timed out"),
        _ => safe_error(ToolErrorKind::ExecutionFailed, "MCP request failed"),
    }
}
pub(crate) fn safe_error(kind: ToolErrorKind, message: &'static str) -> Error {
    Error::tool(kind, "mcp", message)
}
