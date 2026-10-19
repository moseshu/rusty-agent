// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Shared initialized MCP session, bounded discovery, and cancellable calls.

use ra_core::error::{Error, Result, ToolErrorKind};
use rmcp::{
    RoleClient,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientConfig, ClientRequest,
        ErrorCode, GetPromptRequest, GetPromptRequestParams, GetPromptResult, JsonObject,
        ListPromptsRequest, ListPromptsResult, ListResourceTemplatesRequest,
        ListResourceTemplatesResult, ListResourcesRequest, ListResourcesResult,
        PaginatedRequestParams, ReadResourceRequest, ReadResourceRequestParams, ReadResourceResult,
        RequestMetaObject, ServerPeerInfo, ServerResult, Tool,
    },
    service::{ClientInitializeError, Peer, PeerRequestOptions, RunningService, ServiceError},
    transport::IntoTransport,
};
use serde_json::Value;
use std::{
    collections::HashSet,
    future::Future,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "stdio")]
pub mod stdio;

#[cfg(any(feature = "stdio", feature = "http"))]
/// Configured transport selection. Wire-specific options stay in this adapter crate.
#[derive(Clone)]
#[non_exhaustive]
pub enum McpTransportConfig {
    /// An owned child process.
    #[cfg(feature = "stdio")]
    Stdio(stdio::McpStdioParams),
    /// A Streamable HTTP endpoint.
    #[cfg(feature = "http")]
    StreamableHttp(http::McpHttpParams),
}

/// Default per-request deadline, Python's `client_session_timeout_seconds = 5`.
pub const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Overall deadline selected by Codex's explicit catalog policy.
pub const DEFAULT_CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum catalog size selected by Codex's explicit catalog policy.
pub const MAX_MCP_CATALOG_ITEMS: usize = 2_048;

/// Server options shared by all transports, including discovery, approval and tool guardrails.
///
/// As in Python, an enabled session timeout bounds every request: initialize, each tools/list
/// or prompts/list page, and resource requests. Catalog bounds are opt-in product policy.
/// Codex's per-server tool timeout can replace the session timeout for calls.
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)] // Each timeout bounds a different protocol scope.
pub struct McpClientOptions {
    session_timeout: Option<Duration>,
    catalog_timeout: Option<Duration>,
    max_list_pages: Option<NonZeroUsize>,
    max_catalog_items: Option<usize>,
    tool_timeout: Option<Duration>,
    cache_tools_list: bool,
    tool_filter: crate::filter::ToolFilter,
    require_approval: crate::approval::RequireApprovalSetting,
    tool_input_guardrails: Vec<ra_core::tool::ToolGuardrailId>,
    tool_output_guardrails: Vec<ra_core::tool::ToolGuardrailId>,
}
impl Default for McpClientOptions {
    fn default() -> Self {
        Self {
            session_timeout: Some(DEFAULT_SESSION_TIMEOUT),
            catalog_timeout: None,
            max_list_pages: None,
            max_catalog_items: None,
            tool_timeout: None,
            cache_tools_list: false,
            tool_filter: crate::filter::ToolFilter::default(),
            require_approval: crate::approval::RequireApprovalSetting::default(),
            tool_input_guardrails: Vec::new(),
            tool_output_guardrails: Vec::new(),
        }
    }
}
impl McpClientOptions {
    /// Sets input checks declared by every discovered tool on this server, in order.
    /// The host installs their implementations through `RunConfig::with_tool_input_guardrails`.
    #[must_use]
    pub fn with_tool_input_guardrails(
        mut self,
        guardrails: impl IntoIterator<Item = ra_core::tool::ToolGuardrailId>,
    ) -> Self {
        self.tool_input_guardrails = guardrails.into_iter().collect();
        self
    }
    /// Sets output checks declared by every discovered tool on this server, in order.
    /// The host installs their implementations through `RunConfig::with_tool_output_guardrails`.
    #[must_use]
    pub fn with_tool_output_guardrails(
        mut self,
        guardrails: impl IntoIterator<Item = ra_core::tool::ToolGuardrailId>,
    ) -> Self {
        self.tool_output_guardrails = guardrails.into_iter().collect();
        self
    }
    /// Sets Python's server-owned local tool approval policy. Defaults to never.
    #[must_use]
    pub fn with_require_approval(
        mut self,
        policy: impl Into<crate::approval::RequireApprovalSetting>,
    ) -> Self {
        self.require_approval = policy.into();
        self
    }
    /// Reuses clean, nonempty tool lists. Defaults to false, as in Python.
    #[must_use]
    pub const fn with_cache_tools_list(mut self, enabled: bool) -> Self {
        self.cache_tools_list = enabled;
        self
    }
    /// Applies a static or dynamic filter after refreshing or reading the complete catalog.
    #[must_use]
    pub fn with_tool_filter(mut self, filter: crate::filter::ToolFilter) -> Self {
        self.tool_filter = filter;
        self
    }
    /// Sets the per-request deadline, Python's `client_session_timeout_seconds`.
    /// `None` or zero disables it; an explicitly configured catalog deadline still applies.
    #[must_use]
    pub fn with_session_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.session_timeout = timeout.into().filter(|timeout| !timeout.is_zero());
        self
    }
    /// Sets the overall tools/list and prompts/list deadline, including all pages.
    #[must_use]
    pub const fn with_catalog_timeout(mut self, timeout: Duration) -> Self {
        self.catalog_timeout = Some(timeout);
        self
    }
    /// Bounds automatic tool and prompt pagination. Python defaults to no page limit.
    #[must_use]
    pub const fn with_max_list_pages(mut self, limit: Option<NonZeroUsize>) -> Self {
        self.max_list_pages = limit;
        self
    }
    /// Selects Codex's catalog policy: 100 pages, 2048 items, and 30 seconds overall.
    /// These product bounds are opt-in; explicit resource pagination is unaffected.
    #[must_use]
    pub fn with_codex_catalog_limits(mut self) -> Self {
        self.max_list_pages = NonZeroUsize::new(100);
        self.max_catalog_items = Some(MAX_MCP_CATALOG_ITEMS);
        self.catalog_timeout = Some(DEFAULT_CATALOG_TIMEOUT);
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
    initialize_result: Arc<ServerPeerInfo>,
    service: Mutex<Option<RunningService<RoleClient, ClientConfig>>>,
    options: McpClientOptions,
    request_lock: Option<Mutex<()>>,
    #[cfg(feature = "http")]
    http_session: Option<Arc<http::HttpSession>>,
}

/// A connected server. Clones and registered tools retain the same session until cleanup.
#[derive(Clone)]
pub struct McpClient {
    server_name: Arc<str>,
    session: Arc<ClientSessionServer>,
    transport_failed: Arc<AtomicBool>,
    pub(crate) tools: Arc<crate::server::tools::ToolsCache>,
}

impl McpClient {
    /// Server-wide input guardrail declarations, copied into each discovered tool.
    #[must_use]
    pub fn tool_input_guardrails(&self) -> &[ra_core::tool::ToolGuardrailId] {
        &self.session.options.tool_input_guardrails
    }
    /// Server-wide output guardrail declarations, copied into each discovered tool.
    #[must_use]
    pub fn tool_output_guardrails(&self) -> &[ra_core::tool::ToolGuardrailId] {
        &self.session.options.tool_output_guardrails
    }
    /// The server's local approval policy, shared by every discovered tool on this session.
    #[must_use]
    pub fn require_approval(&self) -> &crate::approval::RequireApprovalSetting {
        &self.session.options.require_approval
    }
    #[cfg(any(feature = "stdio", feature = "http"))]
    /// Connects a configured transport through the same initialized session implementation.
    pub async fn connect_transport(
        server_name: impl Into<String>,
        transport: McpTransportConfig,
        options: McpClientOptions,
    ) -> Result<Self> {
        let name = server_name.into();
        match transport {
            #[cfg(feature = "stdio")]
            McpTransportConfig::Stdio(params) => Self::connect_stdio(name, params, options).await,
            #[cfg(feature = "http")]
            McpTransportConfig::StreamableHttp(params) => {
                Self::connect_http(name, params, options).await
            }
        }
    }
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
        validate_server_name(&name)?;
        let initialize = rmcp::service::serve_client(ClientConfig::default(), transport);
        let service = match options.session_timeout {
            Some(timeout) => tokio::time::timeout(timeout, initialize)
                .await
                .map_err(|_| safe_error(ToolErrorKind::Timeout, "initialize timed out"))?,
            None => initialize.await,
        }
        .map_err(initialize_error)?;
        let initialize_result = service.peer().peer_info().ok_or_else(|| {
            safe_error(ToolErrorKind::ExecutionFailed, "missing initialize result")
        })?;
        Ok(Self {
            server_name: name.into(),
            transport_failed: Arc::default(),
            tools: Arc::new(crate::server::tools::ToolsCache::new(
                options.cache_tools_list,
                options.tool_filter.clone(),
            )),
            session: Arc::new(ClientSessionServer {
                initialize_result,
                service: Mutex::new(Some(service)),
                cleanup_unconfirmed: AtomicBool::new(false),
                options,
                request_lock: None,
                #[cfg(feature = "http")]
                http_session: None,
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
        self.session.initialize_result.instructions.as_deref()
    }
    /// The SDK view of initialize, including capabilities, protocol version, metadata and identity.
    #[must_use]
    pub fn server_initialize_result(&self) -> &ServerPeerInfo {
        &self.session.initialize_result
    }
    /// Whether the owned SDK service is still running.
    pub async fn is_connected(&self) -> bool {
        self.session
            .service
            .lock()
            .await
            .as_ref()
            .is_some_and(|service| !service.is_closed() && !service.peer().is_transport_closed())
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
    /// Lists every tool page. Optional catalog limits never return a partial result.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        self.list_tools_with_context(None, None).await
    }
    /// Lists tools with the run and agent required by dynamic filters, even on cache hits.
    pub async fn list_tools_with_context(
        &self,
        run_context: Option<&ra_core::context::RunContext>,
        agent: Option<&ra_core::context::RunAgent>,
    ) -> Result<Vec<Tool>> {
        Ok(self
            .list_cached_tools_with_context(run_context, agent)
            .await?
            .into_iter()
            .map(|cached| cached.tool)
            .collect())
    }
    pub(crate) async fn list_cached_tools_with_context(
        &self,
        run_context: Option<&ra_core::context::RunContext>,
        agent: Option<&ra_core::context::RunAgent>,
    ) -> Result<Vec<crate::metadata::CachedTool>> {
        self.peer().await?;
        let tools = self.tools.list(self.fetch_tools()).await?;
        self.tools
            .filter()
            .apply(tools, run_context, agent, self.server_name())
            .await
    }
    /// Replaces the active discovery filter without invalidating the unfiltered cache.
    pub fn set_tool_filter(&self, filter: crate::filter::ToolFilter) {
        self.tools.set_filter(filter);
    }
    /// Changes cache reuse for subsequent listings without discarding the snapshot.
    pub fn set_cache_tools_list(&self, enabled: bool) {
        self.tools.set_cache_tools_list(enabled);
    }
    /// Returns the last complete unfiltered listing, including when caching is disabled or dirty.
    #[must_use]
    pub fn cached_tools(&self) -> Option<Vec<Tool>> {
        self.tools.snapshot()
    }
    /// Invalidates future cache hits without discarding the last successful listing.
    pub fn invalidate_tools_cache(&self) {
        self.tools.invalidate();
    }
    async fn fetch_tools(&self) -> Result<Vec<Tool>> {
        let peer = self.peer().await?;
        collect_paginated(self.options(), "tools/list", |params| {
            let peer = peer.clone();
            async move {
                let request = params.map_or_else(
                    rmcp::model::ListToolsRequest::default,
                    rmcp::model::ListToolsRequest::with_param,
                );
                match self
                    .send_request(
                        peer,
                        ClientRequest::ListToolsRequest(request),
                        self.options().session_timeout,
                        None,
                    )
                    .await?
                {
                    ServerResult::ListToolsResult(page) => Ok((page.tools, page.next_cursor)),
                    _ => Err(unexpected_response()),
                }
            }
        })
        .await
    }
    /// Aggregates every prompt page and retains the first page's metadata, as Python does.
    pub async fn list_prompts(&self) -> Result<ListPromptsResult> {
        let peer = self.peer().await?;
        let first = Mutex::new(None);
        let prompts = collect_paginated(self.options(), "prompts/list", |params| {
            let peer = peer.clone();
            let first = &first;
            async move {
                let request =
                    params.map_or_else(ListPromptsRequest::default, ListPromptsRequest::with_param);
                match self
                    .send_request(
                        peer,
                        ClientRequest::ListPromptsRequest(request),
                        self.options().session_timeout,
                        None,
                    )
                    .await?
                {
                    ServerResult::ListPromptsResult(mut page) => {
                        let prompts = std::mem::take(&mut page.prompts);
                        let next = page.next_cursor.take();
                        let mut slot = first.lock().await;
                        if slot.is_none() {
                            *slot = Some(page);
                        }
                        Ok((prompts, next))
                    }
                    _ => Err(unexpected_response()),
                }
            }
        })
        .await?;
        let mut result = first.into_inner().unwrap_or_default();
        result.prompts = prompts;
        Ok(result)
    }
    /// Fetches a prompt. Omitted arguments remain omitted on the wire.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<GetPromptResult> {
        let mut params = GetPromptRequestParams::new(name);
        params.arguments = arguments;
        match self
            .request(ClientRequest::GetPromptRequest(GetPromptRequest::new(
                params,
            )))
            .await?
        {
            ServerResult::GetPromptResult(result) => Ok(result),
            _ => Err(unexpected_response()),
        }
    }
    /// Fetches one resource page; the caller owns pagination, including empty cursors.
    pub async fn list_resources(&self, cursor: Option<String>) -> Result<ListResourcesResult> {
        let request = cursor.map_or_else(ListResourcesRequest::default, |cursor| {
            ListResourcesRequest::with_param(
                PaginatedRequestParams::default().with_cursor(Some(cursor)),
            )
        });
        match self
            .request(ClientRequest::ListResourcesRequest(request))
            .await?
        {
            ServerResult::ListResourcesResult(result) => Ok(result),
            _ => Err(unexpected_response()),
        }
    }
    /// Fetches one resource-template page without aggregating or clearing its cursor.
    pub async fn list_resource_templates(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourceTemplatesResult> {
        let request = cursor.map_or_else(ListResourceTemplatesRequest::default, |cursor| {
            ListResourceTemplatesRequest::with_param(
                PaginatedRequestParams::default().with_cursor(Some(cursor)),
            )
        });
        match self
            .request(ClientRequest::ListResourceTemplatesRequest(request))
            .await?
        {
            ServerResult::ListResourceTemplatesResult(result) => Ok(result),
            _ => Err(unexpected_response()),
        }
    }
    /// Reads a resource URI through MCP; the URI is not treated as a host file or URL.
    pub async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
        match self
            .request(ClientRequest::ReadResourceRequest(
                ReadResourceRequest::new(ReadResourceRequestParams::new(uri)),
            ))
            .await?
        {
            ServerResult::ReadResourceResult(result) => Ok(result),
            _ => Err(unexpected_response()),
        }
    }
    async fn request(&self, request: ClientRequest) -> Result<ServerResult> {
        self.send_request(
            self.peer().await?,
            request,
            self.options().session_timeout,
            None,
        )
        .await
    }
    async fn send_request(
        &self,
        peer: Peer<RoleClient>,
        request: ClientRequest,
        timeout: Option<Duration>,
        meta: Option<JsonObject>,
    ) -> Result<ServerResult> {
        let _request = match &self.session.request_lock {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        send_request(peer, request, timeout, meta).await
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
        self.tools
            .validate_required(name, arguments.as_ref(), self.server_name())?;
        params.arguments = arguments;
        let result = self
            .send_request(
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
        #[cfg(feature = "http")]
        if let Some(session) = &self.session.http_session {
            session.clear();
        }
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
                Ok(Some(_)) => {
                    if self.transport_failed.load(Ordering::Acquire) {
                        return Err(safe_error(
                            ToolErrorKind::ExecutionFailed,
                            "MCP stdio transport failed",
                        ));
                    }
                    self.session
                        .cleanup_unconfirmed
                        .store(false, Ordering::Release);
                }
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

fn unexpected_response() -> Error {
    safe_error(
        ToolErrorKind::ExecutionFailed,
        "MCP returned an unexpected response",
    )
}

async fn collect_paginated<T, F, Fut>(
    options: &McpClientOptions,
    method: &str,
    mut fetch: F,
) -> Result<Vec<T>>
where
    F: FnMut(Option<PaginatedRequestParams>) -> Fut,
    Fut: Future<Output = Result<(Vec<T>, Option<String>)>>,
{
    let collect = async {
        let mut items = Vec::new();
        let mut cursor = None;
        let mut seen = HashSet::new();
        let mut pages = 0;
        loop {
            if options
                .max_list_pages
                .is_some_and(|limit| pages >= limit.get())
            {
                return Err(Error::tool(
                    ToolErrorKind::ExecutionFailed,
                    "mcp",
                    format!("{method} exceeded {pages} pages"),
                ));
            }
            let params = cursor
                .take()
                .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
            let (page, next) = fetch(params).await?;
            pages += 1;
            if let Some(limit) = options.max_catalog_items
                && page.len() > limit.saturating_sub(items.len())
            {
                return Err(Error::tool(
                    ToolErrorKind::ExecutionFailed,
                    "mcp",
                    format!("{method} exceeded {limit} items"),
                ));
            }
            items.extend(page);
            let Some(next) = next else {
                return Ok(items);
            };
            // Codex's cursor bound is part of its explicitly selected catalog policy.
            if options.max_catalog_items.is_some() && next.len() > 64 * 1024 {
                return Err(safe_error(
                    ToolErrorKind::ExecutionFailed,
                    "pagination cursor exceeded 65536 bytes",
                ));
            }
            if !seen.insert(next.clone()) {
                return Err(safe_error(
                    ToolErrorKind::ExecutionFailed,
                    "MCP returned a repeated pagination cursor",
                ));
            }
            cursor = Some(next);
        }
    };
    match options.catalog_timeout {
        Some(timeout) => tokio::time::timeout(timeout, collect)
            .await
            .map_err(|_| safe_error(ToolErrorKind::Timeout, "MCP catalog listing timed out"))?,
        None => collect.await,
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
                params.reason = Some("MCP request cancelled".to_owned());
                let _ = tokio::time::timeout(Duration::from_secs(2), peer.notify_cancelled(params))
                    .await;
            });
        }
    }
}

fn initialize_error(error: ClientInitializeError) -> Error {
    match error {
        ClientInitializeError::JsonRpcError(error) => service_error(ServiceError::McpError(error)),
        _ => safe_error(ToolErrorKind::ExecutionFailed, "initialize failed"),
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

fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(Error::config(
            "MCP server name must be nonempty and contain no control characters",
        ));
    }
    Ok(())
}
