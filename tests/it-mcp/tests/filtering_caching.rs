//! Filtering and cache contracts ported from the Python SDK, over real SDK sessions.

#[path = "../support/mod.rs"]
mod support;

use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::Result,
    state::RunId,
};
use ra_mcp::{
    McpServer,
    client::{
        McpClient, McpClientOptions,
        stdio::{McpServerStdio, McpStdioParams},
    },
    filter::{ToolFilter, create_static_tool_filter},
    in_process::InProcessToolServer,
    tool::{McpDiscoveryOptions, McpToolNaming, discover_tools, discover_tools_with_context},
};
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::json;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

struct ListRequest {
    cursor: Option<String>,
    reply: oneshot::Sender<std::result::Result<ListToolsResult, ErrorData>>,
}
struct CatalogHandler {
    requests: mpsc::UnboundedSender<ListRequest>,
    calls: Arc<AtomicUsize>,
}
impl ServerHandler for CatalogHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        let (reply, receive) = oneshot::channel();
        self.requests
            .send(ListRequest {
                cursor: request.and_then(|params| params.cursor),
                reply,
            })
            .unwrap();
        tokio::select! {
            result = receive => result.unwrap_or_else(|_| Err(ErrorData::internal_error("fixture reply dropped", None))),
            () = context.ct.cancelled() => Err(ErrorData::internal_error("cancelled", None)),
        }
    }
    async fn call_tool(
        &self,
        _: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into())
    }
}
struct Harness {
    server: InProcessToolServer,
    requests: mpsc::UnboundedReceiver<ListRequest>,
    calls: Arc<AtomicUsize>,
}
impl Harness {
    async fn new(options: McpClientOptions) -> Self {
        let (send, requests) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let server = InProcessToolServer::connect(
            "catalog",
            CatalogHandler {
                requests: send,
                calls: calls.clone(),
            },
            options,
        )
        .await
        .unwrap();
        Self {
            server,
            requests,
            calls,
        }
    }
    fn client(&self) -> McpClient {
        self.server.client()
    }
    async fn request(&mut self) -> ListRequest {
        tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn respond(&mut self, tools: Vec<Tool>, next_cursor: Option<&str>) {
        let request = self.request().await;
        let result = ListToolsResult {
            tools,
            next_cursor: next_cursor.map(str::to_owned),
            ..Default::default()
        };
        request.reply.send(Ok(result)).unwrap();
    }
    async fn list(&mut self, tools: Vec<Tool>) -> Vec<Tool> {
        let client = self.client();
        let (result, ()) = tokio::join!(listed(&client), self.respond(tools, None));
        result.unwrap()
    }
    async fn list_with_context(
        &mut self,
        context: &RunContext,
        _agent: &AgentSpec,
        tools: Vec<Tool>,
    ) -> Vec<Tool> {
        let client = self.client();
        let (result, ()) = tokio::join!(
            client.list_tools_with_context(Some(context), Some(context.agent())),
            self.respond(tools, None)
        );
        result.unwrap()
    }
}
async fn listed(client: &McpClient) -> Result<Vec<Tool>> {
    tokio::time::timeout(Duration::from_secs(2), client.list_tools())
        .await
        .unwrap()
}
fn tool(name: &str, description: &str, required: &[&str]) -> Tool {
    serde_json::from_value(json!({"name":name,"description":description,"inputSchema":{
        "type":"object","properties":{"query":{"type":"string"}},"required":required
    }}))
    .unwrap()
}
fn agent(name: &str) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(name))
        .name(name)
        .build()
        .unwrap()
}
fn context(agent: &AgentSpec, admin: bool) -> RunContext {
    RunContext::new(RunId::new("filter-run"), agent).with_app_context(Arc::new(admin))
}
fn names(tools: &[Tool]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_ref()).collect()
}
fn caching() -> McpClientOptions {
    McpClientOptions::default().with_cache_tools_list(true)
}

#[tokio::test]
async fn caching_defaults_off_but_keeps_the_last_complete_snapshot() {
    let mut harness = Harness::new(McpClientOptions::default()).await;
    let client = harness.client();
    assert!(client.cached_tools().is_none());
    harness.list(vec![tool("read", "first", &[])]).await;
    let second = harness.list(vec![tool("read", "second", &["query"])]).await;
    assert_eq!(second, client.cached_tools().unwrap());
    assert_eq!(second[0].description.as_deref(), Some("second"));
    assert!(client.call_tool("read", None, None).await.is_err());
    assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn empty_cache_remains_a_snapshot_but_is_fetched_again() {
    let mut harness = Harness::new(caching()).await;
    harness.list(vec![]).await;
    assert_eq!(harness.client().cached_tools(), Some(vec![]));
    let tools = harness.list(vec![tool("new", "", &[])]).await;
    assert_eq!(names(&tools), ["new"]);
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn paginated_catalog_is_cached_before_static_filtering_and_reused() {
    let filter = create_static_tool_filter(Some(vec!["second".into()]), None).unwrap();
    let mut harness = Harness::new(caching().with_tool_filter(filter)).await;
    let client = harness.client();
    let task_client = client.clone();
    let task = tokio::spawn(async move { listed(&task_client).await });
    let first = tool("first", "", &[]);
    let second = tool("second", "", &[]);
    harness.respond(vec![first.clone()], Some("")).await;
    let request = harness.request().await;
    assert_eq!(request.cursor.as_deref(), Some(""));
    assert!(client.cached_tools().is_none());
    let page = ListToolsResult {
        tools: vec![second.clone()],
        ..Default::default()
    };
    request.reply.send(Ok(page)).unwrap();
    assert_eq!(task.await.unwrap().unwrap(), std::slice::from_ref(&second));
    assert_eq!(
        listed(&client).await.unwrap(),
        std::slice::from_ref(&second)
    );
    assert_eq!(client.cached_tools().unwrap(), [first, second]);
    assert!(harness.requests.try_recv().is_err());
    client.invalidate_tools_cache();
    assert_eq!(client.cached_tools().unwrap().len(), 2);
    assert_eq!(
        names(&harness.list(vec![tool("second", "updated", &[])]).await),
        ["second"]
    );
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn static_allow_block_and_empty_lists_match_python() {
    assert!(create_static_tool_filter(None, None).is_none());
    for (allowed, blocked, expected) in [
        (None, None, vec!["read", "write", "admin"]),
        (Some(vec![]), None, vec![]),
        (None, Some(vec![]), vec!["read", "write", "admin"]),
        (
            Some(vec!["write", "read"]),
            Some(vec!["write"]),
            vec!["read"],
        ),
        (None, Some(vec!["admin"]), vec!["read", "write"]),
    ] {
        let filter = create_static_tool_filter(
            allowed.map(|names| names.into_iter().map(str::to_owned).collect()),
            blocked.map(|names| names.into_iter().map(str::to_owned).collect()),
        )
        .unwrap_or_default();
        let mut harness = Harness::new(caching().with_tool_filter(filter)).await;
        let tools = harness
            .list(vec![
                tool("read", "", &[]),
                tool("write", "", &[]),
                tool("admin", "", &[]),
            ])
            .await;
        assert_eq!(names(&tools), expected);
        harness.server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn live_filter_and_cache_settings_reuse_the_unfiltered_snapshot() {
    let mut harness = Harness::new(caching()).await;
    harness
        .list(vec![tool("read", "", &[]), tool("write", "", &[])])
        .await;
    let client = harness.client();
    client.set_tool_filter(create_static_tool_filter(Some(vec!["read".into()]), None).unwrap());
    assert_eq!(names(&listed(&client).await.unwrap()), ["read"]);
    client.set_tool_filter(ToolFilter::dynamic_sync(|_, tool| Ok(tool.name == "write")));
    let agent = agent("regular");
    let context = context(&agent, false);
    assert_eq!(
        names(
            &client
                .list_tools_with_context(Some(&context), Some(context.agent()))
                .await
                .unwrap()
        ),
        ["write"]
    );
    client.set_tool_filter(ToolFilter::default());
    assert_eq!(names(&listed(&client).await.unwrap()), ["read", "write"]);
    assert_eq!(client.cached_tools().unwrap().len(), 2);
    assert!(harness.requests.try_recv().is_err());
    client.set_cache_tools_list(false);
    harness.list(vec![tool("read", "updated", &[])]).await;
    client.set_cache_tools_list(true);
    assert_eq!(
        listed(&client).await.unwrap()[0].description.as_deref(),
        Some("updated")
    );
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn sync_and_async_filters_see_run_agent_and_original_server_on_each_cache_hit() {
    for asynchronous in [false, true] {
        let evaluations = Arc::new(AtomicUsize::new(0));
        let count = evaluations.clone();
        let predicate = move |context: ra_mcp::filter::ToolFilterContext<'_>, tool: Tool| {
            count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(context.server_name(), "catalog");
            Ok(*context.run_context().app_context::<bool>().unwrap()
                || context.agent().name() == "admin"
                || tool.name == "read")
        };
        let filter = if asynchronous {
            ToolFilter::dynamic(move |context, tool| {
                let result = predicate(context, tool);
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    result
                })
            })
        } else {
            ToolFilter::dynamic_sync(predicate)
        };
        let mut harness = Harness::new(caching().with_tool_filter(filter)).await;
        let regular = agent("regular");
        let admin = agent("admin");
        let regular_context = context(&regular, false);
        let admin_context = context(&regular, true);
        let first = harness
            .list_with_context(
                &regular_context,
                &regular,
                vec![tool("read", "", &[]), tool("write", "", &[])],
            )
            .await;
        assert_eq!(names(&first), ["read"]);
        let client = harness.client();
        for (run_context, agent) in [(&regular_context, &admin), (&admin_context, &regular)] {
            let agent_context = RunContext::new(RunId::new("filter-agent"), agent);
            let tools = client
                .list_tools_with_context(Some(run_context), Some(agent_context.agent()))
                .await
                .unwrap();
            assert_eq!(names(&tools), ["read", "write"]);
        }
        assert_eq!(evaluations.load(Ordering::SeqCst), 6);
        assert_eq!(names(&client.cached_tools().unwrap()), ["read", "write"]);
        assert!(harness.requests.try_recv().is_err());
        harness.server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn dynamic_filters_require_both_contexts_even_when_the_catalog_is_empty() {
    let mut harness =
        Harness::new(caching().with_tool_filter(ToolFilter::dynamic_sync(|_, _| Ok(true)))).await;
    let client = harness.client();
    let (result, ()) = tokio::join!(
        listed(&client),
        harness.respond(vec![tool("read", "", &[])], None)
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("run_context and agent are required")
    );
    assert_eq!(client.cached_tools().unwrap().len(), 1);
    let agent = agent("regular");
    let context = context(&agent, false);
    for (run_context, agent) in [(Some(&context), None), (None, Some(context.agent()))] {
        assert!(
            client
                .list_tools_with_context(run_context, agent)
                .await
                .is_err()
        );
    }
    client.invalidate_tools_cache();
    let (result, ()) = tokio::join!(listed(&client), harness.respond(vec![], None));
    assert!(result.is_err());
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn snapshots_and_mutating_filters_cannot_change_returned_tools_or_call_validation() {
    let filter = ToolFilter::dynamic_sync(|_, mut tool| {
        Arc::make_mut(&mut tool.input_schema).insert("required".into(), json!([]));
        tool.description = Some("mutated by filter".into());
        Ok(true)
    });
    let mut harness = Harness::new(caching().with_tool_filter(filter)).await;
    let agent = agent("regular");
    let context = context(&agent, false);
    let mut tools = harness
        .list_with_context(&context, &agent, vec![tool("read", "original", &["query"])])
        .await;
    assert_eq!(tools[0].description.as_deref(), Some("original"));
    assert_eq!(tools[0].input_schema["required"], json!(["query"]));
    Arc::make_mut(&mut tools[0].input_schema).insert("required".into(), json!([]));
    tools[0].name = "injected".into();
    let client = harness.client();
    let mut snapshot = client.cached_tools().unwrap();
    Arc::make_mut(&mut snapshot[0].input_schema).insert("required".into(), json!([]));
    snapshot.clear();
    let again = client
        .list_tools_with_context(Some(&context), Some(context.agent()))
        .await
        .unwrap();
    assert_eq!(names(&again), ["read"]);
    assert_eq!(again[0].input_schema["required"], json!(["query"]));
    assert_eq!(
        client.cached_tools().unwrap()[0].description.as_deref(),
        Some("original")
    );
    for arguments in [None, Some(json!({}))] {
        assert!(
            client
                .call_tool("read", arguments, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("missing required parameters: query")
        );
    }
    assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
    client
        .call_tool("read", json!({"query":null}), None)
        .await
        .unwrap();
    client.invalidate_tools_cache();
    client.call_tool("read", None, None).await.unwrap();
    assert_eq!(harness.calls.load(Ordering::SeqCst), 2);
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn invalidation_during_refresh_keeps_the_previous_snapshot_and_requires_another_refresh() {
    let mut harness = Harness::new(caching()).await;
    let initial = harness
        .list(vec![tool("read", "initial", &["query"])])
        .await;
    let client = harness.client();
    client.invalidate_tools_cache();
    let task_client = client.clone();
    let task = tokio::spawn(async move { listed(&task_client).await });
    let request = harness.request().await;
    client.invalidate_tools_cache();
    let page = ListToolsResult {
        tools: vec![tool("read", "stale", &[])],
        ..Default::default()
    };
    request.reply.send(Ok(page)).unwrap();
    assert_eq!(
        task.await.unwrap().unwrap()[0].description.as_deref(),
        Some("stale")
    );
    assert_eq!(client.cached_tools().unwrap(), initial);
    client.call_tool("read", None, None).await.unwrap();
    harness
        .list(vec![tool("read", "latest", &["latest"])])
        .await;
    assert!(
        client
            .call_tool("read", None, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("latest")
    );
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn concurrent_refreshes_publish_by_successful_start_order() {
    for newer_succeeds in [false, true] {
        let mut harness = Harness::new(caching()).await;
        let client = harness.client();
        let first_client = client.clone();
        let first = tokio::spawn(async move { listed(&first_client).await });
        let first_request = harness.request().await;
        let second_client = client.clone();
        let second = tokio::spawn(async move { listed(&second_client).await });
        let second_request = harness.request().await;
        if newer_succeeds {
            let page = ListToolsResult {
                tools: vec![tool("read", "second", &["latest"])],
                ..Default::default()
            };
            second_request.reply.send(Ok(page)).unwrap();
            second.await.unwrap().unwrap();
        } else {
            second_request
                .reply
                .send(Err(ErrorData::invalid_params("newer failed", None)))
                .unwrap();
            assert!(second.await.unwrap().is_err());
        }
        let page = ListToolsResult {
            tools: vec![tool("read", "first", &["old"])],
            ..Default::default()
        };
        first_request.reply.send(Ok(page)).unwrap();
        assert_eq!(
            first.await.unwrap().unwrap()[0].description.as_deref(),
            Some("first")
        );
        let expected = if newer_succeeds { "second" } else { "first" };
        assert_eq!(
            client.cached_tools().unwrap()[0].description.as_deref(),
            Some(expected)
        );
        assert_eq!(
            listed(&client).await.unwrap()[0].description.as_deref(),
            Some(expected)
        );
        harness.server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn failed_partial_refresh_never_publishes_and_remains_dirty() {
    for repeated_cursor in [false, true] {
        let mut harness = Harness::new(caching()).await;
        let initial = harness.list(vec![tool("read", "initial", &[])]).await;
        let client = harness.client();
        client.invalidate_tools_cache();
        let task_client = client.clone();
        let task = tokio::spawn(async move { listed(&task_client).await });
        harness
            .respond(vec![tool("partial", "", &[])], Some(""))
            .await;
        assert_eq!(client.cached_tools().unwrap(), initial);
        let request = harness.request().await;
        if repeated_cursor {
            let page = ListToolsResult {
                next_cursor: Some(String::new()),
                ..Default::default()
            };
            request.reply.send(Ok(page)).unwrap();
        } else {
            request
                .reply
                .send(Err(ErrorData::invalid_params("second page failed", None)))
                .unwrap();
        }
        assert!(task.await.unwrap().is_err());
        assert_eq!(client.cached_tools().unwrap(), initial);
        harness.list(vec![tool("read", "recovered", &[])]).await;
        assert_eq!(
            client.cached_tools().unwrap()[0].description.as_deref(),
            Some("recovered")
        );
        harness.server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn cancelled_refresh_does_not_publish_or_block_recovery() {
    let mut harness = Harness::new(caching()).await;
    let initial = harness.list(vec![tool("read", "initial", &[])]).await;
    let client = harness.client();
    client.invalidate_tools_cache();
    let task_client = client.clone();
    let task = tokio::spawn(async move { listed(&task_client).await });
    let request = harness.request().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(request);
    assert_eq!(client.cached_tools().unwrap(), initial);
    harness.list(vec![tool("read", "recovered", &[])]).await;
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn page_limit_failure_keeps_the_last_complete_snapshot() {
    let mut harness =
        Harness::new(caching().with_max_list_pages(std::num::NonZeroUsize::new(1))).await;
    let initial = harness.list(vec![tool("read", "initial", &[])]).await;
    let client = harness.client();
    client.invalidate_tools_cache();
    let (result, ()) = tokio::join!(
        listed(&client),
        harness.respond(vec![tool("partial", "", &[])], Some("next"))
    );
    assert!(result.unwrap_err().to_string().contains("page"));
    assert_eq!(client.cached_tools().unwrap(), initial);
    assert!(harness.requests.try_recv().is_err());
    harness.list(vec![tool("read", "recovered", &[])]).await;
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn cached_validation_uses_unfiltered_tools_and_only_string_required_names() {
    let filter = ToolFilter::new(Some(HashSet::new()), HashSet::new());
    let mut harness = Harness::new(caching().with_tool_filter(filter)).await;
    let mut declaration = tool("hidden", "", &[]);
    Arc::make_mut(&mut declaration.input_schema)
        .insert("required".into(), json!(["z", 5, "a", null]));
    assert!(harness.list(vec![declaration]).await.is_empty());
    let client = harness.client();
    assert!(
        client
            .call_tool("hidden", None, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("missing required parameters: a, z")
    );
    client
        .call_tool("hidden", json!({"a":null, "z":false}), None)
        .await
        .unwrap();
    client.call_tool("unlisted", None, None).await.unwrap();
    assert_eq!(harness.calls.load(Ordering::SeqCst), 2);
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn stdio_dynamic_filter_receives_context_through_the_server_trait() {
    let filter = ToolFilter::dynamic_sync(|context, tool| {
        assert_eq!(context.server_name(), "stdio: fixture");
        Ok(*context.run_context().app_context::<bool>().unwrap() || tool.name == "search")
    });
    let server = McpServerStdio::new(McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture")))
        .with_name("stdio: fixture")
        .with_options(caching().with_tool_filter(filter));
    let server: &dyn McpServer = &server;
    let agent = agent("regular");
    let regular = context(&agent, false);
    let admin = context(&agent, true);
    server.connect().await.unwrap();
    assert_eq!(
        names(
            &server
                .list_tools_with_context(Some(&regular), Some(regular.agent()))
                .await
                .unwrap()
        ),
        ["search"]
    );
    assert_eq!(
        server
            .list_tools_with_context(Some(&admin), Some(admin.agent()))
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(server.list_tools().await.is_err());
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn contextual_discovery_filters_original_names_before_model_name_allocation() {
    let filter = ToolFilter::dynamic(|context, tool| {
        Box::pin(async move {
            tokio::task::yield_now().await;
            assert_eq!(context.server_name(), "catalog");
            Ok(context.agent().name() == "regular" && tool.name == "read")
        })
    });
    let mut harness = Harness::new(caching()).await;
    harness
        .list(vec![tool("read", "", &[]), tool("write", "", &[])])
        .await;
    let agent = agent("regular");
    let context = context(&agent, false);
    let servers = [(harness.client(), filter)];
    let options = McpDiscoveryOptions::new().with_naming(McpToolNaming::ServerPrefixed);
    assert!(discover_tools(&servers, &options).await.is_err());
    let tools =
        discover_tools_with_context(&servers, &options, Some(&context), Some(context.agent()))
            .await
            .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].info().tool().name, "read");
    assert_eq!(tools[0].info().callable_name(), "mcp_catalog__read");
    assert_eq!(harness.client().cached_tools().unwrap().len(), 2);
    harness.server.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_stdio_retains_server_cache_across_cleanup_and_reconnect() {
    let server = McpServerStdio::new(McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture")))
        .with_options(caching());
    let server: &dyn McpServer = &server;
    assert!(server.cached_tools().is_none());
    assert!(server.list_tools().await.is_err());
    server.connect().await.unwrap();
    let original = server.list_tools().await.unwrap();
    assert_eq!(server.cached_tools().unwrap(), original);
    server.cleanup().await.unwrap();
    assert_eq!(server.cached_tools().unwrap(), original);
    assert!(server.list_tools().await.is_err());
    server.connect().await.unwrap();
    assert_eq!(server.list_tools().await.unwrap(), original);
    server.invalidate_tools_cache();
    assert_eq!(server.list_tools().await.unwrap(), original);
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_http_shares_cache_and_contextual_filtering_with_client_discovery() {
    use ra_mcp::client::http::{McpHttpParams, McpServerStreamableHttp};
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    let token = tokio_util::sync::CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = token.clone();
    let fixture = support::Fixture::new("http");
    let pages = fixture.pages.clone();
    let service = StreamableHttpService::new(
        move || Ok(fixture.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = token.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    let filter = ToolFilter::dynamic_sync(|context, tool| {
        Ok(*context.run_context().app_context::<bool>().unwrap() || tool.name == "search")
    });
    let server = Arc::new(
        McpServerStreamableHttp::new(McpHttpParams::new(format!("http://{address}/mcp")))
            .with_name("http")
            .with_options(caching().with_tool_filter(filter.clone())),
    );
    let resolver = ra_mcp::resolver::McpToolResolver::new(
        vec![(server.clone(), ToolFilter::default())],
        McpDiscoveryOptions::new(),
    );
    let agent = agent("regular");
    let regular_context = context(&agent, false);
    let admin_context = context(&agent, true);
    assert!(server.cached_tools().is_none());
    server.connect().await.unwrap();
    let tools = server
        .list_tools_with_context(Some(&regular_context), Some(regular_context.agent()))
        .await
        .unwrap();
    assert_eq!(names(&tools), ["search"]);
    assert_eq!(pages.load(Ordering::SeqCst), 2);
    let resolved =
        ra_core::tool::ToolResolver::get_tools(&resolver, &regular_context, &Default::default())
            .await
            .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].origin().name(), "search");
    let client = server.client().unwrap();
    let tools = discover_tools_with_context(
        &[(client.clone(), ToolFilter::default())],
        &McpDiscoveryOptions::new(),
        Some(&admin_context),
        Some(admin_context.agent()),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(pages.load(Ordering::SeqCst), 2);
    assert_eq!(client.cached_tools(), server.cached_tools());
    server.set_tool_filter(ToolFilter::default());
    assert_eq!(client.list_tools().await.unwrap().len(), 2);
    client.set_tool_filter(filter);
    server.set_cache_tools_list(false);
    server
        .list_tools_with_context(Some(&regular_context), Some(regular_context.agent()))
        .await
        .unwrap();
    assert_eq!(pages.load(Ordering::SeqCst), 4);
    server.set_cache_tools_list(true);
    client.invalidate_tools_cache();
    server
        .list_tools_with_context(Some(&regular_context), Some(regular_context.agent()))
        .await
        .unwrap();
    assert_eq!(pages.load(Ordering::SeqCst), 6);
    let cached = server.cached_tools();
    server.cleanup().await.unwrap();
    assert_eq!(server.cached_tools(), cached);
    assert!(
        client
            .list_tools_with_context(Some(&admin_context), Some(admin_context.agent()))
            .await
            .is_err()
    );
    assert!(
        ra_core::tool::ToolResolver::get_tools(&resolver, &regular_context, &Default::default(),)
            .await
            .is_err()
    );
    server.connect().await.unwrap();
    let resolved =
        ra_core::tool::ToolResolver::get_tools(&resolver, &regular_context, &Default::default())
            .await
            .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].origin().name(), "search");
    server
        .list_tools_with_context(Some(&regular_context), Some(regular_context.agent()))
        .await
        .unwrap();
    assert_eq!(pages.load(Ordering::SeqCst), 6);
    server.invalidate_tools_cache();
    server
        .list_tools_with_context(Some(&regular_context), Some(regular_context.agent()))
        .await
        .unwrap();
    assert_eq!(pages.load(Ordering::SeqCst), 8);
    server.cleanup().await.unwrap();
    token.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}
