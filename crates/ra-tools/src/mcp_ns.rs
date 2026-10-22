// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The three independent MCP resource tools, preserving Codex's names, schemas and payloads.
//!
//! Tools route each request through a client source, like discovered MCP function tools: a source
//! is either an initialized session or a host binding that waits for a starting connection, as
//! Codex's resource handlers wait for the connection manager. The host owns startup, reconnect and
//! cleanup. Resource URIs are opaque MCP addresses, never host file paths or HTTP requests.
//!
//! All-server listings exhaust pagination and isolate server failures. Aggregation uses Codex's
//! catalog bounds with explicit host overrides; single-server requests return one page. Each
//! request uses the server's tool-call deadline, as Codex applies its per-server tool timeout.
//! Output projection belongs to the runner, preserving complete observations instead of
//! truncating data.

use std::{collections::BTreeMap, future::Future, marker::PhantomData, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    tool::{
        Tool, ToolConcurrency, ToolContext, ToolFailureHandling, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_mcp::{
    client::McpClient,
    rmcp::model::{ReadResourceResult, Resource, ResourceTemplate},
    tool::McpToolClientSource,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

type Servers = Arc<BTreeMap<String, Arc<dyn McpToolClientSource>>>;

/// Builds resource listing, template listing and reading tools over initialized MCP sessions.
///
/// Server names must be unique because requests address them by name. Empty snapshots yield
/// valid tools with empty all-server listings. This function starts no connection. Automatic
/// listings use Codex's catalog defaults, overridden by explicit catalog settings; single-server
/// requests return one page. Every request uses the client's tool-call deadline.
///
/// ```no_run
/// use std::sync::Arc;
/// use ra_core::agent::{AgentId, AgentSpec};
/// use ra_mcp::client::McpClient;
///
/// fn resource_agent(clients: Vec<McpClient>) -> ra_core::error::Result<Arc<AgentSpec>> {
///     AgentSpec::builder()
///         .id(AgentId::new("resources"))
///         .name("Resources")
///         .tools(ra_tools::mcp_ns::resource_tools(clients)?)
///         .build()
/// }
/// ```
pub fn resource_tools(clients: Vec<McpClient>) -> Result<Vec<Arc<dyn Tool>>> {
    resource_tools_from_sources(
        clients
            .into_iter()
            .map(|client| {
                (
                    client.server_name().to_owned(),
                    Arc::new(client) as Arc<dyn McpToolClientSource>,
                )
            })
            .collect(),
    )
}

/// Builds the resource tools over named client sources resolved when each request runs.
///
/// A host binding may wait for a starting connection. An all-server listing skips a server whose
/// source fails, as Codex skips a connection whose client cannot be obtained; a single-server
/// request reports that failure to the model. Server names must be unique.
pub fn resource_tools_from_sources(
    servers: Vec<(String, Arc<dyn McpToolClientSource>)>,
) -> Result<Vec<Arc<dyn Tool>>> {
    let mut by_name = BTreeMap::new();
    for (name, source) in servers {
        if by_name.insert(name, source).is_some() {
            return Err(Error::config("MCP resource server names must be unique"));
        }
    }
    let servers: Servers = Arc::new(by_name);
    Ok(vec![
        Arc::new(ListTool::<Resources>::new(servers.clone())?) as Arc<dyn Tool>,
        Arc::new(ListTool::<Templates>::new(servers.clone())?),
        Arc::new(ReadTool::new(servers)?),
    ])
}

// These descriptions and parameter schemas are ported verbatim from mcp_resource_spec.rs.
const READ_NAME: &str = "read_mcp_resource";
const READ_METHOD: &str = "resources/read";
const READ_DESCRIPTION: &str =
    "Read a specific resource from an MCP server given the server name and resource URI.";
const READ_SERVER_DESCRIPTION: &str = "MCP server name exactly as configured. Must match the 'server' field returned by list_mcp_resources.";
const READ_URI_DESCRIPTION: &str =
    "Resource URI to read. Must be one of the URIs returned by list_mcp_resources.";

/// One listing operation: its wire method, tool contract and payload shape.
trait Listing: Send + Sync + 'static {
    type Item: Serialize + Send + 'static;
    const NAME: &'static str;
    const METHOD: &'static str;
    const DESCRIPTION: &'static str;
    const SERVER_DESCRIPTION: &'static str;
    const CURSOR_DESCRIPTION: &'static str;

    fn page(
        client: &McpClient,
        cursor: Option<String>,
    ) -> impl Future<Output = Result<(Vec<Self::Item>, Option<String>)>> + Send;
    fn all(client: &McpClient) -> impl Future<Output = Result<Vec<Self::Item>>> + Send;
    fn payload(
        server: Option<String>,
        items: Vec<ResourceWithServer<Self::Item>>,
        next_cursor: Option<String>,
    ) -> Result<String>;
}

struct Resources;
struct Templates;

impl Listing for Resources {
    type Item = Resource;
    const NAME: &'static str = "list_mcp_resources";
    const METHOD: &'static str = "resources/list";
    const DESCRIPTION: &'static str = "Lists resources provided by MCP servers. Resources allow servers to share data that provides context to language models, such as files, database schemas, or application-specific information. Prefer resources over web search when possible.";
    const SERVER_DESCRIPTION: &'static str =
        "MCP server name. Omit to list resources from every configured server.";
    const CURSOR_DESCRIPTION: &'static str =
        "Opaque cursor from a previous list_mcp_resources call; omit for the first page.";

    async fn page(
        client: &McpClient,
        cursor: Option<String>,
    ) -> Result<(Vec<Resource>, Option<String>)> {
        let page = client
            .list_resources_with_timeout(cursor, client.options().tool_timeout())
            .await?;
        Ok((page.resources, page.next_cursor))
    }
    async fn all(client: &McpClient) -> Result<Vec<Resource>> {
        client.list_all_resources().await
    }
    fn payload(
        server: Option<String>,
        resources: Vec<ResourceWithServer<Resource>>,
        next_cursor: Option<String>,
    ) -> Result<String> {
        encode(
            Self::NAME,
            &ListResourcesPayload {
                server,
                resources,
                next_cursor,
            },
        )
    }
}

impl Listing for Templates {
    type Item = ResourceTemplate;
    const NAME: &'static str = "list_mcp_resource_templates";
    const METHOD: &'static str = "resources/templates/list";
    const DESCRIPTION: &'static str = "Lists resource templates provided by MCP servers. Parameterized resource templates allow servers to share data that takes parameters and provides context to language models, such as files, database schemas, or application-specific information. Prefer resource templates over web search when possible.";
    const SERVER_DESCRIPTION: &'static str =
        "MCP server name. Omit to list resource templates from every configured server.";
    const CURSOR_DESCRIPTION: &'static str =
        "Opaque cursor from a previous list_mcp_resource_templates call; omit for the first page.";

    async fn page(
        client: &McpClient,
        cursor: Option<String>,
    ) -> Result<(Vec<ResourceTemplate>, Option<String>)> {
        let page = client
            .list_resource_templates_with_timeout(cursor, client.options().tool_timeout())
            .await?;
        Ok((page.resource_templates, page.next_cursor))
    }
    async fn all(client: &McpClient) -> Result<Vec<ResourceTemplate>> {
        client.list_all_resource_templates().await
    }
    fn payload(
        server: Option<String>,
        resource_templates: Vec<ResourceWithServer<ResourceTemplate>>,
        next_cursor: Option<String>,
    ) -> Result<String> {
        encode(
            Self::NAME,
            &ListResourceTemplatesPayload {
                server,
                resource_templates,
                next_cursor,
            },
        )
    }
}

// The payload types below keep Codex's field order: `server` precedes the flattened MCP fields.
#[derive(Serialize)]
struct ResourceWithServer<T> {
    server: String,
    #[serde(flatten)]
    resource: T,
}

impl<T> ResourceWithServer<T> {
    fn from_server(server: &str, resources: Vec<T>) -> Vec<Self> {
        resources
            .into_iter()
            .map(|resource| Self {
                server: server.to_owned(),
                resource,
            })
            .collect()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResourcesPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    resources: Vec<ResourceWithServer<Resource>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResourceTemplatesPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    resource_templates: Vec<ResourceWithServer<ResourceTemplate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct ReadResourcePayload {
    server: String,
    uri: String,
    #[serde(flatten)]
    result: ReadResourceResult,
}

#[derive(Default, Deserialize)]
struct ListArgs {
    server: Option<String>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
struct ReadArgs {
    server: String,
    uri: String,
}

struct ListTool<L> {
    origin: ToolOrigin,
    schema: ToolSchema,
    servers: Servers,
    listing: PhantomData<fn() -> L>,
}

impl<L: Listing> ListTool<L> {
    fn new(servers: Servers) -> Result<Self> {
        let parameters = json!({"type":"object", "properties": {
            "server":{"type":"string", "description":L::SERVER_DESCRIPTION},
            "cursor":{"type":"string", "description":L::CURSOR_DESCRIPTION}
        }, "additionalProperties":false});
        Ok(Self {
            origin: ToolOrigin::new(L::NAME)?,
            schema: ToolSchema::loose(L::NAME, parameters)?.with_description(L::DESCRIPTION),
            servers,
            listing: PhantomData,
        })
    }

    async fn list(&self, arguments: &Value) -> Result<String> {
        let args: ListArgs = if arguments.is_null() {
            ListArgs::default()
        } else {
            parse(L::NAME, arguments)?
        };
        let server = normalize(args.server);
        let cursor = normalize(args.cursor);
        if let Some(server) = server {
            let client = client(&self.servers, &server).await?;
            let (items, next_cursor) = L::page(&client, cursor).await.map_err(|error| {
                error.with_context(format!("{} failed for `{server}`", L::METHOD))
            })?;
            let items = ResourceWithServer::from_server(&server, items);
            return L::payload(Some(server), items, next_cursor);
        }
        if cursor.is_some() {
            return Err(invalid(
                L::NAME,
                "cursor can only be used when a server is specified",
            ));
        }
        let mut pending = tokio::task::JoinSet::new();
        for (name, source) in self.servers.iter() {
            let name = name.clone();
            let source = source.clone();
            pending.spawn(async move {
                let items = match source.client().await {
                    Ok(client) => L::all(&client).await,
                    Err(error) => Err(error),
                };
                (name, items)
            });
        }
        let mut by_server = BTreeMap::new();
        while let Some(result) = pending.join_next().await {
            match result {
                Ok((server, Ok(items))) => {
                    by_server.insert(server, items);
                }
                Ok((server, Err(error))) => {
                    tracing::warn!(
                        server = %ra_mcp::catalog::safe_server_name(&server),
                        method = L::METHOD,
                        code = error.code(),
                        "Failed to list MCP resources for a server"
                    );
                }
                Err(_) => {
                    tracing::warn!(method = L::METHOD, "Task panic when listing MCP resources");
                }
            }
        }
        let items = by_server
            .into_iter()
            .flat_map(|(server, items)| ResourceWithServer::from_server(&server, items))
            .collect();
        L::payload(None, items, None)
    }
}

struct ReadTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    servers: Servers,
}

impl ReadTool {
    fn new(servers: Servers) -> Result<Self> {
        let parameters = json!({"type":"object", "properties": {
            "server":{"type":"string", "description":READ_SERVER_DESCRIPTION},
            "uri":{"type":"string", "description":READ_URI_DESCRIPTION}
        }, "required":["server","uri"], "additionalProperties":false});
        Ok(Self {
            origin: ToolOrigin::new(READ_NAME)?,
            schema: ToolSchema::loose(READ_NAME, parameters)?.with_description(READ_DESCRIPTION),
            servers,
        })
    }

    async fn read(&self, arguments: &Value) -> Result<String> {
        if arguments.is_null() {
            return Err(invalid(
                READ_NAME,
                "failed to parse function arguments: expected value",
            ));
        }
        let args: ReadArgs = parse(READ_NAME, arguments)?;
        let required = |field, value| {
            normalize(Some(value))
                .ok_or_else(|| invalid(READ_NAME, format!("{field} must be provided")))
        };
        let server = required("server", args.server)?;
        let uri = required("uri", args.uri)?;
        let client = client(&self.servers, &server).await?;
        let result = client
            .read_resource_with_timeout(&uri, client.options().tool_timeout())
            .await
            .map_err(|error| {
                error.with_context(format!("{READ_METHOD} failed for `{server}` ({uri})"))
            })?;
        encode(
            READ_NAME,
            &ReadResourcePayload {
                server,
                uri,
                result,
            },
        )
    }
}

/// Model-correctable argument errors, returned verbatim as Codex's `RespondToModel` messages.
fn invalid(tool: &str, message: impl Into<String>) -> Error {
    Error::tool(ToolErrorKind::InvalidInput, tool, message)
}

fn parse<T: serde::de::DeserializeOwned>(tool: &str, arguments: &Value) -> Result<T> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| invalid(tool, format!("failed to parse function arguments: {error}")))
}

async fn client(servers: &Servers, server: &str) -> Result<McpClient> {
    let source = servers.get(server).ok_or_else(|| {
        Error::tool(
            ToolErrorKind::InvalidInput,
            "mcp",
            format!("unknown MCP server '{server}'"),
        )
    })?;
    source
        .client()
        .await
        .map_err(|error| error.with_context("failed to get client"))
}

fn normalize(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn encode(tool: &str, payload: &impl Serialize) -> Result<String> {
    serde_json::to_string(payload).map_err(|error| {
        Error::tool(
            ToolErrorKind::ExecutionFailed,
            tool,
            format!("failed to serialize MCP resource response: {error}"),
        )
    })
}

fn options() -> ToolOptions {
    ToolOptions::new()
        .with_action_classes([crate::action_classes::READ])
        .with_concurrency(ToolConcurrency::Parallel)
        .with_failure_handling(ToolFailureHandling::Custom)
}

/// Renders a failure as Codex's handlers do: argument errors raised by the tool itself are
/// returned verbatim, while lookup and remote failures carry the `{method} failed:` prefix.
/// Messages come from the framework error's model-safe text, never its host-facing display.
fn failure(tool: &str, method: &str, error: &Error) -> Option<ToolOutput> {
    if error.is_control_signal() {
        return None;
    }
    let message = match error {
        Error::Tool {
            tool: source,
            message,
            ..
        } if source == tool => return Some(ToolOutput::text(message.clone())),
        Error::Tool { message, .. }
        | Error::Config { message, .. }
        | Error::Protocol { message, .. }
        | Error::Session { message, .. } => message.as_str(),
        _ => "MCP resource operation failed",
    };
    Some(ToolOutput::text(format!("{method} failed: {message}")))
}

#[async_trait]
impl<L: Listing> Tool for ListTool<L> {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        options()
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text(self.list(context.arguments()).await?))
    }
    async fn handle_failure(
        &self,
        _: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        Ok(failure(L::NAME, L::METHOD, error))
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        options()
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text(self.read(context.arguments()).await?))
    }
    async fn handle_failure(
        &self,
        _: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        Ok(failure(READ_NAME, READ_METHOD, error))
    }
}
