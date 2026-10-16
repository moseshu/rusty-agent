//! Real protocol round trips through all supported transports.
#[path = "../support/mod.rs"]
mod support;
use ra_core::tool::{Tool, ToolConcurrency, ToolExposure, ToolFailureHandling};
use ra_mcp::{
    client::{McpClient, McpClientOptions, http::McpHttpParams, stdio::McpStdioParams},
    filter::ToolFilter,
    in_process::InProcessToolServer,
    tool::{McpDiscoveryOptions, McpToolNaming, discover_tools},
};
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::json;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

async fn roundtrip(client: McpClient) {
    let prompts = client.list_prompts().await.unwrap();
    assert_eq!(prompts.prompts.len(), 2);
    assert!(prompts.next_cursor.is_none());
    assert_eq!(
        client
            .list_resources(None)
            .await
            .unwrap()
            .next_cursor
            .as_deref(),
        Some("")
    );
    assert!(client.read_resource("memo://fixture/note").await.is_ok());
    let tools = discover_tools(
        &[(client.clone(), ToolFilter::default())],
        &McpDiscoveryOptions::new(),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 2);
    let tool = tools
        .iter()
        .find(|t| t.info().tool().name == "search")
        .unwrap();
    // Python's default: the original MCP name as a bare function name.
    assert_eq!(tool.schema().name(), "search");
    assert!(tool.origin().namespace().is_none());
    assert_eq!(
        tool.invoke(json!({"query":"hello"}))
            .await
            .unwrap()
            .as_text(),
        Some(format!("{}:search:hello", client.server_name()).as_str())
    );
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
    assert_eq!(tool.options().exposure(), ToolExposure::Advertised);
    assert_eq!(
        tool.options().failure_handling(),
        ToolFailureHandling::ModelVisible
    );
    assert!(!tool.schema().strict_json_schema());
    assert_eq!(
        tool.schema().input_schema(),
        &json!({"type":"object","properties":{"query":{"type":"string"}}})
    );
    assert!(client.call_tool("search", json!([]), None).await.is_err());
    let absent = client
        .call_tool("argument_state", None, None)
        .await
        .unwrap();
    let object = client
        .call_tool("argument_state", json!({}), None)
        .await
        .unwrap();
    assert!(
        matches!(&absent.content[0], rmcp::model::ContentBlock::Text(text) if text.text == "omitted")
    );
    assert!(
        matches!(&object.content[0], rmcp::model::ContentBlock::Text(text) if text.text == "object")
    );
    client.cleanup().await.unwrap();
    client.cleanup().await.unwrap();
    assert!(tool.invoke(json!({})).await.is_err());
}
#[tokio::test]
async fn in_process_initialize_paginate_call_and_cleanup() {
    let mut server = InProcessToolServer::connect(
        "memory",
        support::Fixture::new("memory"),
        McpClientOptions::default(),
    )
    .await
    .unwrap();
    roundtrip(server.client()).await;
    server.cleanup().await.unwrap();
}
#[tokio::test]
async fn stdio_initialize_paginate_call_and_cleanup() {
    let client = McpClient::connect_stdio(
        "stdio",
        McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture")),
        McpClientOptions::default(),
    )
    .await
    .unwrap();
    roundtrip(client).await;
}
#[tokio::test]
async fn stdio_environment_overrides_and_cwd_reach_the_child() {
    let cwd = std::env::temp_dir().canonicalize().unwrap();
    let client = McpClient::connect_stdio(
        "stdio",
        McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture"))
            .with_cwd(cwd.clone())
            .with_env(std::collections::BTreeMap::from([(
                "MCP_FIXTURE_VALUE".into(),
                "literal $value".into(),
            )])),
        McpClientOptions::default(),
    )
    .await
    .unwrap();
    let result = client.call_tool("env", json!({}), None).await.unwrap();
    let value: serde_json::Value = serde_json::from_str(&match &result.content[0] {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    })
    .unwrap();
    assert_eq!(value["cwd"], cwd.to_str().unwrap());
    assert_eq!(value["value"], "literal $value");
    assert!(value["path"].is_string());
    client.cleanup().await.unwrap();
}
#[tokio::test]
async fn streamable_http_initialize_paginate_call_and_cleanup() {
    http_roundtrip(false).await;
    http_roundtrip(true).await;
}
async fn http_roundtrip(bearer_env: bool) {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    let token = tokio_util::sync::CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = token.clone();
    let service = StreamableHttpService::new(
        || Ok(support::Fixture::new("http")),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let checked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = checked.clone();
    // Read an existing environment value so this test never mutates process-global environment.
    let expected_bearer = if bearer_env {
        format!("Bearer {}", std::env::var("PATH").unwrap())
    } else {
        "Bearer fixture-token".to_owned()
    };
    let app = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let observed = observed.clone();
                let expected_bearer = expected_bearer.clone();
                async move {
                    assert_eq!(request.headers()["authorization"], expected_bearer);
                    assert_eq!(request.headers()["x-fixture"], "present");
                    observed.fetch_add(1, Ordering::SeqCst);
                    next.run(request).await
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = token.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    let mut params = McpHttpParams::new(format!("http://{address}/mcp")).with_headers(
        std::collections::BTreeMap::from([
            ("Authorization".into(), "Bearer fixture-token".into()),
            ("x-fixture".into(), "present".into()),
        ]),
    );
    if bearer_env {
        params = params.with_bearer_token_env_var("PATH");
    }
    let client = McpClient::connect_http("http", params, McpClientOptions::default())
        .await
        .unwrap();
    roundtrip(client).await;
    assert!(checked.load(Ordering::SeqCst) >= 4);
    token.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn same_named_tools_on_colliding_servers_route_to_their_original_server() {
    let mut a = InProcessToolServer::connect(
        "a-b",
        support::Fixture::new("first"),
        McpClientOptions::default(),
    )
    .await
    .unwrap();
    let mut b = InProcessToolServer::connect(
        "a_b",
        support::Fixture::new("second"),
        McpClientOptions::default(),
    )
    .await
    .unwrap();
    let error = discover_tools(
        &[
            (a.client(), ToolFilter::default()),
            (b.client(), ToolFilter::default()),
        ],
        &McpDiscoveryOptions::new(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("Duplicate tool names found across MCP servers: 'first-page', 'search'")
    );
    for naming in [McpToolNaming::ServerPrefixed, McpToolNaming::Namespaced] {
        let tools = discover_tools(
            &[
                (a.client(), ToolFilter::default()),
                (b.client(), ToolFilter::default()),
            ],
            &McpDiscoveryOptions::new()
                .with_naming(naming)
                .with_tool_search(true),
        )
        .await
        .unwrap();
        let search: Vec<_> = tools
            .iter()
            .filter(|t| t.info().tool().name == "search")
            .collect();
        assert_eq!(search.len(), 2);
        assert_ne!(
            search[0].origin().lookup_key(),
            search[1].origin().lookup_key()
        );
        assert_eq!(
            search[0].origin().namespace().is_some(),
            naming == McpToolNaming::Namespaced
        );
        for tool in &search {
            let label = if tool.info().server_name() == "a-b" {
                "first"
            } else {
                "second"
            };
            assert_eq!(tool.options().exposure(), ToolExposure::Deferred);
            assert_eq!(
                tool.invoke(json!({})).await.unwrap().as_text(),
                Some(format!("{label}:search:").as_str())
            );
        }
    }
    a.cleanup().await.unwrap();
    b.cleanup().await.unwrap();
}
#[tokio::test]
async fn pagination_rejects_cycles_and_all_catalog_limits() {
    for (mode, expected, count) in [
        ("cycle", "repeated", 2),
        ("pages", "100 pages", 100),
        ("overflow", "2048 items", 1),
        ("across_pages", "2048 items", 2),
        ("oversized_cursor", "65536", 1),
    ] {
        let mut fixture = support::Fixture::new("limits");
        fixture.mode = mode;
        let requests = fixture.pages.clone();
        let mut server = InProcessToolServer::connect(
            "limits",
            fixture,
            McpClientOptions::default().with_codex_catalog_limits(),
        )
        .await
        .unwrap();
        assert!(
            server
                .client()
                .list_tools()
                .await
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
        assert_eq!(requests.load(Ordering::SeqCst), count);
        server.cleanup().await.unwrap();
    }
}
#[tokio::test]
async fn tools_list_has_one_overall_deadline() {
    let mut fixture = support::Fixture::new("slow");
    fixture.mode = "slow_list";
    let mut server = InProcessToolServer::connect(
        "slow",
        fixture,
        McpClientOptions::default()
            .with_session_timeout(None)
            .with_catalog_timeout(Duration::from_millis(30)),
    )
    .await
    .unwrap();
    assert_eq!(
        server.client().list_tools().await.unwrap_err().code(),
        "tool.timeout"
    );
    server.cleanup().await.unwrap();
}
#[tokio::test]
async fn cancellation_and_timeout_notify_the_server_without_closing_the_session() {
    for timeout in [false, true] {
        let fixture = support::Fixture::new("cancel");
        let entered = fixture.entered.clone();
        let cancelled = fixture.cancelled.clone();
        let options = if timeout {
            McpClientOptions::default()
                .with_session_timeout(None)
                .with_tool_timeout(Duration::from_millis(50))
        } else {
            McpClientOptions::default().with_session_timeout(None)
        };
        let mut server = InProcessToolServer::connect("cancel", fixture, options)
            .await
            .unwrap();
        let client = server.client();
        let task = tokio::spawn(async move { client.call_tool("wait", json!({}), None).await });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        if timeout {
            assert_eq!(task.await.unwrap().unwrap_err().code(), "tool.timeout");
        } else {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
            .await
            .unwrap();
        assert!(
            server
                .client()
                .call_tool("search", json!({}), None)
                .await
                .is_ok()
        );
        server.cleanup().await.unwrap();
    }
}
#[tokio::test]
async fn handshake_failure_and_http_errors_do_not_expose_credentials() {
    let error = McpClient::connect_http(
        "broken",
        McpHttpParams::new("http://username:password@127.0.0.1:1/mcp?token=fixture-secret"),
        McpClientOptions::default().with_session_timeout(Duration::from_millis(50)),
    )
    .await
    .err()
    .unwrap();
    let rendered = format!("{error:?} {error}");
    for secret in ["username", "password", "fixture-secret"] {
        assert!(!rendered.contains(secret));
    }
    let missing = McpClient::connect_http(
        "missing",
        McpHttpParams::new("http://127.0.0.1:1/mcp")
            .with_bearer_token_env_var("RUSTY_MCP_MISSING_TOKEN_TEST"),
        McpClientOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert!(missing.to_string().contains("missing or empty"));
}

#[tokio::test]
async fn filtering_visibility_and_annotation_defaults_reach_the_registered_tool() {
    let mut fixture = support::Fixture::new("policy");
    let mut write = support::declaration("write");
    write.annotations = None;
    write.description = None;
    write.title = Some("Write title".into());
    let mut hidden = support::declaration("hidden");
    hidden.meta = Some(serde_json::from_value(json!({"ui":{"visibility":["app"]}})).unwrap());
    fixture.tools_override = Some(vec![write, hidden, support::declaration("blocked")]);
    let mut server = InProcessToolServer::connect("policy", fixture, McpClientOptions::default())
        .await
        .unwrap();
    let filter = ToolFilter::new(
        None,
        std::collections::HashSet::from(["blocked".to_owned()]),
    );
    let tools = discover_tools(
        &[(server.client(), filter)],
        &McpDiscoveryOptions::new().with_tool_search(true),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 2);
    let write = tools
        .iter()
        .find(|t| t.info().tool().name == "write")
        .unwrap();
    let hidden = tools
        .iter()
        .find(|t| t.info().tool().name == "hidden")
        .unwrap();
    assert_eq!(write.options().concurrency(), ToolConcurrency::Exclusive);
    assert_eq!(write.schema().description(), Some("Write title"));
    assert_eq!(hidden.options().exposure(), ToolExposure::Hidden);
    assert!(!hidden.options().is_discoverable());
    assert!(hidden.invoke(json!({})).await.is_ok());
    // One server twice: Python's duplicate-name error, and Codex's server-name identity.
    for naming in [McpToolNaming::Original, McpToolNaming::Namespaced] {
        assert!(
            discover_tools(
                &[
                    (server.client(), ToolFilter::default()),
                    (server.client(), ToolFilter::default())
                ],
                &McpDiscoveryOptions::new().with_naming(naming)
            )
            .await
            .is_err()
        );
    }
    // A JSON-RPC error is the server's own answer and keeps its message, as Python's MCPError.
    let error = server
        .client()
        .call_tool("protocol_error", json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "tool.execution_failed");
    assert!(
        error
            .to_string()
            .contains("MCP server error -32603: server detail")
    );
    let error = server
        .client()
        .call_tool("invalid_params", json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "tool.invalid_input");
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn schemas_get_properties_meta_is_forwarded_and_bad_declarations_are_skipped() {
    let mut fixture = support::Fixture::new("shape");
    let mut bare = support::declaration("bare");
    bare.input_schema = Arc::new(serde_json::from_value(json!({"type":"object"})).unwrap());
    let mut null = support::declaration("null");
    null.input_schema =
        Arc::new(serde_json::from_value(json!({"type":"object","properties":null})).unwrap());
    let mut meta = support::declaration("meta_echo");
    meta.meta = Some(serde_json::from_value(json!({"fixture/static":"declared"})).unwrap());
    // A name the neutral tool contract cannot carry; Codex skips such a declaration.
    let invalid = support::declaration(" padded ");
    fixture.tools_override = Some(vec![bare, null, meta, invalid]);
    let mut server = InProcessToolServer::connect("shape", fixture, McpClientOptions::default())
        .await
        .unwrap();
    let tools = discover_tools(
        &[(server.client(), ToolFilter::default())],
        &McpDiscoveryOptions::new(),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 3);
    for name in ["bare", "null"] {
        let tool = tools.iter().find(|t| t.schema().name() == name).unwrap();
        assert_eq!(
            tool.schema().input_schema(),
            &json!({"type":"object","properties":{}})
        );
    }
    let echo = tools
        .iter()
        .find(|t| t.schema().name() == "meta_echo")
        .unwrap();
    let echoed = echo.invoke(json!({})).await.unwrap();
    assert!(
        echoed
            .as_text()
            .unwrap()
            .contains("\"fixture/static\":\"declared\"")
    );
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn namespaced_tools_describe_their_namespace_with_server_instructions() {
    let mut fixture = support::Fixture::new("instructed");
    fixture.instructions = Some("Fixture server instructions".into());
    let mut server =
        InProcessToolServer::connect("instructed", fixture, McpClientOptions::default())
            .await
            .unwrap();
    assert_eq!(
        server.client().server_instructions(),
        Some("Fixture server instructions")
    );
    let tools = discover_tools(
        &[(server.client(), ToolFilter::default())],
        &McpDiscoveryOptions::new().with_naming(McpToolNaming::Namespaced),
    )
    .await
    .unwrap();
    assert!(tools.iter().all(|tool| {
        tool.origin().namespace().map(|n| n.as_str()) == Some("mcp__instructed")
            && tool.schema().namespace_description() == Some("Fixture server instructions")
    }));
    let bare = discover_tools(
        &[(server.client(), ToolFilter::default())],
        &McpDiscoveryOptions::new(),
    )
    .await
    .unwrap();
    assert!(
        bare.iter()
            .all(|tool| tool.schema().namespace_description().is_none())
    );
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_session_timeout_matches_python() {
    let options = McpClientOptions::default();
    assert_eq!(options.session_timeout(), Some(Duration::from_secs(5)));
    assert_eq!(options.tool_timeout(), Some(Duration::from_secs(5)));
    assert_eq!(
        options
            .with_tool_timeout(Duration::from_secs(300))
            .tool_timeout(),
        Some(Duration::from_secs(300))
    );
}

struct DelayedFixture {
    inner: support::Fixture,
    initialize_delay: Duration,
    request_delay: Duration,
}

impl ServerHandler for DelayedFixture {
    fn get_info(&self) -> ServerConfig {
        self.inner.get_info()
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        tokio::time::sleep(self.initialize_delay).await;
        self.inner.initialize(request, context).await
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        tokio::time::sleep(self.request_delay).await;
        self.inner.list_tools(request, context).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        tokio::time::sleep(self.request_delay).await;
        self.inner.call_tool(request, context).await
    }
}

#[tokio::test(start_paused = true)]
async fn none_and_zero_disable_session_deadlines_for_initialize_list_and_call() {
    for timeout in [None, Some(Duration::ZERO)] {
        let options = McpClientOptions::default().with_session_timeout(timeout);
        assert_eq!(options.session_timeout(), None);
        assert_eq!(options.tool_timeout(), None);
        let mut server = InProcessToolServer::connect(
            "delayed",
            DelayedFixture {
                inner: support::Fixture::new("delayed"),
                initialize_delay: Duration::from_secs(6),
                request_delay: Duration::from_secs(6),
            },
            options,
        )
        .await
        .unwrap();
        assert_eq!(server.client().list_tools().await.unwrap().len(), 2);
        assert!(
            server
                .client()
                .call_tool("search", json!({}), None)
                .await
                .is_ok()
        );
        server.cleanup().await.unwrap();
    }
    assert_eq!(
        McpClientOptions::default()
            .with_session_timeout(Duration::ZERO)
            .session_timeout(),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn positive_session_deadlines_still_bound_initialize_list_and_call() {
    let options = McpClientOptions::default().with_session_timeout(Duration::from_secs(1));
    let error = InProcessToolServer::connect(
        "delayed",
        DelayedFixture {
            inner: support::Fixture::new("delayed"),
            initialize_delay: Duration::from_secs(2),
            request_delay: Duration::ZERO,
        },
        options.clone(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.code(), "tool.timeout");

    let mut server = InProcessToolServer::connect(
        "delayed",
        DelayedFixture {
            inner: support::Fixture::new("delayed"),
            initialize_delay: Duration::ZERO,
            request_delay: Duration::from_secs(2),
        },
        options,
    )
    .await
    .unwrap();
    assert_eq!(
        server.client().list_tools().await.unwrap_err().code(),
        "tool.timeout"
    );
    assert_eq!(
        server
            .client()
            .call_tool("search", json!({}), None)
            .await
            .unwrap_err()
            .code(),
        "tool.timeout"
    );
    server.cleanup().await.unwrap();
}
