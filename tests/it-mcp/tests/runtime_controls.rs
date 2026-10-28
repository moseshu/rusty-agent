//! Product control acceptance, using real initialized sessions and lifecycle fault injection.

use async_trait::async_trait;
use ra_coding::mcp::{CodingMcpRuntime, McpServerConfig};
use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::{Error, Result},
    state::RunId,
    tool::ToolResolver,
};
use ra_mcp::{
    McpServer, McpServerManagerOptions,
    client::{McpClient, McpClientOptions},
    in_process::InProcessToolServer,
    resolver::McpClientSource,
    tool::McpDiscoveryOptions,
};
use ra_protocol::server::{McpRuntimeControl, protocol::McpServerConnectionStatus as Status};
use rmcp::{RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

struct InventoryGate {
    blocked: AtomicBool,
    started: Notify,
    permits: Semaphore,
}

struct Catalog(
    usize,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
    Option<Arc<InventoryGate>>,
);
impl ServerHandler for Catalog {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, rmcp::ErrorData> {
        if let Some(gate) = &self.3
            && gate.blocked.load(Ordering::SeqCst)
        {
            gate.started.notify_one();
            gate.permits.acquire().await.unwrap().forget();
        }
        if self.1.load(Ordering::SeqCst) {
            return Err(rmcp::ErrorData::internal_error("catalog unavailable", None));
        }
        Ok(ListToolsResult {
            tools: vec![
                serde_json::from_value(json!({
                    "name":format!("echo_{}", self.0),
                    "inputSchema":{"type":"object","properties":{}}
                }))
                .unwrap(),
            ],
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        _: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, rmcp::ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text("called")]).into())
    }
    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, rmcp::ErrorData> {
        self.2.fetch_add(1, Ordering::SeqCst);
        Ok(ListResourcesResult::default())
    }
    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, rmcp::ErrorData> {
        self.2.fetch_add(1, Ordering::SeqCst);
        Ok(ListResourceTemplatesResult::default())
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, rmcp::ErrorData> {
        Ok(
            serde_json::from_value::<ReadResourceResult>(json!({"contents":[{
                "uri":request.uri,"text":format!("session generation {}",self.0)
            }]}))
            .unwrap()
            .into(),
        )
    }
}
struct Probe {
    name: String,
    fail_listing: Arc<AtomicBool>,
    inventory_gate: Arc<InventoryGate>,
    resource_calls: Arc<AtomicUsize>,
    client: Mutex<Option<McpClient>>,
    session: tokio::sync::Mutex<Option<InProcessToolServer>>,
    connects: AtomicUsize,
    cleanups: AtomicUsize,
    failures: AtomicUsize,
    owner: Mutex<Option<tokio::task::Id>>,
    block_connect: AtomicBool,
    block_cleanup: AtomicBool,
    fail_cleanup: AtomicBool,
    started: Notify,
    cleanup_started: Notify,
    gate: Semaphore,
    cleanup_gate: Semaphore,
}
impl Probe {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            fail_listing: Arc::new(AtomicBool::new(false)),
            inventory_gate: Arc::new(InventoryGate {
                blocked: AtomicBool::new(false),
                started: Notify::new(),
                permits: Semaphore::new(0),
            }),
            resource_calls: Arc::new(AtomicUsize::new(0)),
            client: Mutex::new(None),
            session: tokio::sync::Mutex::new(None),
            connects: AtomicUsize::new(0),
            cleanups: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            owner: Mutex::new(None),
            block_connect: AtomicBool::new(false),
            block_cleanup: AtomicBool::new(false),
            fail_cleanup: AtomicBool::new(false),
            started: Notify::new(),
            cleanup_started: Notify::new(),
            gate: Semaphore::new(0),
            cleanup_gate: Semaphore::new(0),
        })
    }
}
impl McpClientSource for Probe {
    fn current_client(&self) -> Result<McpClient> {
        self.client
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Error::config("not initialized"))
    }
}
#[async_trait]
impl McpServer for Probe {
    fn name(&self) -> &str {
        &self.name
    }
    async fn connect(&self) -> Result<()> {
        let count = self.connects.fetch_add(1, Ordering::SeqCst) + 1;
        *self.owner.lock().unwrap() = Some(tokio::task::id());
        self.started.notify_one();
        if self.block_connect.load(Ordering::SeqCst) {
            self.gate.acquire().await.unwrap().forget();
        }
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(
                Error::config("MCP handshake rejected: unsupported protocol")
                    .with_source(std::io::Error::other("secret-token-123 connect failed")),
            );
        }
        let session = InProcessToolServer::connect(
            &self.name,
            Catalog(
                count,
                self.fail_listing.clone(),
                self.resource_calls.clone(),
                Some(self.inventory_gate.clone()),
            ),
            McpClientOptions::default(),
        )
        .await?;
        *self.client.lock().unwrap() = Some(session.client());
        *self.session.lock().await = Some(session);
        Ok(())
    }
    async fn cleanup(&self) -> Result<()> {
        self.cleanups.fetch_add(1, Ordering::SeqCst);
        assert_eq!(*self.owner.lock().unwrap(), Some(tokio::task::id()));
        self.cleanup_started.notify_one();
        if self.block_cleanup.load(Ordering::SeqCst) {
            self.cleanup_gate.acquire().await.unwrap().forget();
        }
        if self.fail_cleanup.load(Ordering::SeqCst) {
            return Err(Error::config("secret cleanup failed"));
        }
        // Closing the fixture wakes in-flight requests before joining its handler task.
        if self.inventory_gate.blocked.swap(false, Ordering::SeqCst) {
            self.inventory_gate.permits.add_permits(1);
        }
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
fn runtime(probes: &[Arc<Probe>]) -> Arc<CodingMcpRuntime> {
    CodingMcpRuntime::new(
        probes
            .iter()
            .map(|p| McpServerConfig::new(p.clone()))
            .collect(),
        McpDiscoveryOptions::new(),
    )
    .unwrap()
}
fn context() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("test"))
        .name("Test")
        .build()
        .unwrap();
    RunContext::new(RunId::new("runtime-controls"), &agent)
}
async fn names(runtime: &CodingMcpRuntime) -> Vec<String> {
    runtime
        .get_tools(&context(), &BTreeSet::new())
        .await
        .unwrap()
        .into_iter()
        .filter(|tool| {
            !matches!(
                tool.schema().name(),
                "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource"
            )
        })
        .map(|tool| tool.schema().name().to_owned())
        .collect()
}

#[tokio::test]
async fn toggle_and_reconnect_refresh_next_model_surface_and_close_old_session() {
    let server = Probe::new("docs");
    let runtime = runtime(std::slice::from_ref(&server));
    assert_eq!(
        runtime.get_mcp_status()[0].runtime_status(),
        Some(Status::NotStarted)
    );
    runtime.connect_all().await.unwrap();
    assert_eq!(names(&runtime).await, ["echo_1"]);
    let old = server.current_client().unwrap();
    runtime.toggle_mcp_server("docs", false).await.unwrap();
    assert_eq!(
        runtime.get_mcp_status()[0].runtime_status(),
        Some(Status::Disabled)
    );
    assert!(names(&runtime).await.is_empty());
    assert!(!old.is_connected().await);
    runtime.reconnect_mcp_server(None, false).await.unwrap();
    assert_eq!(server.connects.load(Ordering::SeqCst), 1);
    runtime.toggle_mcp_server("docs", true).await.unwrap();
    assert_eq!(names(&runtime).await, ["echo_2"]);
    runtime
        .reconnect_mcp_server(Some("docs"), false)
        .await
        .unwrap();
    assert_eq!(names(&runtime).await, ["echo_3"]);
    assert_eq!(
        McpRuntimeControl::inventory(runtime.as_ref(), "docs", false)
            .await
            .runtime_status(),
        Some(Status::Connected)
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_only_retry_preserves_healthy_connections_and_redacts_errors() {
    let good = Probe::new("good");
    let bad = Probe::new("bad");
    bad.failures.store(1, Ordering::SeqCst);
    let runtime = runtime(&[good.clone(), bad.clone()]);
    runtime.connect_all().await.unwrap();
    assert_eq!(names(&runtime).await, ["echo_1"]);
    let status = serde_json::to_value(runtime.get_mcp_status()).unwrap();
    assert_eq!(status[1]["runtimeStatus"], "failed");
    assert_eq!(status[1]["lifecyclePhase"], "connect");
    assert!(status[1]["toolsError"].is_null());
    assert!(!status.to_string().contains("secret-token"));
    assert!(
        std::error::Error::source(runtime.errors()[0].2.as_ref())
            .unwrap()
            .to_string()
            .contains("secret-token")
    );
    runtime.reconnect_mcp_server(None, true).await.unwrap();
    assert_eq!(good.connects.load(Ordering::SeqCst), 1);
    assert_eq!(good.cleanups.load(Ordering::SeqCst), 0);
    assert_eq!(bad.connects.load(Ordering::SeqCst), 2);
    assert_eq!(bad.cleanups.load(Ordering::SeqCst), 1);
    assert!(runtime.errors().is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn parallel_startup_publishes_healthy_status_while_other_server_waits() {
    let slow = Probe::new("slow");
    slow.block_connect.store(true, Ordering::SeqCst);
    let good = Probe::new("good");
    let runtime = CodingMcpRuntime::new(
        vec![
            McpServerConfig::new(slow.clone()),
            McpServerConfig::new(good.clone()),
        ],
        McpDiscoveryOptions::new().with_naming(ra_mcp::tool::McpToolNaming::ServerPrefixed),
    )
    .unwrap();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.connect_all().await }
    });
    slow.started.notified().await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.get_mcp_status()[1].runtime_status() == Some(Status::Connected) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        runtime.get_mcp_status()[0].runtime_status(),
        Some(Status::Starting)
    );
    let discovery = tokio::spawn({
        let runtime = runtime.clone();
        async move { names(&runtime).await }
    });
    fixtures::settle().await;
    assert!(!discovery.is_finished());
    slow.gate.add_permits(1);
    task.await.unwrap().unwrap();
    assert_eq!(
        discovery.await.unwrap(),
        ["mcp_slow__echo_1", "mcp_good__echo_1"]
    );
    assert!(runtime.errors().is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_toggle_waiter_keeps_cleanup_owned_and_serializes_enable() {
    let probe = Probe::new("docs");
    let runtime = runtime(std::slice::from_ref(&probe));
    runtime.connect_all().await.unwrap();
    probe.block_cleanup.store(true, Ordering::SeqCst);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.toggle_mcp_server("docs", false).await }
    });
    probe.cleanup_started.notified().await;
    task.abort();
    let _ = task.await;
    assert!(names(&runtime).await.is_empty());
    let enabled = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.toggle_mcp_server("docs", true).await }
    });
    tokio::task::yield_now().await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    probe.block_cleanup.store(false, Ordering::SeqCst);
    probe.cleanup_gate.add_permits(1);
    enabled.await.unwrap().unwrap();
    assert_eq!(names(&runtime).await, ["echo_2"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_failure_keeps_phase_and_blocks_second_connection() {
    let probe = Probe::new("docs");
    let runtime = runtime(std::slice::from_ref(&probe));
    runtime.connect_all().await.unwrap();
    probe.fail_cleanup.store(true, Ordering::SeqCst);
    runtime.reconnect_mcp_server(None, false).await.unwrap();
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    assert!(names(&runtime).await.is_empty());
    assert_eq!(runtime.errors()[0].1, "cleanup");
    runtime.reconnect_mcp_server(None, true).await.unwrap();
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    probe.fail_cleanup.store(false, Ordering::SeqCst);
    let _ = runtime.shutdown().await;
}

#[tokio::test]
async fn independent_connect_timeout_isolated_from_other_server() {
    let probe = Probe::new("slow");
    probe.block_connect.store(true, Ordering::SeqCst);
    let good = Probe::new("good");
    let options = McpServerManagerOptions::default()
        .with_connect_timeout(Some(Duration::from_millis(40)))
        .unwrap();
    let runtime = CodingMcpRuntime::new(
        vec![
            McpServerConfig::new(probe.clone()).with_lifecycle_options(options),
            McpServerConfig::new(good.clone()),
        ],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), runtime.connect_all())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runtime.errors()[0].1, "connect");
    assert_eq!(names(&runtime).await, ["echo_1"]);
    probe.block_connect.store(false, Ordering::SeqCst);
    runtime.reconnect_mcp_server(None, true).await.unwrap();
    assert_eq!(probe.connects.load(Ordering::SeqCst), 2);
    runtime.shutdown().await.unwrap();
}

#[test]
fn rejects_duplicate_ui_identities() {
    let probe = Probe::new("same");
    assert!(
        CodingMcpRuntime::new(
            vec![
                McpServerConfig::new(probe.clone()),
                McpServerConfig::new(probe)
            ],
            McpDiscoveryOptions::new()
        )
        .is_err()
    );
}

use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcRequest, RequestId},
    server::AppServer,
    transport::{
        ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use tokio::sync::mpsc;

async fn open(
    events: &mpsc::Sender<TransportEvent>,
    id: u64,
) -> mpsc::Receiver<QueuedOutgoingMessage> {
    let (writer, receiver) = mpsc::channel(128);
    events
        .send(connection_opened(
            ConnectionId::new(id),
            ConnectionOrigin::WebSocket,
            writer,
            None,
        ))
        .await
        .unwrap();
    receiver
}
async fn call(
    events: &mpsc::Sender<TransportEvent>,
    receiver: &mut mpsc::Receiver<QueuedOutgoingMessage>,
    connection: u64,
    id: i64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    let request = JsonRpcRequest::new(RequestId::Integer(id), method).with_params(params);
    events
        .send(TransportEvent::IncomingMessage {
            connection_id: ConnectionId::new(connection),
            message: JsonRpcMessage::Request(request),
        })
        .await
        .unwrap();
    let mut notifications = Vec::new();
    loop {
        let queued = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let value = serde_json::to_value(queued.message()).unwrap();
        if value["id"] == id {
            return (value, notifications);
        }
        notifications.push(value);
    }
}
fn init_params(opt_out: bool) -> Value {
    json!({"clientInfo":{"name":"mcp-test","version":"1"},"capabilities":{"optOutNotificationMethods":if opt_out { vec!["mcpServer/startupStatus/updated","mcpServer/status/updated"] } else { vec![] }}})
}

#[tokio::test]
async fn app_server_lists_pages_controls_connections_and_routes_status_to_ui() {
    let docs = Probe::new("docs");
    let alpha = Probe::new("alpha");
    let runtime = runtime(&[docs.clone(), alpha.clone()]);
    let server = AppServer::new("/tmp/mcp-control-test")
        .unwrap()
        .with_mcp_runtime(runtime.clone());
    let (events, receiver) = mpsc::channel(128);
    let task = tokio::spawn(server.run(receiver));
    let mut client = open(&events, 1).await;
    let mut quiet = open(&events, 2).await;
    let mut uninitialized = open(&events, 3).await;
    assert_eq!(
        call(
            &events,
            &mut client,
            1,
            1,
            "mcpServerStatus/list",
            json!({})
        )
        .await
        .0["error"]["message"],
        "Not initialized"
    );
    assert!(
        call(&events, &mut client, 1, 2, "initialize", init_params(false))
            .await
            .0["result"]
            .is_object()
    );
    assert!(
        call(&events, &mut quiet, 2, 3, "initialize", init_params(true))
            .await
            .0["result"]
            .is_object()
    );
    let initial = call(
        &events,
        &mut client,
        1,
        4,
        "mcpServerStatus/list",
        json!({"limit":0}),
    )
    .await
    .0;
    assert_eq!(initial["result"]["data"][0]["name"], "alpha");
    assert_eq!(initial["result"]["data"].as_array().unwrap().len(), 1);
    assert_eq!(initial["result"]["nextCursor"], "1");
    let result = call(
        &events,
        &mut client,
        1,
        5,
        "mcpServer/reconnect",
        json!({"failedOnly":false}),
    )
    .await;
    assert_eq!(result.0["result"], json!({}));
    let (listed, mut notifications) = call(
        &events,
        &mut client,
        1,
        6,
        "mcpServerStatus/list",
        json!({"serverName":"docs","detail":"toolsAndAuthOnly"}),
    )
    .await;
    notifications.extend(result.1);
    assert_eq!(listed["result"]["data"][0]["runtimeStatus"], "connected");
    assert!(listed["result"]["data"][0]["tools"]["echo_1"].is_object());
    assert!(listed["result"]["data"][0]["serverInfo"].is_object());
    assert!(
        notifications
            .iter()
            .any(|value| value["method"] == "mcpServer/startupStatus/updated"
                && value["params"]["status"] == "starting")
    );
    assert!(
        notifications
            .iter()
            .any(|value| value["method"] == "mcpServer/startupStatus/updated"
                && value["params"]["status"] == "ready")
    );
    assert!(
        call(&events, &mut quiet, 2, 7, "mcpServerStatus/list", json!({}))
            .await
            .1
            .is_empty()
    );
    assert!(
        call(
            &events,
            &mut uninitialized,
            3,
            8,
            "initialize",
            init_params(false)
        )
        .await
        .1
        .is_empty()
    );
    assert!(
        call(
            &events,
            &mut client,
            1,
            9,
            "mcpServer/toggle",
            json!({"name":"docs","enabled":false})
        )
        .await
        .0["result"]
            .is_object()
    );
    let disabled = call(
        &events,
        &mut client,
        1,
        10,
        "mcpServerStatus/list",
        json!({"serverName":"docs"}),
    )
    .await
    .0;
    assert_eq!(disabled["result"]["data"][0]["runtimeStatus"], "disabled");
    assert_eq!(disabled["result"]["data"][0]["tools"], json!({}));
    assert!(
        call(
            &events,
            &mut client,
            1,
            11,
            "config/mcpServer/reload",
            Value::Null
        )
        .await
        .0["result"]
            .is_object()
    );
    assert_eq!(docs.connects.load(Ordering::SeqCst), 1);
    assert_eq!(alpha.connects.load(Ordering::SeqCst), 1);
    for (id, params, expected) in [
        (12, json!({"cursor":"wat"}), "invalid cursor: wat"),
        (
            13,
            json!({"cursor":"3"}),
            "cursor 3 exceeds total MCP servers 2",
        ),
        (
            14,
            json!({"threadId":"thread-1"}),
            "mcpServerStatus/list.threadId is not supported by the shared host runtime",
        ),
    ] {
        assert_eq!(
            call(&events, &mut client, 1, id, "mcpServerStatus/list", params)
                .await
                .0["error"]["message"],
            expected
        );
    }
    assert!(
        call(
            &events,
            &mut client,
            1,
            15,
            "mcpServer/toggle",
            json!({"name":"missing","enabled":true})
        )
        .await
        .0["error"]
            .is_object()
    );
    drop(events);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(alpha.current_client().is_err());
}

#[tokio::test]
async fn actual_stdio_control_closes_and_replaces_the_transport_session() {
    use ra_mcp::client::stdio::{McpServerStdio, McpStdioParams};
    let server = Arc::new(
        McpServerStdio::new(McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture")))
            .with_name("stdio"),
    );
    let runtime = CodingMcpRuntime::new(
        vec![McpServerConfig::new(server.clone())],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    let old = server.client().unwrap();
    assert!(!names(&runtime).await.is_empty());
    runtime
        .reconnect_mcp_server(Some("stdio"), false)
        .await
        .unwrap();
    assert!(!old.is_connected().await);
    assert!(server.client().unwrap().is_connected().await);
    runtime.toggle_mcp_server("stdio", false).await.unwrap();
    assert!(names(&runtime).await.is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn inventory_failure_retains_capabilities_and_detail_skips_resource_requests() {
    let probe = Probe::new("docs");
    let runtime = runtime(std::slice::from_ref(&probe));
    runtime.connect_all().await.unwrap();
    let inventory = McpRuntimeControl::inventory(runtime.as_ref(), "docs", false).await;
    assert_eq!(inventory.runtime_status(), Some(Status::Connected));
    assert_eq!(probe.resource_calls.load(Ordering::SeqCst), 0);
    let _ = McpRuntimeControl::inventory(runtime.as_ref(), "docs", true).await;
    assert_eq!(probe.resource_calls.load(Ordering::SeqCst), 2);
    probe.fail_listing.store(true, Ordering::SeqCst);
    let failed =
        serde_json::to_value(McpRuntimeControl::inventory(runtime.as_ref(), "docs", false).await)
            .unwrap();
    assert!(failed["serverCapabilities"]["tools"].is_object());
    assert!(
        failed["toolsError"]
            .as_str()
            .unwrap()
            .contains("catalog unavailable")
    );
    assert_eq!(failed["tools"], json!({}));
    assert_eq!(failed["runtimeStatus"], "connected");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn closed_transport_is_reported_without_waiting_and_can_be_retried() {
    let probe = Probe::new("docs");
    let runtime = runtime(std::slice::from_ref(&probe));
    runtime.connect_all().await.unwrap();
    assert_eq!(names(&runtime).await, ["echo_1"]);
    probe.current_client().unwrap().cleanup().await.unwrap();
    assert_eq!(
        runtime.get_mcp_status()[0].runtime_status(),
        Some(Status::Failed)
    );
    assert!(names(&runtime).await.is_empty());
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.errors()[0].1, "session");
    runtime.reconnect_mcp_server(None, true).await.unwrap();
    assert_eq!(names(&runtime).await, ["echo_2"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn initially_disabled_server_stays_closed_until_enabled_and_unknown_names_fail() {
    let probe = Probe::new("docs");
    let runtime = CodingMcpRuntime::new(
        vec![McpServerConfig::new(probe.clone()).with_enabled(false)],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    assert!(runtime.toggle_mcp_server("missing", true).await.is_err());
    assert!(
        runtime
            .reconnect_mcp_server(Some("missing"), false)
            .await
            .is_err()
    );
    assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    runtime.toggle_mcp_server("docs", true).await.unwrap();
    assert_eq!(names(&runtime).await, ["echo_1"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn actual_http_control_reconnects_and_shutdown_closes_current_session() {
    use ra_mcp::client::http::{McpHttpParams, McpServerStreamableHttp};
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    let cancellation = tokio_util::sync::CancellationToken::new();
    let service = StreamableHttpService::new(
        || {
            Ok(Catalog(
                1,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicUsize::new(0)),
                None,
            ))
        },
        Arc::new(LocalSessionManager::default()),
        {
            let mut config = StreamableHttpServerConfig::default();
            config.cancellation_token = cancellation.clone();
            config
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let app = axum::Router::new().nest_service("/mcp", service);
    let stop = cancellation.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap()
    });
    let server = Arc::new(McpServerStreamableHttp::new(McpHttpParams::new(url)).with_name("http"));
    let runtime = CodingMcpRuntime::new(
        vec![McpServerConfig::new(server.clone())],
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    let old = server.client().unwrap();
    let session = server.session_id();
    assert!(session.is_some());
    runtime
        .reconnect_mcp_server(Some("http"), false)
        .await
        .unwrap();
    assert!(!old.is_connected().await);
    assert_ne!(server.session_id(), session);
    assert_eq!(names(&runtime).await, ["echo_1"]);
    runtime.shutdown().await.unwrap();
    assert!(server.client().is_err());
    cancellation.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn coding_host_installs_the_same_runtime_for_live_discovery() {
    use ra_coding::{agent::build_agent_with_host, host::CodingHost};
    let probe = Probe::new("docs");
    let runtime = runtime(std::slice::from_ref(&probe));
    let host = CodingHost::open(std::env::temp_dir())
        .unwrap()
        .with_mcp_runtime(runtime.clone());
    let agent = build_agent_with_host(
        AgentId::new("coding"),
        "Coding",
        &ra_core::prompt::PromptRole::Main,
        &host,
    )
    .await
    .unwrap();
    let resolver = agent.tool_resolver().unwrap();
    let context = RunContext::new(RunId::new("host-wiring"), &agent);
    assert_eq!(
        resolver
            .get_tools(&context, &BTreeSet::new())
            .await
            .unwrap()[0]
            .schema()
            .name(),
        "echo_1"
    );
    let cloned = host.with_new_process_manager();
    assert!(Arc::ptr_eq(cloned.mcp_runtime().unwrap(), &runtime));
    runtime.toggle_mcp_server("docs", false).await.unwrap();
    assert!(
        resolver
            .get_tools(&context, &BTreeSet::new())
            .await
            .unwrap()
            .is_empty()
    );
    runtime.shutdown().await.unwrap();
}

#[path = "runtime_controls/fixtures.rs"]
mod fixtures;

#[path = "runtime_controls/discovery.rs"]
mod discovery;

#[path = "runtime_controls/lazy_startup.rs"]
mod lazy_startup;

#[path = "runtime_controls/recovery.rs"]
mod recovery;

#[path = "runtime_controls/registry_ordering.rs"]
mod registry_ordering;

#[path = "runtime_controls/startup_status.rs"]
mod startup_status;

#[path = "runtime_controls/thread_preparation.rs"]
mod thread_preparation;

#[path = "runtime_controls/shared_startup.rs"]
mod shared_startup;

#[path = "runtime_controls/resources.rs"]
mod resources;

#[path = "runtime_controls/configured_host.rs"]
mod configured_host;

#[path = "runtime_controls/replacement.rs"]
mod replacement;
