//! Host-owned MCP runtime controls; no transport or provider dependencies enter the protocol.

use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use futures::future::BoxFuture;
use tokio::sync::broadcast;

use super::{
    outgoing::{OutgoingMessageSender, ServerNotification},
    protocol::{ListMcpServerStatusParams, ListMcpServerStatusResponse, McpServerStatus},
};
use crate::frame::JsonRpcErrorObject;

/// A product's explicitly installed MCP control port. Connections remain owned by the host.
#[async_trait]
pub trait McpRuntimeControl: Send + Sync + 'static {
    /// Reads connection snapshots without waiting for startup or doing network I/O.
    fn get_mcp_status(&self) -> Vec<McpServerStatus>;
    /// Captures one server's status and session synchronously, returning detached inventory I/O.
    /// Implementations must discard inventory if that captured session's generation changes.
    fn inventory(&self, name: &str, full: bool) -> BoxFuture<'static, McpServerStatus>;
    /// Reloads effective registrations while retaining unchanged healthy connections.
    async fn reload_mcp_servers(&self) -> Result<(), JsonRpcErrorObject>;
    /// Retries one or all enabled servers, optionally limiting retries to failed connections.
    async fn reconnect_mcp_server(
        &self,
        name: Option<&str>,
        failed_only: bool,
    ) -> Result<(), JsonRpcErrorObject>;
    /// Enables or disables a configured server for subsequent model calls.
    async fn toggle_mcp_server(&self, name: &str, enabled: bool) -> Result<(), JsonRpcErrorObject>;
    /// Cleans the host runtime when the app server stops.
    async fn shutdown(&self);
    /// Receives live state changes. A lagged consumer can resynchronize through the snapshot.
    fn subscribe(&self) -> broadcast::Receiver<McpServerStatus>;
}

#[derive(Default)]
pub(crate) struct McpControlSlot(RwLock<Option<Arc<dyn McpRuntimeControl>>>);

impl std::fmt::Debug for McpControlSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpControlSlot").finish_non_exhaustive()
    }
}

impl McpControlSlot {
    pub(crate) fn set(&self, control: Arc<dyn McpRuntimeControl>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(control);
    }

    pub(crate) fn get(&self) -> Result<Arc<dyn McpRuntimeControl>, JsonRpcErrorObject> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                JsonRpcErrorObject::invalid_request("MCP runtime control is not configured")
            })
    }

    pub(crate) fn forward(&self, outgoing: Arc<OutgoingMessageSender>) -> Option<Forwarder> {
        let control = self.get().ok()?;
        let mut receiver = control.subscribe();
        Some(Forwarder(tokio::spawn(async move {
            loop {
                let statuses = match receiver.recv().await {
                    Ok(status) => vec![status],
                    Err(broadcast::error::RecvError::Lagged(_)) => control.get_mcp_status(),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                for status in statuses {
                    use super::protocol::McpServerConnectionStatus as State;
                    let startup = match status.runtime_status() {
                        Some(State::Starting) => Some("starting"),
                        Some(State::Connected) => Some("ready"),
                        Some(State::Failed | State::AuthenticationRequired) => Some("failed"),
                        Some(State::Cancelled) => Some("cancelled"),
                        _ => None,
                    };
                    if let Some(startup) = startup {
                        let error = if startup == "failed" {
                            status.startup_error()
                        } else {
                            None
                        };
                        let params = serde_json::json!({
                            "threadId": null, "name": status.name(), "status": startup,
                            "error": error,
                            "failureReason": null,
                        });
                        outgoing
                            .send_server_notification(ServerNotification::new(
                                "mcpServer/startupStatus/updated",
                                params,
                            ))
                            .await;
                    }
                    // Toggle is a product extension: Codex's startup enum has no disabled state.
                    outgoing
                        .send_server_notification(ServerNotification::new(
                            "mcpServer/status/updated",
                            serde_json::json!({"server": status}),
                        ))
                        .await;
                }
            }
        })))
    }
}

pub(crate) struct Forwarder(tokio::task::JoinHandle<()>);
impl Drop for Forwarder {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn prepare_list(
    control: &dyn McpRuntimeControl,
    params: ListMcpServerStatusParams,
) -> Result<BoxFuture<'static, ListMcpServerStatusResponse>, JsonRpcErrorObject> {
    if params.thread_id.is_some() {
        return Err(JsonRpcErrorObject::invalid_request(
            "mcpServerStatus/list.threadId is not supported by the shared host runtime",
        ));
    }
    let mut statuses = control.get_mcp_status();
    statuses.sort_by(|a, b| a.name().cmp(b.name()));
    statuses.retain(|status| {
        params
            .server_name
            .as_deref()
            .is_none_or(|name| status.name() == name)
    });
    let total = statuses.len();
    let start = params.cursor.map_or(Ok(0), |cursor| {
        cursor
            .parse::<usize>()
            .map_err(|_| JsonRpcErrorObject::invalid_request(format!("invalid cursor: {cursor}")))
    })?;
    if start > total {
        return Err(JsonRpcErrorObject::invalid_request(format!(
            "cursor {start} exceeds total MCP servers {total}"
        )));
    }
    let limit = params.limit.map_or(total, |n| n.max(1) as usize);
    let end = start.saturating_add(limit).min(total);
    let full = params.detail != Some(super::protocol::McpServerStatusDetail::ToolsAndAuthOnly);
    let inventories: Vec<_> = statuses[start..end]
        .iter()
        .map(|status| control.inventory(status.name(), full))
        .collect();
    Ok(Box::pin(async move {
        ListMcpServerStatusResponse {
            data: futures::future::join_all(inventories).await,
            next_cursor: (end < total).then(|| end.to_string()),
        }
    }))
}
