// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The product's executor for MCP hooks: Codex's `CoreHookMcpExecutor`, calling the thread's MCP
//! runtime on behalf of a configured hook handler.
//!
//! # Differences from Codex
//!
//! - The call goes through [`CodingMcpRuntime::call_tool_now`], the product's counterpart of
//!   `McpRuntime::latest_call_tool` without waiting for the server; see its own differences.
//! - Content blocks are read through their wire form, as Codex reads its converted result.

use std::sync::Arc;

use anyhow::bail;
use futures::FutureExt;
use futures::future::BoxFuture;
use ra_core::session::SessionId;
use serde_json::Value;

use crate::hooks::{HookMcpCall, HookMcpExecutor};
use crate::mcp::CodingMcpRuntime;

/// Calls a hook's MCP tool on the thread's MCP runtime.
pub struct CodingHookMcpExecutor {
    runtime: Arc<CodingMcpRuntime>,
    // Session-scoped MCP tools require the owning thread ID in request metadata.
    thread_id: SessionId,
}

impl CodingHookMcpExecutor {
    /// An executor for the thread `thread_id`, calling servers registered in `runtime`.
    #[must_use]
    pub const fn new(runtime: Arc<CodingMcpRuntime>, thread_id: SessionId) -> Self {
        Self { runtime, thread_id }
    }
}

impl HookMcpExecutor for CodingHookMcpExecutor {
    fn execute(&self, call: HookMcpCall) -> BoxFuture<'_, anyhow::Result<String>> {
        async move {
            let mut metadata = call.metadata.unwrap_or_default();
            metadata.insert(
                "threadId".to_string(),
                Value::String(self.thread_id.to_string()),
            );

            let result = self
                .runtime
                .call_tool_now(
                    &call.server,
                    &call.tool,
                    call.environment_id.as_deref(),
                    Some(Value::Object(call.input)),
                    Some(metadata),
                    Some(call.timeout),
                )
                .await?;
            let content = result
                .content
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            let text = content
                .iter()
                .filter_map(|content| {
                    (content.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| content.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n");
            if result.is_error == Some(true) {
                bail!("MCP tool returned an error: {text}");
            }

            Ok(text)
        }
        .boxed()
    }
}
