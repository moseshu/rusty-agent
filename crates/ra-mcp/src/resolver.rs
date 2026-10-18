//! Per-model-call MCP discovery behind the provider-neutral tool resolver boundary.

use crate::{
    client::McpClient,
    filter::ToolFilter,
    tool::{McpDiscoveryOptions, discover_tools_with_context},
};
use ra_core::{
    context::RunContext,
    error::Result,
    tool::{Tool, ToolResolver},
};
use std::{collections::BTreeSet, sync::Arc};

/// Supplies the current session of a host-owned MCP server at discovery time.
///
/// Configured servers follow reconnects. A raw [`McpClient`] is an explicitly fixed session,
/// suitable for an in-process server or a host that does not reconnect during its lifetime.
pub trait McpClientSource: Send + Sync {
    /// Captures the session to use for one discovery and its resulting tool calls.
    fn current_client(&self) -> Result<McpClient>;
}
impl McpClientSource for McpClient {
    fn current_client(&self) -> Result<McpClient> {
        Ok(self.clone())
    }
}
#[cfg(feature = "stdio")]
impl McpClientSource for crate::client::stdio::McpServerStdio {
    fn current_client(&self) -> Result<McpClient> {
        self.client()
    }
}
#[cfg(feature = "http")]
impl McpClientSource for crate::client::http::McpServerStreamableHttp {
    fn current_client(&self) -> Result<McpClient> {
        self.client()
    }
}

/// Retains live server sources and resolves them with each model call's real run context.
/// Hosts connect before the run and retain transport ownership until it finishes.
pub struct McpToolResolver {
    servers: Vec<(Arc<dyn McpClientSource>, ToolFilter)>,
    options: McpDiscoveryOptions,
}
impl McpToolResolver {
    /// Configures joint discovery without listing tools or constructing a synthetic run context.
    #[must_use]
    pub fn new(
        servers: Vec<(Arc<dyn McpClientSource>, ToolFilter)>,
        options: McpDiscoveryOptions,
    ) -> Self {
        Self { servers, options }
    }
}
#[async_trait::async_trait]
impl ToolResolver for McpToolResolver {
    fn requires_reserved_tool_names(&self) -> bool {
        self.options.requires_reserved_tool_names()
    }

    async fn get_tools(
        &self,
        context: &RunContext,
        reserved_tool_names: &BTreeSet<String>,
    ) -> Result<Vec<Arc<dyn Tool>>> {
        let options = self.options.clone().with_reserved_tool_names(
            self.options
                .reserved_tool_names()
                .iter()
                .chain(reserved_tool_names)
                .cloned(),
        );
        let servers = self
            .servers
            .iter()
            .map(|(source, filter)| Ok((source.current_client()?, filter.clone())))
            .collect::<Result<Vec<_>>>()?;
        Ok(
            discover_tools_with_context(&servers, &options, Some(context), Some(context.agent()))
                .await?
                .into_iter()
                .map(|tool| tool as Arc<dyn Tool>)
                .collect(),
        )
    }
}
