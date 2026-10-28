//! Explicit product plugin-management port and invalidation notifications.
use super::{
    outgoing::{OutgoingMessageSender, ServerNotification},
    protocol::PluginManagementRequest,
};
use crate::frame::JsonRpcErrorObject;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::{Arc, PoisonError, RwLock};
use tokio::sync::watch;

/// Product-owned configuration and package operations, installed independently of
/// provider-neutral threads.
#[async_trait]
pub trait PluginManagement: Send + Sync + 'static {
    /// Handles an already parsed operation using the product's real config and package services.
    async fn handle(&self, request: PluginManagementRequest) -> Result<Value, JsonRpcErrorObject>;
    /// Receives invalidations; notification acknowledges a state change, not runtime readiness.
    fn subscribe(&self) -> watch::Receiver<u64>;
}
#[derive(Default)]
pub(crate) struct PluginControlSlot(RwLock<Option<Arc<dyn PluginManagement>>>);
impl std::fmt::Debug for PluginControlSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginControlSlot").finish_non_exhaustive()
    }
}
impl PluginControlSlot {
    pub(crate) fn set(&self, control: Arc<dyn PluginManagement>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(control);
    }
    pub(crate) fn get(&self) -> Result<Arc<dyn PluginManagement>, JsonRpcErrorObject> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                JsonRpcErrorObject::invalid_request("Plugin management is not configured")
            })
    }
    pub(crate) fn forward(&self, outgoing: Arc<OutgoingMessageSender>) -> Option<PluginForwarder> {
        let control = self.get().ok()?;
        let mut changes = control.subscribe();
        Some(PluginForwarder(tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                changes.borrow_and_update();
                outgoing
                    .send_server_notification(ServerNotification::new(
                        "skills/changed",
                        serde_json::json!({}),
                    ))
                    .await;
            }
        })))
    }
}
pub(crate) struct PluginForwarder(tokio::task::JoinHandle<()>);
impl Drop for PluginForwarder {
    fn drop(&mut self) {
        self.0.abort();
    }
}
