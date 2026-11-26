// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Streamable HTTP configuration, authentication hooks, and reusable server lifecycle.

use super::{McpClient, McpClientOptions, safe_error};
use futures::future::BoxFuture;
use ra_core::error::{Result, ToolErrorKind};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use std::{collections::BTreeMap, ffi::OsString, sync::Arc, time::Duration};

mod headers_helper;
mod transport;

/// The reqwest version used by authentication hooks and client factories. Use this re-export
/// so hook types always match the adapter's HTTP stack.
pub use reqwest;

/// Safe process diagnostics a headers helper launcher can attach to its callback error.
/// Other callback errors are redacted, including any command, output or credential text.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum HttpHeadersHelperProcessError {
    /// The launcher could not start the process.
    Start(std::io::ErrorKind),
    /// The process returned a non-success exit status.
    Exit(std::process::ExitStatus),
}

impl std::fmt::Display for HttpHeadersHelperProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(kind) => write!(
                f,
                "MCP HTTP headers helper failed to start: {}",
                std::io::Error::from(*kind)
            ),
            Self::Exit(status) => match status.code() {
                Some(code) => write!(f, "MCP HTTP headers helper exited with status {code}"),
                None => write!(f, "MCP HTTP headers helper exited: {status}"),
            },
        }
    }
}

impl std::error::Error for HttpHeadersHelperProcessError {}

pub(super) fn helper_diagnostic(
    error: &rmcp::transport::DynamicTransportError,
) -> Option<ra_core::error::Error> {
    use rmcp::transport::streamable_http_client::StreamableHttpError;

    let StreamableHttpError::Client(error) = error
        .error
        .downcast_ref::<StreamableHttpError<std::io::Error>>()?
    else {
        return None;
    };
    let diagnostic = error
        .get_ref()?
        .downcast_ref::<headers_helper::HelperFailure>()?;
    Some(ra_core::error::Error::tool(
        ToolErrorKind::ExecutionFailed,
        "mcp",
        diagnostic.to_string(),
    ))
}

// Reqwest configures authentication on requests rather than on its client. Keep this native
// representation inside the adapter; OAuth challenge handling and storage are separate concerns.
type Auth = Arc<
    dyn Fn(reqwest::RequestBuilder) -> BoxFuture<'static, Result<reqwest::RequestBuilder>>
        + Send
        + Sync,
>;
type ClientFactory = Arc<
    dyn Fn(reqwest::ClientBuilder) -> std::result::Result<reqwest::Client, reqwest::Error>
        + Send
        + Sync,
>;

/// Remote parameters corresponding to Python's `MCPServerStreamableHttpParams`.
/// Secrets and user-supplied callbacks are deliberately absent from Debug output.
#[derive(Clone)]
pub struct McpHttpParams {
    url: String,
    headers: BTreeMap<String, String>,
    bearer_token_env_var: Option<String>,
    timeout: Duration,
    sse_read_timeout: Duration,
    terminate_on_close: bool,
    ignore_initialized_notification_failure: bool,
    auth: Option<Auth>,
    client_factory: Option<ClientFactory>,
    route_factory: Option<ra_http::HttpClientFactory>,
    headers_helper: Option<headers_helper::RunHelper>,
}
impl McpHttpParams {
    /// Selects an MCP endpoint. HTTP connect and stream read timeouts default to 5 and 300 seconds.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: BTreeMap::new(),
            bearer_token_env_var: None,
            timeout: Duration::from_secs(5),
            sse_read_timeout: Duration::from_mins(5),
            terminate_on_close: true,
            ignore_initialized_notification_failure: false,
            auth: None,
            client_factory: None,
            route_factory: None,
            headers_helper: None,
        }
    }
    /// Explicitly routes product requests through the owner's destination and proxy policy.
    /// Authentication callbacks still prepare each request; the selected factory owns transport.
    #[must_use]
    pub fn with_route_factory(mut self, factory: ra_http::HttpClientFactory) -> Self {
        self.route_factory = Some(factory);
        self
    }

    /// Supplies static headers.
    #[must_use]
    pub fn with_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.headers = headers;
        self
    }
    /// Loads the token at connection time; it overrides static Authorization and the auth hook.
    #[must_use]
    pub fn with_bearer_token_env_var(mut self, name: impl Into<String>) -> Self {
        self.bearer_token_env_var = Some(name.into());
        self
    }
    /// Sets the HTTP connection timeout. Reqwest has no separate write or pool timeout;
    /// the shared session deadline still bounds complete MCP requests.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    /// Sets the inactivity timeout for HTTP reads, including each SSE body chunk.
    #[must_use]
    pub const fn with_sse_read_timeout(mut self, timeout: Duration) -> Self {
        self.sse_read_timeout = timeout;
        self
    }
    /// Selects whether cleanup sends DELETE for a stateful session. Defaults to true.
    #[must_use]
    pub const fn with_terminate_on_close(mut self, terminate: bool) -> Self {
        self.terminate_on_close = terminate;
        self
    }
    /// Opts into ignoring HTTP failures only for `notifications/initialized`.
    /// Defaults to false. Other notifications and all requests retain their errors.
    #[must_use]
    pub const fn with_ignore_initialized_notification_failure(mut self, ignore: bool) -> Self {
        self.ignore_initialized_notification_failure = ignore;
        self
    }
    /// Applies native request authentication to every POST, GET and DELETE after static headers.
    /// The callback can asynchronously obtain credentials; its errors are redacted.
    /// Multi-request challenge flows and OAuth token storage are outside this hook.
    #[must_use]
    pub fn with_auth<F, Fut>(mut self, auth: F) -> Self
    where
        F: Fn(reqwest::RequestBuilder) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<reqwest::RequestBuilder>> + Send + 'static,
    {
        self.auth = Some(Arc::new(move |request| Box::pin(auth(request))));
        self
    }
    /// Sets a connection-scoped Codex headers helper. The launcher receives the MCP subprocess
    /// environment and maximum stdout size, and returns the command's JSON output. The transport
    /// applies the decoded headers to the configured origin and refreshes them after rejected POSTs.
    /// A launcher can attach [`HttpHeadersHelperProcessError`] as a safe error source.
    #[must_use]
    pub fn with_http_headers_helper<F, Fut>(mut self, helper: F) -> Self
    where
        F: Fn(Vec<(OsString, OsString)>, usize) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<u8>>> + Send + 'static,
    {
        self.headers_helper = Some(Arc::new(move |env, limit| Box::pin(helper(env, limit))));
        self
    }
    /// Builds a client at each connection from a builder containing the configured headers,
    /// timeouts and disabled redirects. A factory may replace these defaults, as in Python.
    /// Authentication remains a request hook because reqwest has no client-level auth handler.
    #[must_use]
    pub fn with_http_client_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn(reqwest::ClientBuilder) -> std::result::Result<reqwest::Client, reqwest::Error>
            + Send
            + Sync
            + 'static,
    {
        self.client_factory = Some(Arc::new(factory));
        self
    }
}

impl McpClient {
    /// Initializes a Streamable HTTP session using the SDK's protocol handling and shutdown.
    pub async fn connect_http(
        server_name: impl Into<String>,
        params: McpHttpParams,
        options: McpClientOptions,
    ) -> Result<Self> {
        let name = server_name.into();
        super::validate_server_name(&name)?;
        let backend = transport::HttpClient::new(&params)?;
        let session = backend.session.clone();
        let mut guard = transport::ConnectGuard::new(backend.clone());
        // Python's initialized-session path does not transparently reinitialize or replay a
        // tools/call after a 404. Codex recovery is a separately selected product mechanism.
        let transport = StreamableHttpClientTransport::with_client(
            backend,
            StreamableHttpClientTransportConfig::with_uri(params.url)
                .reinit_on_expired_session(false),
        );
        let mut client = match Self::connect(name, transport, options).await {
            Ok(client) => client,
            Err(error) => {
                // Python's transport logs a failed DELETE, so the connection error is returned.
                // Only cancellation needs the guard's detached fallback; Drop cannot await.
                guard.cleanup().await;
                return Err(error);
            }
        };
        let state = Arc::get_mut(&mut client.session).ok_or_else(|| {
            safe_error(ToolErrorKind::ExecutionFailed, "MCP session setup failed")
        })?;
        state.request_lock = Some(tokio::sync::Mutex::new(()));
        state.http_session = Some(session);
        guard.disarm();
        Ok(client)
    }
    /// Current HTTP session ID, or None for a stateless, non-HTTP or cleaned-up session.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.session.http_session.as_ref()?.id()
    }
}

pub(crate) use transport::HttpSession;

/// A reusable configured server, corresponding to Python's `MCPServerStreamableHttp`.
/// All protocol operations use the same shared client as stdio and in-process transports.
pub struct McpServerStreamableHttp {
    name: String,
    params: McpHttpParams,
    options: McpClientOptions,
    connection: crate::server::ServerConnection,
}
impl McpServerStreamableHttp {
    /// Configures a disconnected server. The default name is `streamable_http: <url>`.
    #[must_use]
    pub fn new(params: McpHttpParams) -> Self {
        Self {
            name: format!("streamable_http: {}", params.url),
            params,
            options: McpClientOptions::default(),
            connection: crate::server::ServerConnection::default(),
        }
    }
    /// Sets the host-assigned name before connecting.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
    /// Sets shared session options before connecting.
    #[must_use]
    pub fn with_options(mut self, options: McpClientOptions) -> Self {
        self.connection
            .tools
            .configure(options.cache_tools_list, options.tool_filter.clone());
        self.options = options;
        self
    }
    /// Replaces the active filter, including before connection and across reconnects.
    pub fn set_tool_filter(&self, filter: crate::filter::ToolFilter) {
        self.connection.tools.set_filter(filter);
    }
    /// Changes cache reuse without invalidating the last successful listing.
    pub fn set_cache_tools_list(&self, enabled: bool) {
        self.connection.tools.set_cache_tools_list(enabled);
    }
    /// Returns the connected snapshot used by discovery. A reconnect never retargets it.
    pub fn client(&self) -> Result<McpClient> {
        self.connection.client()
    }
    /// Reports the current session's liveness without connecting.
    pub async fn is_connected(&self) -> bool {
        self.connection.is_connected().await
    }
    /// Returns the server-assigned session ID, or None before connect and after cleanup.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.client().ok()?.session_id()
    }
}
#[async_trait::async_trait]
impl crate::server::McpServer for McpServerStreamableHttp {
    fn name(&self) -> &str {
        &self.name
    }
    async fn connect(&self) -> Result<()> {
        self.connection
            .connect(
                &self.name,
                Box::pin(McpClient::connect_http(
                    &self.name,
                    self.params.clone(),
                    self.options.clone(),
                )),
            )
            .await
    }
    async fn cleanup(&self) -> Result<()> {
        self.connection.cleanup(&self.name).await
    }
    async fn list_tools(&self) -> Result<Vec<rmcp::model::Tool>> {
        self.client()?.list_tools().await
    }
    async fn list_tools_with_context(
        &self,
        run_context: Option<&ra_core::context::RunContext>,
        agent: Option<&ra_core::context::RunAgent>,
    ) -> Result<Vec<rmcp::model::Tool>> {
        self.client()?
            .list_tools_with_context(run_context, agent)
            .await
    }
    fn cached_tools(&self) -> Option<Vec<rmcp::model::Tool>> {
        self.connection.tools.snapshot()
    }
    fn invalidate_tools_cache(&self) {
        self.connection.tools.invalidate();
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        meta: Option<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult> {
        self.client()?.call_tool(name, arguments, meta).await
    }
    async fn list_prompts(&self) -> Result<rmcp::model::ListPromptsResult> {
        self.client()?.list_prompts().await
    }
    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::GetPromptResult> {
        self.client()?.get_prompt(name, arguments).await
    }
    async fn list_resources(
        &self,
        cursor: Option<String>,
    ) -> Result<rmcp::model::ListResourcesResult> {
        self.client()?.list_resources(cursor).await
    }
    async fn list_resource_templates(
        &self,
        cursor: Option<String>,
    ) -> Result<rmcp::model::ListResourceTemplatesResult> {
        self.client()?.list_resource_templates(cursor).await
    }
    async fn read_resource(&self, uri: &str) -> Result<rmcp::model::ReadResourceResult> {
        self.client()?.read_resource(uri).await
    }
}

/// Access to the request transport for tests that need overlapping requests on one connection.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub fn transport_for_test(
    params: &McpHttpParams,
) -> Result<
    impl rmcp::transport::streamable_http_client::StreamableHttpClient<Error = std::io::Error> + use<>,
> {
    transport::HttpClient::new(params)
}
