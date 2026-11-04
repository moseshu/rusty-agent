//! Direct tool calls on the product MCP runtime, as MCP hooks make them: Codex's
//! `McpConnectionSet::call_tool` with `wait_for_server` false and its `CoreHookMcpExecutor`.
//!
//! The first three cases port Codex's `connection_manager_tests.rs`
//! (`call_tool_requires_connection_without_waiting_for_startup`,
//! `connected_call_respects_server_tool_filters`,
//! `call_tool_validates_environment_without_waiting_for_ready_connections`). Codex inserts a
//! client whose startup a gate holds; here the server's own connect is held. A ready server here
//! answers the call, where Codex's uninitialized test transport fails it; both show the ready
//! path reaching the transport. Every product server runs in the local environment, so the
//! environment case names `local` where Codex names `executor-a`.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_coding::{
    hook_mcp_executor::CodingHookMcpExecutor,
    hooks::{HookMcpCall, HookMcpExecutor},
    mcp::{CodingMcpRuntime, McpServerConfig},
};
use ra_core::{
    error::{Error, Result},
    session::SessionId,
};
use ra_mcp::{
    McpServer,
    client::{McpClient, McpClientOptions},
    filter::ToolFilter,
    in_process::InProcessToolServer,
    resolver::McpClientSource,
    tool::McpDiscoveryOptions,
};
use rmcp::{RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, Semaphore};

/// Answers `search` with two text blocks and an image, the request metadata, and `fail` with an
/// error result; `slow` never answers.
struct Docs;

impl ServerHandler for Docs {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult {
            tools: ["search", "fail", "slow"]
                .into_iter()
                .map(|name| {
                    serde_json::from_value(json!({
                        "name": name,
                        "inputSchema": {"type": "object", "properties": {}}
                    }))
                    .unwrap()
                })
                .collect(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, rmcp::ErrorData> {
        match request.name.as_ref() {
            "fail" => Ok(CallToolResult::error(vec![ContentBlock::text("boom")]).into()),
            "slow" => {
                std::future::pending::<()>().await;
                unreachable!()
            }
            _ => {
                let meta = serde_json::to_value(&context.meta).unwrap();
                let arguments = serde_json::to_value(&request.arguments).unwrap();
                Ok(CallToolResult::success(vec![
                    ContentBlock::text(format!("meta {meta}")),
                    ContentBlock::image("aW1n", "image/png"),
                    ContentBlock::text(format!("arguments {arguments}")),
                ])
                .into())
            }
        }
    }
}

/// A server whose connect can be held until the case releases it.
struct Server {
    name: String,
    hold_connect: bool,
    connect_started: Notify,
    release: Semaphore,
    client: Mutex<Option<McpClient>>,
    session: tokio::sync::Mutex<Option<InProcessToolServer>>,
}

impl Server {
    fn new(name: &str, hold_connect: bool) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            hold_connect,
            connect_started: Notify::new(),
            release: Semaphore::new(0),
            client: Mutex::new(None),
            session: tokio::sync::Mutex::new(None),
        })
    }
}

impl McpClientSource for Server {
    fn current_client(&self) -> Result<McpClient> {
        self.client
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Error::config("not initialized"))
    }
}

#[async_trait]
impl McpServer for Server {
    fn name(&self) -> &str {
        &self.name
    }
    async fn connect(&self) -> Result<()> {
        self.connect_started.notify_one();
        if self.hold_connect {
            self.release.acquire().await.unwrap().forget();
        }
        let session =
            InProcessToolServer::connect(&self.name, Docs, McpClientOptions::default()).await?;
        *self.client.lock().unwrap() = Some(session.client());
        *self.session.lock().await = Some(session);
        Ok(())
    }
    async fn cleanup(&self) -> Result<()> {
        self.client.lock().unwrap().take();
        if let Some(mut session) = self.session.lock().await.take() {
            session.cleanup().await?;
        }
        Ok(())
    }
    async fn list_tools(&self) -> Result<Vec<Tool>> {
        self.current_client()?.list_tools().await
    }
    async fn call_tool(
        &self,
        name: &str,
        args: Option<Value>,
        meta: Option<JsonObject>,
    ) -> Result<CallToolResult> {
        self.current_client()?.call_tool(name, args, meta).await
    }
    async fn list_prompts(&self) -> Result<ListPromptsResult> {
        self.current_client()?.list_prompts().await
    }
    async fn get_prompt(&self, name: &str, args: Option<JsonObject>) -> Result<GetPromptResult> {
        self.current_client()?.get_prompt(name, args).await
    }
}

fn runtime(config: McpServerConfig) -> Arc<CodingMcpRuntime> {
    CodingMcpRuntime::new(vec![config], McpDiscoveryOptions::new()).unwrap()
}

async fn call(
    runtime: &CodingMcpRuntime,
    tool: &str,
    environment_id: Option<&str>,
) -> anyhow::Result<CallToolResult> {
    runtime
        .call_tool_now(
            "docs",
            tool,
            environment_id,
            None,
            None,
            Some(Duration::from_secs(5)),
        )
        .await
}

#[tokio::test]
async fn call_tool_requires_connection_without_waiting_for_startup() {
    let server = Server::new("docs", true);
    let runtime = runtime(McpServerConfig::new(server.clone()));
    runtime.start_background();
    server.connect_started.notified().await;

    let pending_call =
        tokio::time::timeout(Duration::from_millis(50), call(&runtime, "search", None))
            .await
            .expect("ready-only invocation must not wait for pending server startup")
            .expect_err("pending server must not accept ready-only calls");
    assert!(pending_call.to_string().contains("not connected"));

    server.release.add_permits(1);
    runtime.connect_all().await.unwrap();

    let ready = call(&runtime, "search", None)
        .await
        .expect("a ready server answers the call");
    assert_eq!(ready.is_error, Some(false));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn call_tool_rejects_a_closed_client_without_refreshing_status() {
    let server = Server::new("docs", false);
    let runtime = runtime(McpServerConfig::new(server.clone()));
    runtime.connect_all().await.unwrap();

    // Keep the registered client and cached status while closing its transport.
    server.current_client().unwrap().cleanup().await.unwrap();
    let error = call(&runtime, "search", None)
        .await
        .expect_err("a closed client must be refused before invoking its tool");

    assert_eq!(error.to_string(), "MCP server 'docs' is not connected");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn connected_call_respects_server_tool_filters() {
    let server = Server::new("docs", false);
    let runtime = runtime(
        McpServerConfig::new(server)
            .with_filter(ToolFilter::new(None, HashSet::from(["search".to_owned()]))),
    );
    runtime.connect_all().await.unwrap();

    let filtered_error = call(&runtime, "search", None)
        .await
        .expect_err("disabled tools should not be callable");
    assert!(filtered_error.to_string().contains("disabled"));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn call_tool_validates_environment_without_waiting_for_ready_connections() {
    let server = Server::new("docs", true);
    let runtime = runtime(McpServerConfig::new(server.clone()));
    runtime.start_background();
    server.connect_started.notified().await;

    let mismatched_environment = call(&runtime, "search", Some("executor-b"))
        .await
        .expect_err("calls must reject a server from a different environment");
    assert_eq!(
        mismatched_environment.to_string(),
        "MCP server `docs` is running in environment `local`, expected `executor-b`"
    );

    let pending_call = tokio::time::timeout(
        Duration::from_millis(50),
        call(&runtime, "search", Some("local")),
    )
    .await
    .expect("environment-scoped calls must not wait for pending server startup")
    .expect_err("pending server must not accept environment-scoped calls");
    assert!(pending_call.to_string().contains("not connected"));

    server.release.add_permits(1);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_servers_and_dynamic_filters_are_refused() {
    let server = Server::new("docs", false);
    let runtime = runtime(
        McpServerConfig::new(server).with_filter(ToolFilter::dynamic_sync(|_, _| Ok(true))),
    );
    runtime.connect_all().await.unwrap();

    let unknown = runtime
        .call_tool_now("missing", "search", None, None, None, None)
        .await
        .expect_err("an unregistered server is refused");
    assert_eq!(unknown.to_string(), "unknown MCP server 'missing'");
    // A dynamic filter cannot be evaluated without a run, so the tool is not called unchecked.
    let dynamic = call(&runtime, "search", None)
        .await
        .expect_err("a dynamically filtered tool is refused outside a run");
    assert!(dynamic.to_string().contains("disabled"));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn disabled_servers_are_unknown_to_direct_calls() {
    // Codex's connection set holds enabled servers only, so calling a disabled one reports it
    // unknown rather than not connected.
    let configured_off = Server::new("docs", false);
    let runtime_off = runtime(McpServerConfig::new(configured_off).with_enabled(false));
    runtime_off.connect_all().await.unwrap();
    let error = call(&runtime_off, "search", None)
        .await
        .expect_err("a server disabled by configuration is refused");
    assert_eq!(error.to_string(), "unknown MCP server 'docs'");
    runtime_off.shutdown().await.unwrap();

    let toggled = Server::new("docs", false);
    let runtime = runtime(McpServerConfig::new(toggled));
    runtime.connect_all().await.unwrap();
    call(&runtime, "search", None)
        .await
        .expect("an enabled server answers");
    runtime.toggle_mcp_server("docs", false).await.unwrap();
    let error = call(&runtime, "search", None)
        .await
        .expect_err("a server disabled at runtime is refused");
    assert_eq!(error.to_string(), "unknown MCP server 'docs'");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_requested_deadline_bounds_the_call() {
    let server = Server::new("docs", false);
    let runtime = runtime(McpServerConfig::new(server));
    runtime.connect_all().await.unwrap();

    let started = std::time::Instant::now();
    let error = runtime
        .call_tool_now(
            "docs",
            "slow",
            None,
            None,
            None,
            Some(Duration::from_millis(100)),
        )
        .await
        .expect_err("the call ends at the requested deadline");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        format!("{error:#}").contains("tool call failed for `docs/slow`"),
        "{error:#}"
    );
    runtime.shutdown().await.unwrap();
}

fn hook_call(tool: &str) -> HookMcpCall {
    let mut metadata = Map::new();
    metadata.insert("source".to_owned(), json!("hook"));
    let mut input = Map::new();
    input.insert("query".to_owned(), json!("rust"));
    HookMcpCall {
        server: "docs".to_owned(),
        tool: tool.to_owned(),
        environment_id: None,
        metadata: Some(metadata),
        input,
        timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn hook_executor_joins_text_blocks_and_sends_the_thread_id() {
    let server = Server::new("docs", false);
    let runtime = runtime(McpServerConfig::new(server));
    runtime.connect_all().await.unwrap();
    let executor = CodingHookMcpExecutor::new(Arc::clone(&runtime), SessionId::new("thread-7"));

    let text = executor.execute(hook_call("search")).await.unwrap();

    let mut lines = text.lines();
    let meta: Value =
        serde_json::from_str(lines.next().unwrap().strip_prefix("meta ").unwrap()).unwrap();
    assert_eq!(meta["threadId"], "thread-7");
    assert_eq!(meta["source"], "hook");
    assert_eq!(lines.next(), Some(r#"arguments {"query":"rust"}"#));
    assert_eq!(lines.next(), None, "non-text blocks are skipped: {text}");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn hook_executor_reports_an_error_result() {
    let server = Server::new("docs", false);
    let runtime = runtime(McpServerConfig::new(server));
    runtime.connect_all().await.unwrap();
    let executor = CodingHookMcpExecutor::new(Arc::clone(&runtime), SessionId::new("thread-7"));

    let error = executor.execute(hook_call("fail")).await.unwrap_err();

    assert_eq!(error.to_string(), "MCP tool returned an error: boom");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_catalog_keeps_app_only_tools_and_honors_both_filters() {
    let tools = ["visible", "app_only", "server_disabled", "binding_disabled"].into_iter().map(|name| serde_json::from_value(json!({"name":name,"inputSchema":{"type":"object","properties":{}},"_meta":{"ui":{"visibility":["app"]}}})).unwrap()).collect();
    let catalog = ra_mcp::tool::McpToolCatalog::new(
        "cleanup",
        tools,
        McpClientOptions::default().with_tool_filter(ToolFilter::new(
            None,
            HashSet::from(["server_disabled".into()]),
        )),
    );
    let agent = ra_core::agent::AgentSpec::builder()
        .id(ra_core::agent::AgentId::new("test-agent"))
        .name("test")
        .build()
        .unwrap();
    let run = ra_core::context::RunContext::new(ra_core::state::RunId::new("run"), &agent);
    let infos = catalog
        .enabled_tool_infos(
            &ToolFilter::new(None, HashSet::from(["binding_disabled".into()])),
            &run,
        )
        .await
        .unwrap();
    assert_eq!(
        infos
            .iter()
            .map(|info| info.tool().name.as_ref())
            .collect::<Vec<_>>(),
        ["visible", "app_only"]
    );
}
