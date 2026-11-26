//! Cross-connection catalog reuse through configured transports.
use super::*;
use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    state::RunId,
    tool::ToolResolver,
};
use ra_mcp::{McpCatalogBuilder, McpServerRegistration};
use rmcp::{
    RoleServer, ServerHandler,
    model::{
        InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerConfig,
    },
    service::{Peer, RequestContext},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicUsize, Ordering},
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
}

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
        context.peer.set_peer_info(params.clone());
        self.negotiate_initialize(&params)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        self.requested
            .send((self.session, context.peer.clone()))
            .unwrap();
        self.gates[self.session - 1]
            .acquire()
            .await
            .unwrap()
            .forget();
        Ok(ListToolsResult {
            tools: vec![
                serde_json::from_value(json!({
                    "name": format!("session_{}", self.session),
                    "inputSchema": {"type": "object"},
                    "annotations": {"readOnlyHint": true}
                }))
                .unwrap(),
            ],
            ..Default::default()
        })
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
}

impl Fixture {
    async fn start(opt_out_session: Option<usize>) -> Self {
        let cancellation = CancellationToken::new();
        let gates = Arc::new((0..4).map(|_| Semaphore::new(0)).collect::<Vec<_>>());
        let (sender, requested) = mpsc::unbounded_channel();
        let initialize_gates = Arc::new((0..4).map(|_| Semaphore::new(1)).collect::<Vec<_>>());
        let (initializing_sender, initializing) = mpsc::unbounded_channel();
        let sessions = AtomicUsize::new(0);
        let service = StreamableHttpService::new(
            {
                let gates = gates.clone();
                let initialize_gates = initialize_gates.clone();
                move || {
                    Ok(StartupCatalog {
                        session: sessions.fetch_add(1, Ordering::SeqCst) + 1,
                        gates: gates.clone(),
                        requested: sender.clone(),
                        opt_out_session,
                        initialize_gates: initialize_gates.clone(),
                        initializing: initializing_sender.clone(),
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
        let mut discovery = Box::pin(names(runtime));
        tokio::select! {
            peer = self.listing(session) => drop(peer),
            tools = &mut discovery => panic!("unexpected cached discovery: {tools:?}"),
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
    let agent = AgentSpec::builder()
        .id(AgentId::new("catalog-test"))
        .name("Catalog")
        .build()
        .unwrap();
    let context = RunContext::new(RunId::new("catalog-test"), &agent);
    runtime
        .get_tools(&context, &BTreeSet::new())
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.schema().name().to_owned())
        .filter(|name| name.starts_with("session_"))
        .collect()
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

#[tokio::test]
async fn list_change_during_discovery_makes_its_catalog_ineligible_for_the_next_connection() {
    let mut fixture = Fixture::start(None).await;
    let runtime =
        CodingMcpRuntime::new(fixture.registrations(false), McpDiscoveryOptions::new()).unwrap();
    runtime.connect_all().await.unwrap();
    let mut discovery = Box::pin(names(&runtime));
    let peer = tokio::select! {
        peer = fixture.listing(1) => peer,
        tools = &mut discovery => panic!("unexpected catalog: {tools:?}"),
    };
    peer.notify_tool_list_changed().await.unwrap();
    fixture.release(1, 1);
    assert_eq!(discovery.await, ["session_1"]);

    fixture.pause_initialize(2);
    runtime.replace_mcp_servers(Vec::new()).await.unwrap();
    runtime
        .replace_mcp_servers(fixture.registrations(false))
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

    let mut discovery = Box::pin(names(&runtime));
    tokio::select! {
        _ = fixture.listing(1) => {}
        tools = &mut discovery => panic!("unexpected catalog: {tools:?}"),
    }
    let (result, tools) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(runtime.shutdown(), &mut discovery)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(tools.is_empty());
    fixture.release(1, 1);
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
