//! Prepared catalogs separate fallible server discovery from joint naming and executable binding.

use super::{
    McpDiscoveryOptions, McpTool, McpToolInfo, McpToolNaming, name_tools_for_function_calling,
    normalize_tools_for_model_with_prefix, warn,
};
use crate::{
    client::{McpClient, McpClientOptions},
    filter::{ToolFilter, tool_is_model_visible},
    metadata::CachedTool,
};
use async_trait::async_trait;
use ra_core::{
    context::{RunAgent, RunContext},
    error::{Error, Result},
};
use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

/// Resolves a transport when a tool is called. Fixed sessions remain fixed; a product may
/// explicitly bind a cached declaration to a deferred startup and its shared recovery attempt.
#[async_trait]
pub trait McpToolClientSource: Send + Sync {
    /// Resolves the initialized session for this invocation without replaying tools/call.
    async fn client(&self) -> Result<McpClient>;
}
#[async_trait]
impl McpToolClientSource for McpClient {
    async fn client(&self) -> Result<McpClient> {
        Ok(self.clone())
    }
}

/// A complete unfiltered catalog and its policy, independent of transport availability.
/// Hosts may retain it across deferred startup; discovery filters still run for each model call.
#[derive(Clone)]
pub struct McpToolCatalog {
    name: String,
    tools: Vec<CachedTool>,
    options: McpClientOptions,
    instructions: Option<String>,
    filtered: bool,
}
impl McpToolCatalog {
    /// Seeds a complete catalog from trusted host configuration or a persisted cache. The options
    /// must be the configured server's policy, including approval and tool guardrails.
    pub fn new(
        name: impl Into<String>,
        tools: Vec<rmcp::model::Tool>,
        options: McpClientOptions,
    ) -> Self {
        Self {
            name: name.into(),
            tools: tools.into_iter().map(CachedTool::from).collect(),
            options,
            instructions: None,
            filtered: false,
        }
    }
    /// Fetches every page without applying run-dependent filters. Listing failures propagate to
    /// the host, which can isolate that server before joint name allocation.
    pub async fn fetch(client: &McpClient) -> Result<Self> {
        let tools = client.list_unfiltered_cached_tools().await?;
        let mut options = client.options().clone();
        options = options.with_tool_filter(client.tools.filter());
        Ok(Self {
            name: client.server_name().to_owned(),
            tools,
            options,
            instructions: client.server_instructions().map(str::to_owned),
            filtered: false,
        })
    }
    pub(super) async fn filtered(
        mut self,
        filter: &ToolFilter,
        run_context: Option<&RunContext>,
        agent: Option<&RunAgent>,
    ) -> Result<Self> {
        let tools = if self.filtered {
            self.tools
        } else {
            self.options
                .tool_filter()
                .apply(self.tools, run_context, agent, &self.name)
                .await?
        };
        self.tools = filter.apply(tools, run_context, agent, &self.name).await?;
        self.filtered = true;
        Ok(self)
    }
    /// Retains initialize instructions in a seeded cache.
    #[must_use]
    pub fn with_server_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }
    /// Original registry identity.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.name
    }
    /// Raw unfiltered declarations for inventory consumers.
    #[must_use]
    pub fn tools(&self) -> Vec<rmcp::model::Tool> {
        self.tools
            .iter()
            .map(|cached| cached.tool.clone())
            .collect()
    }
    /// Whether the cached catalog contains a model-visible tool allowed by both static filters.
    /// Dynamic filters require a run, so their first startup remains eager.
    #[must_use]
    pub fn allows_deferred_startup(&self, filter: &ToolFilter) -> bool {
        let (ToolFilter::Static(server), ToolFilter::Static(discovery)) =
            (self.options.tool_filter(), filter)
        else {
            return false;
        };
        self.tools.iter().any(|cached| {
            tool_is_model_visible(&cached.tool)
                && server.allows(&cached.tool.name)
                && discovery.allows(&cached.tool.name)
        })
    }
}

/// A prepared server catalog, invocation binding and host discovery filter.
pub type McpToolCatalogSource = (McpToolCatalog, Arc<dyn McpToolClientSource>, ToolFilter);

/// Allocates names jointly after hosts collect available catalogs. Duplicate-name/configuration
/// errors remain global; server listing failures must be handled before entering this function.
pub async fn discover_catalog_tools(
    catalogs: &[McpToolCatalogSource],
    options: &McpDiscoveryOptions,
    reserved: &BTreeSet<String>,
    run_context: Option<&RunContext>,
    agent: Option<&RunAgent>,
) -> Result<Vec<Arc<McpTool>>> {
    let mut infos = Vec::new();
    let mut names = HashSet::new();
    for (index, (catalog, _, filter)) in catalogs.iter().enumerate() {
        if options.naming == McpToolNaming::Namespaced && !names.insert(&catalog.name) {
            return Err(Error::config("duplicate MCP server name"));
        }
        let tools = catalog
            .clone()
            .filtered(filter, run_context, agent)
            .await?
            .tools;
        infos.extend(tools.into_iter().map(|cached| {
            McpToolInfo::from_cached(&catalog.name, cached)
                .with_server_index(index)
                .with_namespace_description(catalog.instructions.clone())
        }));
    }
    let reserved = options
        .reserved_tool_names
        .iter()
        .chain(reserved)
        .cloned()
        .collect();
    let infos = match options.naming {
        McpToolNaming::Original => name_tools_for_function_calling(infos, false, &reserved)?,
        McpToolNaming::ServerPrefixed => name_tools_for_function_calling(infos, true, &reserved)?,
        McpToolNaming::Namespaced => normalize_tools_for_model_with_prefix(infos, true, &[]),
    };
    Ok(infos
        .into_iter()
        .filter_map(|info| {
            let (catalog, source, _) = &catalogs[info.server_index];
            let name = info.tool.name.to_string();
            McpTool::new(
                info,
                &catalog.options,
                source.clone(),
                options.tool_search_enabled,
                agent,
            )
            .map(Arc::new)
            .inspect_err(|error| warn!("skipping MCP tool {name:?}: {error}"))
            .ok()
        })
        .collect())
}
