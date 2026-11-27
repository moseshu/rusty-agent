//! Cross-connection catalog reuse through configured transports.
use super::*;
use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::Result,
    item::CallId,
    state::RunId,
    tool::{Tool, ToolApprovalRequest, ToolContext, ToolOutput, ToolResolver},
};
use ra_mcp::{McpCatalogBuilder, McpServerRegistration};
use rmcp::{
    RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerConfig, ServerRequest,
    },
    service::{Peer, RequestContext},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

struct StartupCatalog {
    session: usize,
    gates: Arc<Vec<Semaphore>>,
    requested: mpsc::UnboundedSender<(usize, Peer<RoleServer>)>,
    opt_out_session: Option<usize>,
    initialize_gates: Arc<Vec<Semaphore>>,
    initializing: mpsc::UnboundedSender<usize>,
    revision: Arc<AtomicUsize>,
    layout: Arc<AtomicUsize>,
    live_tools: Arc<Mutex<Option<Vec<rmcp::model::Tool>>>>,
    failing_session: Arc<AtomicUsize>,
    calls: mpsc::UnboundedSender<String>,
}

/// Announces a tool-list change and returns once the client has read the notification.
/// Streamable HTTP sends notifications on the standalone stream but each response on its
/// request's stream, so the two arrive in no fixed order. A ping sent outside any request handler
/// follows the notification on the standalone stream, and its response shows the client read both.
async fn announce_tool_list_change(peer: &Peer<RoleServer>) {
    peer.notify_tool_list_changed().await.unwrap();
    let peer = peer.clone();
    // A request sent from a handler task would ride that request's stream instead.
    tokio::spawn(async move {
        peer.send_request(ServerRequest::PingRequest(Default::default()))
            .await
    })
    .await
    .unwrap()
    .unwrap();
}

/// Lists only the session tool by default. A companion tool can follow or precede it, so a
/// refetch can return an equivalent catalog in a different order.
const COMPANION_AFTER: usize = 1;
const COMPANION_BEFORE: usize = 2;

impl ServerHandler for StartupCatalog {
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        if self.opt_out_session == Some(self.session) {
            capabilities.experimental = Some(
                serde_json::from_value(json!({
                    "codex/tool-catalog-cache": {"cacheable": false}
                }))
                .unwrap(),
            );
        }
        ServerConfig::new(capabilities)
    }

    async fn initialize(
        &self,
        params: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, rmcp::ErrorData> {
        self.initializing.send(self.session).unwrap();
        self.initialize_gates[self.session - 1]
            .acquire()
            .await
            .unwrap()
            .forget();
        if self.failing_session.load(Ordering::SeqCst) == self.session {
            return Err(rmcp::ErrorData::internal_error(
                "fixture initialization rejected",
                None,
            ));
        }
        context.peer.set_peer_info(params.clone());
        self.negotiate_initialize(&params)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let revision = self.revision.load(Ordering::SeqCst);
        let tool_name = if revision == 0 {
            format!("session_{}", self.session)
        } else {
            format!("session_{}_revision_{revision}", self.session)
        };
        self.requested
            .send((self.session, context.peer.clone()))
            .unwrap();
        self.gates[self.session - 1]
            .acquire()
            .await
            .unwrap()
            .forget();
        let tool = |name: &str| {
            serde_json::from_value(json!({
                "name": name,
                "inputSchema": {"type": "object"},
                "annotations": {"readOnlyHint": true}
            }))
            .unwrap()
        };
        let tools = self.live_tools.lock().unwrap().clone().unwrap_or_else(|| {
            match self.layout.load(Ordering::SeqCst) {
                COMPANION_AFTER => vec![tool(&tool_name), tool("companion")],
                COMPANION_BEFORE => vec![tool("companion"), tool(&tool_name)],
                _ => vec![tool(&tool_name)],
            }
        });
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        if request.name == "notify" {
            announce_tool_list_change(&context.peer).await;
        } else {
            self.calls.send(request.name.to_string()).unwrap();
        }
        Ok(CallToolResult::success(vec![ContentBlock::text("called")]).into())
    }
}

struct Fixture {
    url: String,
    gates: Arc<Vec<Semaphore>>,
    requested: mpsc::UnboundedReceiver<(usize, Peer<RoleServer>)>,
    initialize_gates: Arc<Vec<Semaphore>>,
    initializing: mpsc::UnboundedReceiver<usize>,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    revision: Arc<AtomicUsize>,
    layout: Arc<AtomicUsize>,
    live_tools: Arc<Mutex<Option<Vec<rmcp::model::Tool>>>>,
    failing_session: Arc<AtomicUsize>,
    calls: mpsc::UnboundedReceiver<String>,
}

impl Fixture {
    async fn start(opt_out_session: Option<usize>) -> Self {
        let cancellation = CancellationToken::new();
        let gates = Arc::new((0..4).map(|_| Semaphore::new(0)).collect::<Vec<_>>());
        let (sender, requested) = mpsc::unbounded_channel();
        let (call_sender, calls) = mpsc::unbounded_channel();
        let initialize_gates = Arc::new((0..4).map(|_| Semaphore::new(1)).collect::<Vec<_>>());
        let (initializing_sender, initializing) = mpsc::unbounded_channel();
        let sessions = AtomicUsize::new(0);
        let revision = Arc::new(AtomicUsize::new(0));
        let layout = Arc::new(AtomicUsize::new(0));
        let live_tools = Arc::new(Mutex::new(None));
        let failing_session = Arc::new(AtomicUsize::new(0));
        let service = StreamableHttpService::new(
            {
                let gates = gates.clone();
                let initialize_gates = initialize_gates.clone();
                let revision = revision.clone();
                let layout = layout.clone();
                let live_tools = live_tools.clone();
                let failing_session = failing_session.clone();
                move || {
                    Ok(StartupCatalog {
                        session: sessions.fetch_add(1, Ordering::SeqCst) + 1,
                        gates: gates.clone(),
                        requested: sender.clone(),
                        opt_out_session,
                        initialize_gates: initialize_gates.clone(),
                        initializing: initializing_sender.clone(),
                        revision: revision.clone(),
                        layout: layout.clone(),
                        live_tools: live_tools.clone(),
                        failing_session: failing_session.clone(),
                        calls: call_sender.clone(),
                    })
                }
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
        let shutdown = cancellation.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            url,
            gates,
            requested,
            initialize_gates,
            initializing,
            cancellation,
            task,
            revision,
            layout,
            live_tools,
            failing_session,
            calls,
        }
    }

    fn registrations(&self, alternate_credentials: bool) -> Vec<ra_coding::mcp::McpServerConfig> {
        self.configured(
            alternate_credentials,
            Some("catalog"),
            OutboundProxyPolicy::ReqwestDefault,
        )
    }

    fn configured(
        &self,
        alternate_credentials: bool,
        readiness: Option<&str>,
        proxy: OutboundProxyPolicy,
    ) -> Vec<ra_coding::mcp::McpServerConfig> {
        let mut config = json!({"url": self.url});
        if let Some(readiness) = readiness {
            config["startup_readiness"] = json!(readiness);
        }
        if alternate_credentials {
            config["http_headers"] = json!({"Authorization": "Bearer alternate-test-token"});
        }
        self.registrations_from(config, proxy)
    }

    fn registrations_from(
        &self,
        config: serde_json::Value,
        proxy: OutboundProxyPolicy,
    ) -> Vec<ra_coding::mcp::McpServerConfig> {
        let config = serde_json::from_value(config).unwrap();
        let mut catalog = McpCatalogBuilder::default();
        let name = "docs".to_owned();
        catalog.register(McpServerRegistration::from_config(name, config));
        product_mcp_servers(
            &catalog.build(),
            Path::new("."),
            &HttpClientFactory::new(proxy),
        )
        .servers
    }

    fn pause_initialize(&self, session: usize) {
        self.initialize_gates[session - 1]
            .try_acquire()
            .unwrap()
            .forget();
    }

    async fn wait_for_initialize(&mut self, session: usize) {
        tokio::time::timeout(Duration::from_secs(4), async {
            while let Some(index) = self.initializing.recv().await {
                if index == session {
                    return;
                }
                assert!(index < session);
            }
            panic!("initialize channel closed");
        })
        .await
        .unwrap();
    }

    fn resume_initialize(&self, session: usize) {
        self.initialize_gates[session - 1].add_permits(1);
    }

    async fn discover(&mut self, runtime: &CodingMcpRuntime, session: usize) -> Vec<String> {
        self.discover_tools(runtime, session)
            .await
            .iter()
            .map(|tool| tool.schema().name().to_owned())
            .collect()
    }

    async fn discover_tools(
        &mut self,
        runtime: &CodingMcpRuntime,
        session: usize,
    ) -> Vec<Arc<dyn Tool>> {
        let mut discovery = Box::pin(model_tools(runtime));
        tokio::select! {
            peer = self.listing(session) => drop(peer),
            _ = &mut discovery => panic!("unexpected cached discovery"),
        }
        self.release(session, 1);
        discovery.await
    }

    async fn listing(&mut self, session: usize) -> Peer<RoleServer> {
        let (index, peer) = tokio::time::timeout(Duration::from_secs(4), self.requested.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(index, session);
        peer
    }

    fn release(&self, session: usize, count: usize) {
        self.gates[session - 1].add_permits(count);
    }

    async fn close(self, runtime: &CodingMcpRuntime) {
        runtime.shutdown().await.unwrap();
        self.cancellation.cancel();
        self.task.await.unwrap();
    }
}

async fn names(runtime: &CodingMcpRuntime) -> Vec<String> {
    model_tools(runtime)
        .await
        .iter()
        .map(|tool| tool.schema().name().to_owned())
        .collect()
}

fn context() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("catalog-test"))
        .name("Catalog")
        .build()
        .unwrap();
    RunContext::new(RunId::new("catalog-test"), &agent)
}

async fn model_tools(runtime: &CodingMcpRuntime) -> Vec<Arc<dyn Tool>> {
    let context = context();
    runtime
        .get_tools(&context, &BTreeSet::new())
        .await
        .unwrap()
        .into_iter()
        .filter(|tool| tool.schema().name().starts_with("session_"))
        .collect()
}

async fn invoke(tool: &dyn Tool) -> Result<ToolOutput> {
    tool.call(ToolContext::new(
        &context(),
        tool,
        &CallId::new("catalog-call"),
        &json!({}),
    ))
    .await
}

#[tokio::test]
async fn catalog_serves_equivalent_reconnect_but_never_crosses_credentials() {
    let mut fixture = Fixture::start(None).await;
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);

    fixture.pause_initialize(2);
    runtime.replace_mcp_servers(Vec::new()).await.unwrap();
    runtime
        .replace_mcp_servers(fixture.registrations(false))
        .await
        .unwrap();
    fixture.wait_for_initialize(2).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), names(&runtime))
            .await
            .unwrap(),
        ["session_1"]
    );
    fixture.resume_initialize(2);
    wait_status(&runtime, "docs", Status::Connected).await;
    assert_eq!(fixture.discover(&runtime, 2).await, ["session_2"]);

    fixture.pause_initialize(3);
    runtime
        .replace_mcp_servers(fixture.registrations(true))
        .await
        .unwrap();
    fixture.wait_for_initialize(3).await;
    let mut current = Box::pin(names(&runtime));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut current)
            .await
            .is_err()
    );
    fixture.resume_initialize(3);
    tokio::select! {
        _ = fixture.listing(3) => {}
        tools = &mut current => panic!("unexpected cached discovery: {tools:?}"),
    }
    fixture.release(3, 1);
    assert_eq!(current.await, ["session_3"]);
    fixture.close(&runtime).await;
}

async fn capture_startup_cached_tool(
    fixture: &mut Fixture,
    runtime: &CodingMcpRuntime,
) -> Arc<dyn Tool> {
    assert_eq!(fixture.discover(runtime, 1).await, ["session_1"]);
    fixture.pause_initialize(2);
    runtime.replace_mcp_servers(Vec::new()).await.unwrap();
    runtime
        .replace_mcp_servers(fixture.registrations(false))
        .await
        .unwrap();
    fixture.wait_for_initialize(2).await;
    let tools = tokio::time::timeout(Duration::from_secs(1), model_tools(runtime))
        .await
        .expect("the startup cache should advertise tools before connection readiness");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].schema().name(), "session_1");
    tools[0].clone()
}

#[tokio::test]
async fn cached_product_call_uses_live_schema_and_read_only_approval() {
    let mut fixture = Fixture::start(None).await;
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let cached = capture_startup_cached_tool(&mut fixture, &runtime).await;
    *fixture.live_tools.lock().unwrap() = Some(vec![
        serde_json::from_value(json!({
            "name": "session_1",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            },
            "annotations": {"readOnlyHint": true}
        }))
        .unwrap(),
    ]);

    let mut invalid = Box::pin(invoke(cached.as_ref()));
    assert!(futures::poll!(&mut invalid).is_pending());
    fixture.resume_initialize(2);
    tokio::select! {
        _ = fixture.listing(2) => {}
        result = &mut invalid => panic!("the call completed before reading the live catalog: {result:?}"),
    }
    fixture.release(2, 1);
    let error = tokio::time::timeout(Duration::from_secs(2), invalid)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing required parameters: query"),
        "{error}"
    );
    assert!(fixture.calls.try_recv().is_err());

    let context = context();
    let call_id = CallId::new("cached-read");
    let arguments = json!({"query": "rust"});
    tokio::time::timeout(
        Duration::from_secs(2),
        cached.call(ToolContext::new(
            &context,
            cached.as_ref(),
            &call_id,
            &arguments,
        )),
    )
    .await
    .unwrap()
    .expect("the live read-only declaration requires no approval");
    assert_eq!(fixture.calls.try_recv().unwrap(), "session_1");
    assert!(fixture.requested.try_recv().is_err());
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn cached_and_live_product_calls_validate_required_arguments_after_approval() {
    let mut fixture = Fixture::start(None).await;
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let cached = capture_startup_cached_tool(&mut fixture, &runtime).await;
    *fixture.live_tools.lock().unwrap() = Some(vec![
        serde_json::from_value(json!({
            "name": "session_1",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            },
            "annotations": {"destructiveHint": true}
        }))
        .unwrap(),
    ]);
    fixture.resume_initialize(2);
    wait_status(&runtime, "docs", Status::Connected).await;
    let live = fixture.discover_tools(&runtime, 2).await.remove(0);
    let run = context();
    let arguments = json!({});
    for (tool, call_id) in [(cached, "cached-write"), (live, "live-write")] {
        let call_id = CallId::new(call_id);
        let context = || ToolContext::new(&run, tool.as_ref(), &call_id, &arguments);
        let error = tool.call(context()).await.unwrap_err();
        let approval = ToolApprovalRequest::of(&error)
            .unwrap_or_else(|| panic!("expected an approval request, got {error}"));
        assert!(fixture.calls.try_recv().is_err());

        let error = tool
            .call(context().with_resume_state(approval.state()))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing required parameters: query")
        );
        assert!(fixture.calls.try_recv().is_err());
    }
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn cached_product_call_leaves_catalog_notifications_for_discovery() {
    let mut fixture = Fixture::start(None).await;
    let (_home, _services, runtime, _binding) = bind_refresh(&fixture, true).await;
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);

    fixture.pause_initialize(2);
    let reconnect = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .reconnect_mcp_server(Some("docs"), false)
                .await
                .unwrap()
        }
    });
    fixture.wait_for_initialize(2).await;
    let cached = tokio::time::timeout(Duration::from_secs(1), model_tools(&runtime))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(cached.schema().name(), "session_1");
    *fixture.live_tools.lock().unwrap() = Some(vec![
        serde_json::from_value(json!({
            "name": "session_1",
            "inputSchema": {"type": "object"},
            "annotations": {"readOnlyHint": true}
        }))
        .unwrap(),
    ]);
    fixture.resume_initialize(2);
    reconnect.await.unwrap();
    assert_eq!(fixture.discover(&runtime, 2).await, ["session_1"]);

    *fixture.live_tools.lock().unwrap() = None;
    runtime
        .call_tool_now(
            "docs",
            "notify",
            None,
            None,
            None,
            Some(Duration::from_secs(2)),
        )
        .await
        .unwrap();

    let mut call = Box::pin(invoke(cached.as_ref()));
    tokio::select! {
        biased;
        _ = fixture.listing(2) => panic!("call preparation tried to refresh the live catalog"),
        result = &mut call => result.unwrap(),
    };
    assert_eq!(fixture.calls.try_recv().unwrap(), "session_1");

    assert_eq!(fixture.discover(&runtime, 2).await, ["session_2"]);
    let error = invoke(cached.as_ref()).await.unwrap_err();
    assert!(error.to_string().contains("not available to the model"));
    assert!(fixture.calls.try_recv().is_err());
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn cached_product_call_rejects_a_removed_tool_before_dispatch() {
    let mut fixture = Fixture::start(None).await;
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let cached = capture_startup_cached_tool(&mut fixture, &runtime).await;
    fixture.resume_initialize(2);
    let mut call = Box::pin(invoke(cached.as_ref()));
    tokio::select! {
        _ = fixture.listing(2) => {}
        result = &mut call => panic!("the call completed before reading the live catalog: {result:?}"),
    }
    fixture.release(2, 1);
    let error = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("MCP tool `docs/session_1` is not available to the model"),
        "{error}"
    );
    assert!(fixture.calls.try_recv().is_err());
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn cached_product_call_preserves_startup_failure_before_listing() {
    let mut fixture = Fixture::start(None).await;
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let cached = capture_startup_cached_tool(&mut fixture, &runtime).await;
    fixture.failing_session.store(2, Ordering::SeqCst);
    let mut call = Box::pin(invoke(cached.as_ref()));
    assert!(futures::poll!(&mut call).is_pending());
    fixture.resume_initialize(2);
    let error = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("MCP startup failed"), "{error}");
    assert!(fixture.requested.try_recv().is_err());
    assert!(fixture.calls.try_recv().is_err());
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn live_approval_can_resume_through_startup_cache_without_a_declaration_digest() {
    let mut fixture = Fixture::start(None).await;
    *fixture.live_tools.lock().unwrap() = Some(vec![
        serde_json::from_value(json!({
            "name": "session_1",
            "inputSchema": {"type": "object"},
            "annotations": {"destructiveHint": true}
        }))
        .unwrap(),
    ]);
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let live = fixture.discover_tools(&runtime, 1).await.remove(0);
    let context = context();
    let call_id = CallId::new("live-write");
    let arguments = json!({});
    let error = live
        .call(ToolContext::new(
            &context,
            live.as_ref(),
            &call_id,
            &arguments,
        ))
        .await
        .unwrap_err();
    let approval = ToolApprovalRequest::of(&error).expect("the live declaration requires approval");
    assert!(approval.state().get("declaration").is_none());
    assert!(fixture.calls.try_recv().is_err());

    fixture.pause_initialize(2);
    let reconnect = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .reconnect_mcp_server(Some("docs"), false)
                .await
                .unwrap()
        }
    });
    fixture.wait_for_initialize(2).await;
    let cached = tokio::time::timeout(Duration::from_secs(1), model_tools(&runtime))
        .await
        .expect("the tool should be available from the startup cache")
        .remove(0);
    assert_eq!(cached.schema().name(), "session_1");
    let mut resumed = Box::pin(
        cached.call(
            ToolContext::new(&context, cached.as_ref(), &call_id, &arguments)
                .with_resume_state(approval.state()),
        ),
    );
    assert!(futures::poll!(&mut resumed).is_pending());
    fixture.resume_initialize(2);
    tokio::select! {
        _ = fixture.listing(2) => {}
        result = &mut resumed => panic!("the call completed before reading the live catalog: {result:?}"),
    }
    fixture.release(2, 1);
    tokio::time::timeout(Duration::from_secs(2), resumed)
        .await
        .unwrap()
        .expect("a valid live approval can be resumed from a startup cache");
    assert_eq!(fixture.calls.try_recv().unwrap(), "session_1");
    assert!(fixture.calls.try_recv().is_err());
    reconnect.await.unwrap();
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn approval_for_a_cached_call_does_not_authorize_a_changed_live_declaration() {
    let mut fixture = Fixture::start(None).await;
    let (_home, _services, runtime, _binding) = bind_refresh(&fixture, true).await;
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);
    fixture.pause_initialize(2);
    let reconnect = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .reconnect_mcp_server(Some("docs"), false)
                .await
                .unwrap()
        }
    });
    fixture.wait_for_initialize(2).await;
    let cached = tokio::time::timeout(Duration::from_secs(1), model_tools(&runtime))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(cached.schema().name(), "session_1");
    let declaration = |description: &str| {
        serde_json::from_value(json!({
            "name": "session_1",
            "description": description,
            "inputSchema": {"type": "object"},
            "annotations": {"destructiveHint": true}
        }))
        .unwrap()
    };
    *fixture.live_tools.lock().unwrap() = Some(vec![declaration("original")]);
    fixture.resume_initialize(2);
    reconnect.await.unwrap();

    let context = context();
    let call_id = CallId::new("cached-write");
    let arguments = json!({});
    let mut pending = Box::pin(cached.call(ToolContext::new(
        &context,
        cached.as_ref(),
        &call_id,
        &arguments,
    )));
    tokio::select! {
        _ = fixture.listing(2) => {}
        result = &mut pending => panic!("approval was decided before reading the live declaration: {result:?}"),
    }
    fixture.release(2, 1);
    let error = pending.await.unwrap_err();
    let approval = ToolApprovalRequest::of(&error).expect("the live declaration requires approval");
    assert_eq!(approval.state()["description"], "original");
    assert!(fixture.calls.try_recv().is_err());

    *fixture.live_tools.lock().unwrap() = Some(vec![declaration("changed")]);
    runtime
        .call_tool_now(
            "docs",
            "notify",
            None,
            None,
            None,
            Some(Duration::from_secs(2)),
        )
        .await
        .unwrap();
    let current = fixture.discover_tools(&runtime, 2).await.remove(0);
    let error = current
        .call(
            ToolContext::new(&context, current.as_ref(), &call_id, &arguments)
                .with_resume_state(approval.state()),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("changed after approval was requested"),
        "{error}"
    );
    assert!(fixture.calls.try_recv().is_err());
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn server_opt_out_discards_the_previous_catalog() {
    let mut fixture = Fixture::start(Some(2)).await;
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);
    runtime.replace_mcp_servers(Vec::new()).await.unwrap();
    runtime
        .replace_mcp_servers(fixture.registrations(false))
        .await
        .unwrap();
    wait_status(&runtime, "docs", Status::Connected).await;
    assert_eq!(fixture.discover(&runtime, 2).await, ["session_2"]);

    fixture.pause_initialize(3);
    runtime.replace_mcp_servers(Vec::new()).await.unwrap();
    runtime
        .replace_mcp_servers(fixture.registrations(false))
        .await
        .unwrap();
    fixture.wait_for_initialize(3).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), names(&runtime))
            .await
            .is_err()
    );
    fixture.resume_initialize(3);
    assert_eq!(fixture.discover(&runtime, 3).await, ["session_3"]);
    fixture.close(&runtime).await;
}

async fn bind_refresh(
    fixture: &Fixture,
    enabled: bool,
) -> (
    TempDir,
    Arc<PluginServices>,
    Arc<CodingMcpRuntime>,
    ra_coding::plugins::ConsumerBinding,
) {
    let home = TempDir::new().unwrap();
    let features = if enabled {
        "[features]\nmcp_tool_list_changed = true\n"
    } else {
        ""
    };
    write(
        &home.path().join("config.toml"),
        &format!(
            "{features}[mcp_servers.docs]\nurl = {}\nstartup_readiness = \"catalog\"\nstartup_timeout_sec = 1\n",
            toml_string(&fixture.url)
        ),
    );
    let services = services(home.path());
    let runtime = CodingMcpRuntime::new(Vec::new(), McpDiscoveryOptions::new()).unwrap();
    let binding = services.bind_mcp_runtime(&runtime, absolute(home.path()));
    let outcome = binding.wait_for(services.generation()).await.unwrap();
    assert_eq!(outcome.error(), None);
    runtime.connect_all().await.unwrap();
    (home, services, runtime, binding)
}

#[tokio::test]
async fn notifications_during_discovery_and_refresh_relist_without_reconnecting() {
    let mut fixture = Fixture::start(None).await;
    let (_home, _services, runtime, _binding) = bind_refresh(&fixture, true).await;
    let mut readers = Box::pin(async { tokio::join!(names(&runtime), names(&runtime)) });
    let peer = tokio::select! {
        peer = fixture.listing(1) => peer,
        tools = &mut readers => panic!("unexpected catalog: {tools:?}"),
    };
    fixture.revision.store(1, Ordering::SeqCst);
    announce_tool_list_change(&peer).await;
    fixture.release(1, 1);
    tokio::select! {
        _ = fixture.listing(1) => {}
        tools = &mut readers => panic!("initial notification was missed: {tools:?}"),
    }
    fixture.revision.store(2, Ordering::SeqCst);
    announce_tool_list_change(&peer).await;
    fixture.release(1, 1);
    tokio::select! {
        _ = fixture.listing(1) => {}
        tools = &mut readers => panic!("catalog changed during refresh: {tools:?}"),
    }
    fixture.release(1, 1);

    let (first, second) = readers.await;
    assert_eq!(first, ["session_1_revision_2"]);
    assert_eq!(first, second);
    assert_eq!(names(&runtime).await, first);
    assert!(fixture.requested.try_recv().is_err());
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn changed_catalog_rejects_prepared_calls_but_equivalent_catalogs_do_not() {
    let mut fixture = Fixture::start(None).await;
    let (_home, _services, runtime, _binding) = bind_refresh(&fixture, true).await;
    let original = fixture.discover_tools(&runtime, 1).await;
    let old_tool = original[0].as_ref();
    let notify = || {
        runtime.call_tool_now(
            "docs",
            "notify",
            None,
            None,
            None,
            Some(Duration::from_secs(2)),
        )
    };

    notify().await.unwrap();
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);
    tokio::time::timeout(Duration::from_secs(2), invoke(old_tool))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.calls.try_recv().unwrap(), "session_1");

    fixture.revision.store(1, Ordering::SeqCst);
    notify().await.unwrap();
    let updated = fixture.discover_tools(&runtime, 1).await;
    assert_eq!(updated[0].schema().name(), "session_1_revision_1");

    let error = tokio::time::timeout(Duration::from_secs(2), invoke(old_tool))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains(
        "tool call rejected because the catalog changed after `docs/session_1` was prepared"
    ));
    assert!(fixture.calls.try_recv().is_err());

    tokio::time::timeout(Duration::from_secs(2), invoke(updated[0].as_ref()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.calls.try_recv().unwrap(), "session_1_revision_1");
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn reordered_equivalent_catalog_keeps_prepared_calls() {
    let mut fixture = Fixture::start(None).await;
    fixture.layout.store(COMPANION_AFTER, Ordering::SeqCst);
    let (_home, _services, runtime, _binding) = bind_refresh(&fixture, true).await;
    let original = fixture.discover_tools(&runtime, 1).await;
    let old_tool = original[0].as_ref();

    // The refetched catalog has the same definitions in another order.
    fixture.layout.store(COMPANION_BEFORE, Ordering::SeqCst);
    runtime
        .call_tool_now(
            "docs",
            "notify",
            None,
            None,
            None,
            Some(Duration::from_secs(2)),
        )
        .await
        .unwrap();
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);
    tokio::time::timeout(Duration::from_secs(2), invoke(old_tool))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.calls.try_recv().unwrap(), "session_1");
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn failed_refresh_keeps_the_last_listing_and_coalesces_retries() {
    let mut fixture = Fixture::start(None).await;
    let (_home, _services, runtime, _binding) = bind_refresh(&fixture, true).await;
    let mut discovery = Box::pin(names(&runtime));
    let peer = tokio::select! {
        peer = fixture.listing(1) => peer,
        tools = &mut discovery => panic!("unexpected catalog: {tools:?}"),
    };
    fixture.release(1, 1);
    assert_eq!(discovery.await, ["session_1"]);

    let mut refresh = Box::pin(names(&runtime));
    fixture.revision.store(1, Ordering::SeqCst);
    announce_tool_list_change(&peer).await;
    // Wait for delivery through the stream before reading the catalog.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                _ = fixture.listing(1) => break,
                tools = &mut refresh => {
                    assert_eq!(tools, ["session_1"]);
                    tokio::task::yield_now().await;
                    refresh.set(names(&runtime));
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(refresh.await, ["session_1"]);
    assert_eq!(names(&runtime).await, ["session_1"]);
    assert!(fixture.requested.try_recv().is_err());
    fixture.release(1, 1);
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn enabling_notifications_on_reload_waits_for_a_tracked_catalog() {
    let mut fixture = Fixture::start(None).await;
    let (home, services, runtime, binding) = bind_refresh(&fixture, false).await;
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);

    let config_path = home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    write(
        &config_path,
        &format!("{config}\n[features]\nmcp_tool_list_changed = true\n"),
    );
    fixture.pause_initialize(2);
    services.invalidate();
    let outcome = binding.wait_for(services.generation()).await.unwrap();
    assert_eq!(outcome.error(), None);
    fixture.wait_for_initialize(2).await;
    let mut discovery = Box::pin(names(&runtime));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut discovery)
            .await
            .is_err(),
        "refresh-enabled startup must wait for the new connection"
    );
    fixture.resume_initialize(2);
    tokio::select! {
        _ = fixture.listing(2) => {}
        tools = &mut discovery => panic!("unexpected cached discovery: {tools:?}"),
    }
    fixture.release(2, 1);
    assert_eq!(discovery.await, ["session_2"]);
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn replacing_a_connection_cancels_its_notification_refresh() {
    let mut fixture = Fixture::start(None).await;
    let (home, services, runtime, binding) = bind_refresh(&fixture, true).await;
    let mut discovery = Box::pin(names(&runtime));
    let peer = tokio::select! {
        peer = fixture.listing(1) => peer,
        tools = &mut discovery => panic!("unexpected catalog: {tools:?}"),
    };
    announce_tool_list_change(&peer).await;
    fixture.release(1, 1);
    tokio::select! {
        _ = fixture.listing(1) => {}
        tools = &mut discovery => panic!("initial notification was missed: {tools:?}"),
    }
    let config_path = home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    write(
        &config_path,
        &format!("{config}http_headers = {{ \"X-Test-Connection\" = \"replacement\" }}\n"),
    );
    services.invalidate();
    let (tools, outcome) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(&mut discovery, binding.wait_for(services.generation()))
    })
    .await
    .unwrap();
    assert!(tools.is_empty());
    assert_eq!(outcome.unwrap().error(), None);

    fixture.release(1, 1);
    assert_eq!(fixture.discover(&runtime, 2).await, ["session_2"]);
    assert_eq!(names(&runtime).await, ["session_2"]);
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn startup_catalog_invalidation_is_gated_by_the_feature() {
    for enabled in [false, true] {
        let mut fixture = Fixture::start(None).await;
        let (home, services, runtime, binding) = bind_refresh(&fixture, enabled).await;
        let mut discovery = Box::pin(names(&runtime));
        let peer = tokio::select! {
            peer = fixture.listing(1) => peer,
            tools = &mut discovery => panic!("unexpected catalog: {tools:?}"),
        };
        announce_tool_list_change(&peer).await;
        fixture.release(1, 1);
        if enabled {
            tokio::select! {
                _ = fixture.listing(1) => {}
                tools = &mut discovery => panic!("initial notification was missed: {tools:?}"),
            }
            fixture.release(1, 1);
        }
        assert_eq!(discovery.await, ["session_1"]);

        fixture.pause_initialize(2);
        let config_path = home.path().join("config.toml");
        let config = std::fs::read_to_string(&config_path).unwrap();
        write(&config_path, "");
        services.invalidate();
        binding.wait_for(services.generation()).await.unwrap();
        write(&config_path, &config);
        services.invalidate();
        // A paused handshake can delay the config reconciliation, so let tool discovery and
        // configuration observe the newly published entry concurrently.
        fixture.wait_for_initialize(2).await;
        if enabled {
            assert!(
                tokio::time::timeout(Duration::from_millis(50), names(&runtime))
                    .await
                    .is_err()
            );
        } else {
            assert_eq!(names(&runtime).await, ["session_1"]);
        }
        fixture.resume_initialize(2);
        wait_status(&runtime, "docs", Status::Connected).await;
        assert_eq!(fixture.discover(&runtime, 2).await, ["session_2"]);
        fixture.close(&runtime).await;
    }
}

#[tokio::test]
async fn default_connection_lists_once_and_shutdown_cancels_a_stalled_live_listing() {
    let mut fixture = Fixture::start(None).await;
    let runtime = CodingMcpRuntime::new(
        fixture.configured(false, None, OutboundProxyPolicy::ReqwestDefault),
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), runtime.connect_all())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        fixture.requested.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);
    assert!(matches!(
        fixture.requested.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    assert_eq!(names(&runtime).await, ["session_1"]);
    assert!(fixture.requested.try_recv().is_err());
    runtime
        .reconnect_mcp_server(Some("docs"), false)
        .await
        .unwrap();

    let mut discovery = Box::pin(names(&runtime));
    tokio::select! {
        _ = fixture.listing(2) => {}
        tools = &mut discovery => panic!("unexpected catalog: {tools:?}"),
    }
    let (result, tools) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(runtime.shutdown(), &mut discovery)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(tools.is_empty());
    fixture.release(2, 1);
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn first_catalog_uses_reloaded_startup_timeout_on_the_existing_session() {
    use ra_protocol::server::McpRuntimeControl;

    let mut fixture = Fixture::start(None).await;
    let runtime = CodingMcpRuntime::new(
        fixture.configured(false, None, OutboundProxyPolicy::ReqwestDefault),
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    runtime
        .replace_mcp_servers(fixture.registrations_from(
            json!({"url": fixture.url, "startup_timeout_sec": 0.5}),
            OutboundProxyPolicy::ReqwestDefault,
        ))
        .await
        .unwrap();

    let mut discovery = Box::pin(names(&runtime));
    tokio::select! {
        _ = fixture.listing(1) => {}
        tools = &mut discovery => panic!("discovery finished before listing: {tools:?}"),
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(2), discovery)
            .await
            .expect("first discovery should use the reloaded timeout")
            .is_empty()
    );
    let mut inventory = McpRuntimeControl::inventory(runtime.as_ref(), "docs", false);
    tokio::select! {
        _ = fixture.listing(1) => {}
        status = &mut inventory => panic!("inventory finished before listing: {status:?}"),
    }
    let status = tokio::time::timeout(Duration::from_secs(2), inventory)
        .await
        .expect("first inventory should use the reloaded timeout");
    assert!(
        serde_json::to_value(status).unwrap()["toolsError"]
            .as_str()
            .is_some_and(|error| error.contains("timed out"))
    );
    fixture.release(1, 2);
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn queued_first_listing_uses_its_own_deadline_and_preserves_the_startup_error() {
    use ra_protocol::server::McpRuntimeControl;

    let mut fixture = Fixture::start(None).await;
    let runtime = CodingMcpRuntime::new(
        fixture.configured(false, None, OutboundProxyPolicy::ReqwestDefault),
        McpDiscoveryOptions::new(),
    )
    .unwrap();
    runtime.connect_all().await.unwrap();
    runtime
        .replace_mcp_servers(fixture.registrations_from(
            json!({"url": fixture.url, "startup_timeout_sec": 0.5}),
            OutboundProxyPolicy::ReqwestDefault,
        ))
        .await
        .unwrap();
    let mut first = McpRuntimeControl::inventory(runtime.as_ref(), "docs", false);
    tokio::select! {
        _ = fixture.listing(1) => {}
        status = &mut first => panic!("first inventory did not list tools: {status:?}"),
    }

    let mut queued = McpRuntimeControl::inventory(runtime.as_ref(), "docs", false);
    assert!(futures::poll!(&mut queued).is_pending());
    let status = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .unwrap();
    let status = serde_json::to_value(status).unwrap();
    let error = status["toolsError"].as_str().unwrap();
    assert!(
        error.contains("MCP catalog listing timed out") || error.contains("MCP request timed out"),
        "{error}"
    );

    tokio::select! {
        _ = fixture.listing(1) => {}
        status = &mut queued => panic!("queued inventory ran out of time before sending tools/list: {status:?}"),
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    fixture.release(1, 2);
    let status = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .unwrap();
    let status = serde_json::to_value(status).unwrap();
    assert!(status["tools"]["session_1"].is_object(), "{status}");
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn proxy_policy_change_does_not_reenable_a_runtime_disabled_server() {
    let fixture = Fixture::start(None).await;
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    runtime.toggle_mcp_server("docs", false).await.unwrap();
    let statuses = runtime
        .replace_mcp_servers(fixture.configured(
            false,
            Some("catalog"),
            OutboundProxyPolicy::RespectSystemProxy,
        ))
        .await
        .unwrap();
    assert_eq!(statuses[0].runtime_status(), Some(Status::Disabled));
    assert!(names(&runtime).await.is_empty());
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn replacement_cancels_an_old_discovery_and_reuses_only_the_new_catalog() {
    let mut fixture = Fixture::start(None).await;
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let mut discovery = Box::pin(names(&runtime));
    tokio::select! {
        _ = fixture.listing(1) => {}
        tools = &mut discovery => panic!("unexpected catalog: {tools:?}"),
    }
    let (result, tools) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(runtime.replace_mcp_servers(Vec::new()), &mut discovery)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(tools.is_empty());

    runtime
        .replace_mcp_servers(fixture.registrations(false))
        .await
        .unwrap();
    assert_eq!(fixture.discover(&runtime, 2).await, ["session_2"]);
    fixture.release(1, 1);

    fixture.pause_initialize(3);
    runtime.replace_mcp_servers(Vec::new()).await.unwrap();
    runtime
        .replace_mcp_servers(fixture.registrations(false))
        .await
        .unwrap();
    fixture.wait_for_initialize(3).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), names(&runtime))
            .await
            .unwrap(),
        ["session_2"]
    );
    fixture.resume_initialize(3);
    wait_status(&runtime, "docs", Status::Connected).await;
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn bearer_environment_credentials_can_reuse_a_catalog() {
    const TOKEN_VAR: &str = "RA_TEST_MCP_CATALOG_BEARER";
    if std::env::var(TOKEN_VAR).ok().as_deref() != Some("catalog-test-token") {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tool_catalog_cache::bearer_environment_credentials_can_reuse_a_catalog",
            ])
            .env(TOKEN_VAR, "catalog-test-token")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let mut fixture = Fixture::start(None).await;
    let registrations = |fixture: &Fixture| {
        fixture.registrations_from(
            json!({
                "url": fixture.url,
                "bearer_token_env_var": TOKEN_VAR,
                "startup_readiness": "catalog",
            }),
            OutboundProxyPolicy::ReqwestDefault,
        )
    };
    let runtime =
        CodingMcpRuntime::new(registrations(&fixture), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);

    fixture.pause_initialize(2);
    runtime.replace_mcp_servers(Vec::new()).await.unwrap();
    runtime
        .replace_mcp_servers(registrations(&fixture))
        .await
        .unwrap();
    fixture.wait_for_initialize(2).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), names(&runtime))
            .await
            .unwrap(),
        ["session_1"]
    );
    fixture.resume_initialize(2);
    wait_status(&runtime, "docs", Status::Connected).await;
    fixture.close(&runtime).await;
}

#[tokio::test]
async fn default_connection_reuses_its_last_live_catalog_during_reconnect() {
    for opt_out_session in [None, Some(1)] {
        let mut fixture = Fixture::start(opt_out_session).await;
        let runtime = CodingMcpRuntime::new(
            fixture.configured(false, None, OutboundProxyPolicy::ReqwestDefault),
            McpDiscoveryOptions::new(),
        )
        .unwrap();
        runtime.connect_all().await.unwrap();
        assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);

        fixture.pause_initialize(2);
        let mut reconnect = Box::pin(runtime.reconnect_mcp_server(Some("docs"), false));
        tokio::select! {
            () = fixture.wait_for_initialize(2) => {}
            result = &mut reconnect => panic!("reconnect completed before initialize: {result:?}"),
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), names(&runtime))
                .await
                .expect("the existing entry should expose its last live catalog"),
            ["session_1"]
        );
        assert!(matches!(
            fixture.requested.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        fixture.resume_initialize(2);
        reconnect.await.unwrap();
        assert_eq!(fixture.discover(&runtime, 2).await, ["session_2"]);
        fixture.close(&runtime).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn header_helper_catalog_does_not_cross_connection_entries() {
    let mut fixture = Fixture::start(None).await;
    let registrations = |fixture: &Fixture| {
        fixture.registrations_from(
            json!({
                "url": fixture.url,
                "http_headers_helper": "printf '{}'",
                "startup_readiness": "catalog",
            }),
            OutboundProxyPolicy::ReqwestDefault,
        )
    };
    let runtime =
        CodingMcpRuntime::new(registrations(&fixture), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    assert_eq!(fixture.discover(&runtime, 1).await, ["session_1"]);

    fixture.pause_initialize(2);
    runtime.replace_mcp_servers(Vec::new()).await.unwrap();
    runtime
        .replace_mcp_servers(registrations(&fixture))
        .await
        .unwrap();
    fixture.wait_for_initialize(2).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), names(&runtime))
            .await
            .is_err()
    );
    fixture.resume_initialize(2);
    assert_eq!(fixture.discover(&runtime, 2).await, ["session_2"]);
    fixture.close(&runtime).await;
}
