// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! In-process SDK server over a bounded memory stream, with no OS IPC or network.

use crate::client::{McpClient, McpClientOptions, safe_error};
use ra_core::error::{Result, ToolErrorKind};
use rmcp::{ServerHandler, ServiceExt};
use std::time::Duration;

/// Owns the server task paired with an initialized client.
/// Host tools are implemented through the official SDK's `ServerHandler`.
pub struct InProcessToolServer {
    client: McpClient,
    server_cancel: tokio_util::sync::CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl InProcessToolServer {
    /// Starts a handler and completes initialize before returning either endpoint.
    pub async fn connect<H: ServerHandler>(
        server_name: impl Into<String>,
        handler: H,
        options: McpClientOptions,
    ) -> Result<Self> {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server_cancel = tokio_util::sync::CancellationToken::new();
        let startup_guard = server_cancel.clone().drop_guard();
        let task_cancel = server_cancel.clone();
        let task = tokio::spawn(async move {
            if let Ok(service) = handler.serve_with_ct(server_stream, task_cancel).await {
                let _ = service.waiting().await;
            }
        });
        match McpClient::connect(server_name, client_stream, options).await {
            Ok(client) => {
                startup_guard.disarm();
                Ok(Self {
                    client,
                    server_cancel,
                    task: Some(task),
                })
            }
            Err(error) => {
                task.abort();
                let _ = task.await;
                Err(error)
            }
        }
    }
    /// Clones the initialized client for discovery and registration.
    #[must_use]
    pub fn client(&self) -> McpClient {
        self.client.clone()
    }
    /// Closes both endpoints and joins the server task. Repeated cleanup succeeds.
    pub async fn cleanup(&mut self) -> Result<()> {
        self.server_cancel.cancel();
        self.client.cleanup().await?;
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    return Err(safe_error(
                        ToolErrorKind::ExecutionFailed,
                        "in-process MCP server task failed",
                    ));
                }
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    return Err(safe_error(
                        ToolErrorKind::Timeout,
                        "in-process MCP cleanup timed out",
                    ));
                }
            }
        }
        Ok(())
    }
}
impl Drop for InProcessToolServer {
    fn drop(&mut self) {
        self.server_cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
