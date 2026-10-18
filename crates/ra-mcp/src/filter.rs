// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Static and contextual filtering using original protocol tool names.

use crate::catalog::McpToolInfo;
use crate::metadata::CachedTool;
use futures::future::BoxFuture;
use ra_core::{
    context::{RunAgent, RunContext},
    error::{Error, Result},
};
use rmcp::model::Tool;
use std::{collections::HashSet, sync::Arc};

/// Borrowed run and agent views corresponding to Python's `ToolFilterContext`.
/// The agent is the public identity projection, never an executable declaration.
///
/// ```compile_fail
/// fn bypass_dispatch(context: &ra_mcp::filter::ToolFilterContext<'_>) {
///     let _ = context.agent().tools();
/// }
/// ```
pub struct ToolFilterContext<'a> {
    run_context: &'a RunContext,
    agent: &'a RunAgent,
    server_name: &'a str,
}
impl<'a> ToolFilterContext<'a> {
    /// Creates the context supplied to a dynamic filter.
    #[must_use]
    pub const fn new(
        run_context: &'a RunContext,
        agent: &'a RunAgent,
        server_name: &'a str,
    ) -> Self {
        Self {
            run_context,
            agent,
            server_name,
        }
    }
    /// The current run, including its host application context.
    #[must_use]
    pub const fn run_context(&self) -> &'a RunContext {
        self.run_context
    }
    /// The agent requesting the list.
    #[must_use]
    pub const fn agent(&self) -> &'a RunAgent {
        self.agent
    }
    /// The original server name, before model-name allocation.
    #[must_use]
    pub const fn server_name(&self) -> &'a str {
        self.server_name
    }
}

/// An asynchronous predicate inspecting an owned tool snapshot and borrowed run context.
pub type ToolFilterCallable =
    Arc<dyn for<'a> Fn(ToolFilterContext<'a>, Tool) -> BoxFuture<'a, Result<bool>> + Send + Sync>;

/// Python's static-or-callable filter. The default applies no restrictions.
#[derive(Clone)]
#[non_exhaustive]
pub enum ToolFilter {
    /// An optional allowlist followed by a blocklist.
    Static(ToolFilterStatic),
    /// A predicate evaluated on each listing, including cache hits.
    Dynamic(ToolFilterCallable),
}
impl Default for ToolFilter {
    fn default() -> Self {
        Self::Static(ToolFilterStatic::default())
    }
}
impl std::fmt::Debug for ToolFilter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(filter) => formatter.debug_tuple("Static").field(filter).finish(),
            Self::Dynamic(_) => formatter.write_str("Dynamic(<callback>)"),
        }
    }
}
impl ToolFilter {
    /// Creates a static filter, preserving the original constructor's allow/block semantics.
    #[must_use]
    pub fn new(enabled: Option<HashSet<String>>, disabled: HashSet<String>) -> Self {
        Self::Static(ToolFilterStatic::new(enabled, disabled))
    }
    /// Wraps a synchronous fallible predicate.
    #[must_use]
    pub fn dynamic_sync<F>(filter: F) -> Self
    where
        F: Fn(ToolFilterContext<'_>, Tool) -> Result<bool> + Send + Sync + 'static,
    {
        Self::Dynamic(Arc::new(move |context, tool| {
            Box::pin(std::future::ready(filter(context, tool)))
        }))
    }
    /// Wraps an asynchronous predicate, whose future may borrow the supplied context.
    #[must_use]
    pub fn dynamic<F>(filter: F) -> Self
    where
        F: for<'a> Fn(ToolFilterContext<'a>, Tool) -> BoxFuture<'a, Result<bool>>
            + Send
            + Sync
            + 'static,
    {
        Self::Dynamic(Arc::new(filter))
    }
    pub(crate) async fn apply(
        &self,
        tools: Vec<CachedTool>,
        run_context: Option<&RunContext>,
        agent: Option<&RunAgent>,
        server_name: &str,
    ) -> Result<Vec<CachedTool>> {
        let filter = match self {
            Self::Static(filter) => {
                return Ok(tools
                    .into_iter()
                    .filter(|tool| filter.allows(&tool.tool.name))
                    .collect());
            }
            Self::Dynamic(filter) => filter,
        };
        let (Some(run_context), Some(agent)) = (run_context, agent) else {
            return Err(Error::config(
                "run_context and agent are required for dynamic tool filtering",
            ));
        };
        let mut filtered = Vec::new();
        for tool in tools {
            let context = ToolFilterContext::new(run_context, agent, server_name);
            match filter(context, tool.tool.clone()).await {
                Ok(true) => filtered.push(tool),
                Ok(false) => {}
                Err(_) => tracing::error!("Error applying MCP tool filter"),
            }
        }
        Ok(filtered)
    }
}

/// Constructs a static filter; absent lists mean no filter, unlike an empty allowlist.
#[must_use]
pub fn create_static_tool_filter(
    allowed_tool_names: Option<Vec<String>>,
    blocked_tool_names: Option<Vec<String>>,
) -> Option<ToolFilter> {
    if allowed_tool_names.is_none() && blocked_tool_names.is_none() {
        None
    } else {
        Some(ToolFilter::new(
            allowed_tool_names.map(|names| names.into_iter().collect()),
            blocked_tool_names.unwrap_or_default().into_iter().collect(),
        ))
    }
}

/// An optional allowlist followed by a denylist; denial always wins.
#[derive(Debug, Default, Clone)]
pub struct ToolFilterStatic {
    enabled: Option<HashSet<String>>,
    disabled: HashSet<String>,
}

impl ToolFilterStatic {
    /// Creates a filter. An empty explicit allowlist excludes every tool.
    #[must_use]
    pub fn new(enabled: Option<HashSet<String>>, disabled: HashSet<String>) -> Self {
        Self { enabled, disabled }
    }
    /// Whether an original MCP name passes both lists.
    #[must_use]
    pub fn allows(&self, tool_name: &str) -> bool {
        if let Some(enabled) = &self.enabled
            && !enabled.contains(tool_name)
        {
            return false;
        }
        !self.disabled.contains(tool_name)
    }
    /// Applies the filter without changing original or allocated identities.
    #[must_use]
    pub fn filter_tools(&self, tools: Vec<McpToolInfo>) -> Vec<McpToolInfo> {
        tools
            .into_iter()
            .filter(|tool| self.allows(&tool.tool.name))
            .collect()
    }
}

/// Tools without UI visibility metadata remain visible; explicit visibility must include model.
#[must_use]
pub fn tool_is_model_visible(tool: &rmcp::model::Tool) -> bool {
    let Some(visibility) = tool
        .meta
        .as_deref()
        .and_then(|meta| meta.get("ui"))
        .and_then(serde_json::Value::as_object)
        .and_then(|ui| ui.get("visibility"))
        .and_then(serde_json::Value::as_array)
    else {
        return true;
    };
    visibility
        .iter()
        .any(|target| target.as_str() == Some("model"))
}
