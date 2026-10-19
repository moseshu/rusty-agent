// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! MCP declarations and content adapted to the provider-neutral Tool contract.

use crate::{
    catalog::{
        McpToolInfo, name_tools_for_function_calling, normalize_tools_for_model_with_prefix,
    },
    client::{McpClient, McpClientOptions},
    filter::{ToolFilter, tool_is_model_visible},
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ra_core::{
    error::{Error, Result},
    item::{Base64FileSource, FileBlock, FileSource, ImageBlock, ImageSource},
    tool::{
        Tool, ToolConcurrency, ToolContext, ToolExposure, ToolFailureHandling, ToolNamespace,
        ToolOptions, ToolOrigin, ToolOutput, ToolOutputBlock, ToolSchema,
    },
};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};
use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};
use tracing::warn;

mod prepared;
pub use prepared::{
    McpToolCatalog, McpToolCatalogSource, McpToolClientSource, discover_catalog_tools,
};

/// How discovered tools are named for the model.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum McpToolNaming {
    /// Python's default: bare original MCP names; a name repeated across servers is an error.
    #[default]
    Original,
    /// Python's `include_server_in_tool_names`: bare `mcp_{server}__{tool}` names of at most
    /// 64 bytes, hashed on collisions.
    ServerPrefixed,
    /// Codex's `mcp__{server}` namespaces with the server instructions as description. This
    /// extension needs an endpoint that carries tool namespaces (`OpenAI` Responses); other model
    /// adapters reject namespaced tools.
    Namespaced,
}

/// Options for one joint discovery over a set of servers.
#[derive(Debug, Default, Clone)]
pub struct McpDiscoveryOptions {
    naming: McpToolNaming,
    tool_search_enabled: bool,
    reserved_tool_names: BTreeSet<String>,
}
impl McpDiscoveryOptions {
    /// Python's defaults: original names and direct exposure.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Selects the naming contract.
    #[must_use]
    pub const fn with_naming(mut self, naming: McpToolNaming) -> Self {
        self.naming = naming;
        self
    }
    /// Defers visible tools to tool search, as Codex does when search is enabled.
    #[must_use]
    pub const fn with_tool_search(mut self, enabled: bool) -> Self {
        self.tool_search_enabled = enabled;
        self
    }
    /// Names already used by the agent's other tools and handoffs. Prefixed names avoid them,
    /// as Python's `reserved_tool_names`; the other naming contracts do not read them.
    #[must_use]
    pub fn with_reserved_tool_names(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.reserved_tool_names = names.into_iter().collect();
        self
    }
    pub(crate) const fn reserved_tool_names(&self) -> &BTreeSet<String> {
        &self.reserved_tool_names
    }
    pub(crate) const fn requires_reserved_tool_names(&self) -> bool {
        matches!(self.naming, McpToolNaming::ServerPrefixed)
    }
}

/// Immutable metadata and a fixed or explicitly deferred connection binding.
pub struct McpTool {
    info: McpToolInfo,
    client_source: Arc<dyn McpToolClientSource>,
    approval: crate::approval::RequireApprovalSetting,
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    approval_agent: Option<ra_core::context::RunAgent>,
}
impl McpTool {
    fn new(
        info: McpToolInfo,
        policy: &McpClientOptions,
        client_source: Arc<dyn McpToolClientSource>,
        tool_search_enabled: bool,
        agent: Option<&ra_core::context::RunAgent>,
    ) -> Result<Self> {
        let origin = match info.callable_namespace() {
            Some(namespace) => {
                ToolOrigin::namespaced(ToolNamespace::new(namespace)?, info.callable_name())?
            }
            None => ToolOrigin::new(info.callable_name())?,
        };
        // MCP does not require `properties`, but OpenAI function schemas do. Python inserts an
        // empty object when it is absent; Codex also replaces null.
        let mut input_schema = info.tool.input_schema.as_ref().clone();
        if input_schema
            .get("properties")
            .is_none_or(serde_json::Value::is_null)
        {
            input_schema.insert(
                "properties".to_owned(),
                serde_json::Value::Object(serde_json::Map::new()),
            );
        }
        let mut schema = ToolSchema::loose(
            info.callable_name(),
            serde_json::Value::Object(input_schema),
        )?
        .with_description(info.description_for_model());
        if let Some(description) = info.namespace_description() {
            schema = schema.with_namespace_description(description);
        }
        let exposure = if !tool_is_model_visible(&info.tool) {
            ToolExposure::Hidden
        } else if tool_search_enabled {
            ToolExposure::Deferred
        } else {
            ToolExposure::Advertised
        };
        let concurrency = if info
            .tool
            .annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
            == Some(true)
        {
            ToolConcurrency::Parallel
        } else {
            ToolConcurrency::Exclusive
        };
        let mut options = ToolOptions::new()
            .with_approval(
                policy
                    .require_approval()
                    .for_tool(&info.tool.name, agent.is_some()),
            )
            .with_exposure(exposure)
            .with_concurrency(concurrency)
            .with_failure_handling(ToolFailureHandling::ModelVisible);
        for guardrail in policy.tool_input_guardrails() {
            options = options.with_input_guardrail(guardrail.clone());
        }
        for guardrail in policy.tool_output_guardrails() {
            options = options.with_output_guardrail(guardrail.clone());
        }
        // The MCP client owns the deadline so it can send the protocol cancellation before
        // returning. A competing runner timer could drop it before that notification is sent.
        Ok(Self {
            info,
            client_source,
            approval: policy.require_approval().clone(),
            origin,
            schema,
            options,
            approval_agent: agent.cloned(),
        })
    }
    /// Original MCP identity and normalized model names.
    #[must_use]
    pub const fn info(&self) -> &McpToolInfo {
        &self.info
    }
    /// Calls the original tool name on this declaration's fixed session or deferred startup binding.
    /// The declaration's `_meta` is sent as request metadata, as Python's static tool meta is.
    /// This is a raw protocol call; local approval is enforced by runtime dispatch through [`Tool`].
    pub async fn invoke(&self, arguments: serde_json::Value) -> Result<ToolOutput> {
        let meta = self.info.tool.meta.as_ref().map(|meta| meta.0.clone());
        map_tool_result(
            self.client_source
                .client()
                .await?
                .call_tool(&self.info.tool.name, arguments, meta)
                .await?,
        )
    }
}
#[async_trait]
impl Tool for McpTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn recipient_binding(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!([
            crate::catalog::safe_server_name(self.info.server_name()),
            self.info.tool.name.as_ref(),
            self.info.server_index
        ]))
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn title(&self) -> Option<&str> {
        self.info.title()
    }
    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        match self.options.approval() {
            ra_core::tool::ToolApprovalPolicy::Never => Ok(false),
            ra_core::tool::ToolApprovalPolicy::Always => Ok(true),
            ra_core::tool::ToolApprovalPolicy::Dynamic => {
                if let (crate::approval::RequireApprovalSetting::Dynamic(policy), Some(agent)) =
                    (&self.approval, self.approval_agent.as_ref())
                {
                    policy(context.run(), agent, self.info.tool.clone()).await
                } else {
                    Err(Error::config(
                        "dynamic MCP approval requires a policy and agent",
                    ))
                }
            }
            _ => Err(Error::config("unsupported MCP tool approval policy")),
        }
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.invoke(context.arguments().clone()).await
    }
}

/// Discovers one joint catalog so name collisions across servers are resolved together.
/// Filters use original names; tools retain their exact client even after a later discovery.
/// A tool whose declaration cannot be adapted is skipped with a warning, as Codex does.
///
/// # Errors
///
/// Returns listing failures, Python's duplicate-name error for [`McpToolNaming::Original`], and
/// a configuration error for repeated server names under [`McpToolNaming::Namespaced`], whose
/// identities are keyed by server name.
pub async fn discover_tools(
    servers: &[(McpClient, ToolFilter)],
    options: &McpDiscoveryOptions,
) -> Result<Vec<Arc<McpTool>>> {
    discover_tools_with_context(servers, options, None, None).await
}

/// Discovers the joint catalog with the run and agent needed by dynamic server or discovery filters.
pub async fn discover_tools_with_context(
    servers: &[(McpClient, ToolFilter)],
    options: &McpDiscoveryOptions,
    run_context: Option<&ra_core::context::RunContext>,
    agent: Option<&ra_core::context::RunAgent>,
) -> Result<Vec<Arc<McpTool>>> {
    let mut catalogs = Vec::new();
    let mut server_names = HashSet::new();
    for (client, filter) in servers {
        if options.naming == McpToolNaming::Namespaced
            && !server_names.insert(client.server_name().to_owned())
        {
            return Err(Error::config("duplicate MCP server name"));
        }
        catalogs.push((
            McpToolCatalog::fetch(client)
                .await?
                .filtered(filter, run_context, agent)
                .await?,
            Arc::new(client.clone()) as Arc<dyn McpToolClientSource>,
            ToolFilter::default(),
        ));
    }
    discover_catalog_tools(&catalogs, options, &BTreeSet::new(), run_context, agent).await
}

/// Maps protocol content in order. Like Python's default `use_structured_content=false`,
/// content wins over structuredContent, including when isError=true. Such results remain
/// model-visible tool outputs; protocol/transport failures use the Tool failure policy.
/// Embedded resources become files instead of Python's JSON text fallback, preserving bytes;
/// resource links and any other block fall back to their JSON serialization as text, as in
/// Python. A resource link identifies a resource and does not supply downloadable file bytes.
///
/// # Errors
///
/// Returns an error only when a fallback block cannot be serialized.
pub fn map_tool_result(result: CallToolResult) -> Result<ToolOutput> {
    let mut blocks = Vec::new();
    for content in result.content {
        blocks.push(match content {
            ContentBlock::Text(text) => ToolOutputBlock::text(text.text),
            ContentBlock::Image(image) => ToolOutputBlock::Image(ImageBlock::new(
                ImageSource::base64(image.mime_type, image.data),
            )),
            ContentBlock::Audio(audio) => {
                ToolOutputBlock::audio(format!("data:{};base64,{}", audio.mime_type, audio.data))
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    ToolOutputBlock::File(FileBlock::new(FileSource::Base64(
                        Base64FileSource::new(STANDARD.encode(text.as_bytes()))
                            .with_filename(uri.clone()),
                    )))
                }
                ResourceContents::BlobResourceContents { uri, blob, .. } => {
                    ToolOutputBlock::File(FileBlock::new(FileSource::Base64(
                        Base64FileSource::new(blob.clone()).with_filename(uri.clone()),
                    )))
                }
                _ => json_text_block(&ContentBlock::Resource(resource.clone()))?,
            },
            other => json_text_block(&other)?,
        });
    }
    // The neutral result contract requires one block even when MCP legitimately returns [].
    if blocks.is_empty() {
        blocks.push(ToolOutputBlock::text(""));
    }
    ToolOutput::new(blocks)
}

// Python's fallback for content it does not map: the block's JSON, never a language repr.
fn json_text_block(content: &ContentBlock) -> Result<ToolOutputBlock> {
    serde_json::to_string(content)
        .map(ToolOutputBlock::text)
        .map_err(|_| Error::config("MCP content block could not be serialized"))
}
