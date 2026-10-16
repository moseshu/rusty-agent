//! Provider-independent server lifecycle and protocol surface, following Python's `MCPServer`.

use async_trait::async_trait;
use ra_core::error::{Result, ToolErrorKind};
use rmcp::model::{
    CallToolResult, GetPromptResult, JsonObject, ListPromptsResult, ListResourceTemplatesResult,
    ListResourcesResult, ReadResourceResult, Tool,
};
use serde_json::Value;

/// A configured MCP server, distinct from a connected session handle.
///
/// Hosts connect before discovery and clean up after the run. Cache, filtering and approval
/// policy belong to the server adapter, never to the runner. Protocol types stay in `ra-mcp`.
#[async_trait]
pub trait McpServer: Send + Sync {
    /// Host-readable server identity.
    fn name(&self) -> &str;
    /// Opens the configured transport and initializes its session.
    async fn connect(&self) -> Result<()>;
    /// Closes the owned session and transport.
    async fn cleanup(&self) -> Result<()>;
    /// Lists all available tools.
    async fn list_tools(&self) -> Result<Vec<Tool>>;
    /// Invokes the original tool name, preserving omitted arguments and request metadata.
    async fn call_tool(
        &self,
        name: &str,
        arguments: Option<Value>,
        meta: Option<JsonObject>,
    ) -> Result<CallToolResult>;
    /// Returns a detached cache snapshot when the adapter implements caching.
    fn cached_tools(&self) -> Option<Vec<Tool>> {
        None
    }
    /// Lists all available prompts.
    async fn list_prompts(&self) -> Result<ListPromptsResult>;
    /// Fetches one prompt with optional arguments.
    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<GetPromptResult>;
    /// Fetches one resource page. Resource support is optional, as in Python.
    async fn list_resources(&self, _cursor: Option<String>) -> Result<ListResourcesResult> {
        Err(unsupported_resource_operation("list_resources"))
    }
    /// Fetches one resource-template page.
    async fn list_resource_templates(
        &self,
        _cursor: Option<String>,
    ) -> Result<ListResourceTemplatesResult> {
        Err(unsupported_resource_operation("list_resource_templates"))
    }
    /// Reads a resource by its opaque MCP URI.
    async fn read_resource(&self, _uri: &str) -> Result<ReadResourceResult> {
        Err(unsupported_resource_operation("read_resource"))
    }
}

fn unsupported_resource_operation(operation: &str) -> ra_core::error::Error {
    ra_core::error::Error::tool(
        ToolErrorKind::ExecutionFailed,
        "mcp",
        format!("MCP server does not implement {operation}"),
    )
}

// Shared configured-server lifecycle. Transports supply only the initialize future.
//
// Connect and cleanup are serialized on `lifecycle`; requests only read `state`, so they never wait
// for an initialize in progress and see "not initialized" as Python's `session is None` does.
// Python lets cleanup close the exit stack under a pending connect. Rust cannot close a future
// it does not own, so cleanup cancels active and queued connects, dropping their transports.
// A shared cancellation scope stays closed until every cleanup finishes or is dropped.
#[cfg(any(feature = "stdio", feature = "http"))]
#[derive(Default)]
pub(crate) struct ServerConnection {
    lifecycle: tokio::sync::Mutex<()>,
    state: std::sync::Mutex<ConnectionState>,
}
#[cfg(any(feature = "stdio", feature = "http"))]
#[derive(Default)]
struct ConnectionState {
    client: Option<crate::client::McpClient>,
    connect_cancellation: tokio_util::sync::CancellationToken,
    active_cleanups: usize,
}
#[cfg(any(feature = "stdio", feature = "http"))]
impl ServerConnection {
    fn state(&self) -> std::sync::MutexGuard<'_, ConnectionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(crate) fn client(&self) -> Result<crate::client::McpClient> {
        self.state().client.clone().ok_or_else(|| {
            ra_core::error::Error::config(
                "Server not initialized. Make sure you call connect() first.",
            )
        })
    }
    pub(crate) async fn is_connected(&self) -> bool {
        let client = self.state().client.clone();
        match client {
            Some(client) => client.is_connected().await,
            None => false,
        }
    }
    pub(crate) async fn connect(
        &self,
        name: &str,
        initialize: impl std::future::Future<Output = Result<crate::client::McpClient>>,
    ) -> Result<()> {
        // Capture before queueing: cleanup also cancels callers waiting for the lifecycle lock.
        let cancelled = self.state().connect_cancellation.clone();
        tokio::select! {
            biased;
            () = cancelled.cancelled() => Err(connect_cancelled_error()),
            result = async {
                let _lifecycle = self.lifecycle.lock().await;
                let current = self.state().client.clone();
                if let Some(client) = current {
                    if client.is_connected().await {
                        return Ok(());
                    }
                    // Cleanup failures do not prevent replacing a dead session.
                    self.state().client = None;
                    log_cleanup_failure(name, client.cleanup().await);
                }
                let client = initialize.await?;
                let mut state = self.state();
                // Publishing and starting cleanup use the same state lock.
                if cancelled.is_cancelled() {
                    return Err(connect_cancelled_error());
                }
                state.client = Some(client);
                Ok(())
            } => result,
        }
    }
    // Python's cleanup always clears the session in `finally` and logs non-HTTP teardown errors
    // instead of raising them. The session handle itself still reports them from its own cleanup.
    pub(crate) async fn cleanup(&self, name: &str) -> Result<()> {
        let _cleanup = CleanupScope::new(self);
        let _lifecycle = self.lifecycle.lock().await;
        let client = self.state().client.take();
        if let Some(client) = client {
            log_cleanup_failure(name, client.cleanup().await);
        }
        Ok(())
    }
}
#[cfg(any(feature = "stdio", feature = "http"))]
struct CleanupScope<'a>(&'a ServerConnection);

#[cfg(any(feature = "stdio", feature = "http"))]
impl<'a> CleanupScope<'a> {
    fn new(connection: &'a ServerConnection) -> Self {
        let mut state = connection.state();
        state.active_cleanups += 1;
        state.connect_cancellation.cancel();
        Self(connection)
    }
}

#[cfg(any(feature = "stdio", feature = "http"))]
impl Drop for CleanupScope<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.active_cleanups -= 1;
        if state.active_cleanups == 0 {
            state.connect_cancellation = tokio_util::sync::CancellationToken::new();
        }
    }
}

#[cfg(any(feature = "stdio", feature = "http"))]
fn connect_cancelled_error() -> ra_core::error::Error {
    ra_core::error::Error::config("MCP server connect was cancelled by cleanup")
}

#[cfg(any(feature = "stdio", feature = "http"))]
fn log_cleanup_failure(name: &str, result: Result<()>) {
    if let Err(error) = result {
        tracing::warn!(server = %crate::catalog::safe_server_name(name), %error, "Error cleaning up MCP server");
    }
}
