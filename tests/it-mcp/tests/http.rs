//! Streamable HTTP contracts ported from Python's client-factory, auth and session-id tests.

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use ra_mcp::{
    McpServer,
    client::{
        McpClient, McpClientOptions, McpTransportConfig,
        http::{McpHttpParams, McpServerStreamableHttp},
    },
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Seen {
    method: Method,
    headers: HeaderMap,
    body: Value,
}
#[derive(Default)]
struct Behavior {
    initialized_status: Option<StatusCode>,
    request_status: Option<StatusCode>,
    delete_status: Option<StatusCode>,
    initialize_delay: Duration,
    initialized_delay: Duration,
    request_delay: Duration,
    sse: bool,
    initialize_sse_prefix: String,
    stateless: bool,
    malformed: bool,
}
#[derive(Default)]
struct Fixture {
    behavior: Mutex<Behavior>,
    seen: Mutex<Vec<Seen>>,
    sessions: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
}
struct Server {
    url: String,
    state: Arc<Fixture>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new() -> Self {
        let state = Arc::new(Fixture::default());
        let app = Router::new()
            .route("/mcp", any(handle))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            url,
            state,
            cancel,
            task,
        }
    }
    fn params(&self) -> McpHttpParams {
        McpHttpParams::new(&self.url)
    }
    fn requests(&self, method: &str) -> Vec<Seen> {
        self.state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.body["method"] == method || seen.method.as_str() == method)
            .cloned()
            .collect()
    }
    async fn wait_for(&self, method: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.requests(method).len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}
async fn handle(
    State(state): State<Arc<Fixture>>,
    method: Method,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    state.seen.lock().unwrap().push(Seen {
        method: method.clone(),
        headers,
        body: body.clone(),
    });
    if method == Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if method == Method::DELETE {
        return state
            .behavior
            .lock()
            .unwrap()
            .delete_status
            .unwrap_or(StatusCode::OK)
            .into_response();
    }
    let operation = body["method"].as_str().unwrap_or("");
    if operation == "notifications/initialized" {
        let delay = state.behavior.lock().unwrap().initialized_delay;
        tokio::time::sleep(delay).await;
        return state
            .behavior
            .lock()
            .unwrap()
            .initialized_status
            .unwrap_or(StatusCode::ACCEPTED)
            .into_response();
    }
    if operation.starts_with("notifications/") {
        return StatusCode::ACCEPTED.into_response();
    }
    let (delay, status, sse, initialize_sse_prefix, stateless, malformed) = {
        let behavior = state.behavior.lock().unwrap();
        (
            if operation == "initialize" {
                behavior.initialize_delay
            } else {
                behavior.request_delay
            },
            if operation == "initialize" {
                None
            } else {
                behavior.request_status
            },
            behavior.sse,
            if operation == "initialize" {
                behavior.initialize_sse_prefix.clone()
            } else {
                String::new()
            },
            behavior.stateless,
            behavior.malformed,
        )
    };
    let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
    state.peak.fetch_max(active, Ordering::SeqCst);
    tokio::time::sleep(delay).await;
    state.active.fetch_sub(1, Ordering::SeqCst);
    if let Some(status) = status {
        // HTTP error bodies and challenges are transport data, never protocol tool output.
        return (
            status,
            [("www-authenticate", "Bearer realm=credential-secret")],
            "credential-secret",
        )
            .into_response();
    }
    let mut session = None;
    let result = match operation {
        "initialize" => {
            if !stateless {
                session = Some(format!(
                    "session-{}",
                    state.sessions.fetch_add(1, Ordering::SeqCst)
                ));
            }
            json!({"protocolVersion":"2025-11-25", "capabilities":{"tools":{},"prompts":{},"resources":{}},
                "serverInfo":{"name":"fixture","version":"1"}})
        }
        "tools/list" => json!({"tools":[]}),
        "prompts/list" => json!({"prompts":[]}),
        "prompts/get" => json!({"messages":[]}),
        "resources/list" => json!({"resources":[]}),
        "resources/templates/list" => json!({"resourceTemplates":[]}),
        "resources/read" => json!({"contents":[{"uri":"memo://note","text":"note"}]}),
        "tools/call" => json!({"content":[{"type":"text","text":"ok"}]}),
        _ => panic!("unexpected operation: {operation}"),
    };
    let payload = json!({"jsonrpc":"2.0","id":body["id"],"result":result});
    let mut response = if malformed && operation != "initialize" {
        ([("content-type", "application/json")], "credential-secret").into_response()
    } else if sse {
        (
            [("content-type", "text/event-stream")],
            format!("{initialize_sse_prefix}data: {payload}\n\n"),
        )
            .into_response()
    } else {
        axum::Json(payload).into_response()
    };
    if let Some(session) = session {
        response
            .headers_mut()
            .insert("mcp-session-id", session.parse().unwrap());
    }
    response
}

#[tokio::test]
async fn reusable_http_server_tracks_sessions_reconnects_and_reuses_shared_operations() {
    for sse in [false, true] {
        let fixture = Server::new().await;
        fixture.state.behavior.lock().unwrap().sse = sse;
        let server = McpServerStreamableHttp::new(fixture.params());
        assert_eq!(server.name(), format!("streamable_http: {}", fixture.url));
        assert_eq!(server.session_id(), None);
        assert!(!server.is_connected().await);
        assert!(server.list_tools().await.is_err());
        server.connect().await.unwrap();
        server.connect().await.unwrap();
        assert_eq!(fixture.requests("initialize").len(), 1);
        assert_eq!(server.session_id().as_deref(), Some("session-0"));
        let old = server.client().unwrap();
        assert!(server.list_tools().await.unwrap().is_empty());
        assert!(server.list_prompts().await.unwrap().prompts.is_empty());
        assert!(server.get_prompt("review", None).await.is_ok());
        assert!(server.list_resources(None).await.is_ok());
        assert!(server.list_resource_templates(None).await.is_ok());
        assert!(server.read_resource("memo://note").await.is_ok());
        server.call_tool("echo", None, None).await.unwrap();
        for seen in fixture.requests("tools/list") {
            assert_eq!(seen.headers["mcp-session-id"], "session-0");
            assert_eq!(seen.headers["mcp-protocol-version"], "2025-11-25");
        }
        server.cleanup().await.unwrap();
        server.cleanup().await.unwrap();
        assert_eq!(server.session_id(), None);
        assert_eq!(old.session_id(), None);
        assert!(old.list_tools().await.is_err());
        assert_eq!(fixture.requests("DELETE").len(), 1);
        server.connect().await.unwrap();
        assert_eq!(server.session_id().as_deref(), Some("session-1"));
        server.cleanup().await.unwrap();
        assert_eq!(fixture.requests("DELETE").len(), 2);
    }
}

#[tokio::test]
async fn stateless_and_terminate_on_close_false_do_not_delete_sessions() {
    for stateless in [false, true] {
        let fixture = Server::new().await;
        fixture.state.behavior.lock().unwrap().stateless = stateless;
        let client = McpClient::connect_transport(
            "http",
            McpTransportConfig::StreamableHttp(fixture.params().with_terminate_on_close(stateless)),
            McpClientOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(client.session_id().is_none(), stateless);
        client.list_tools().await.unwrap();
        client.cleanup().await.unwrap();
        assert!(fixture.requests("DELETE").is_empty());
    }
}

#[tokio::test]
async fn authentication_and_custom_factory_apply_to_post_get_and_delete() {
    let fixture = Server::new().await;
    let factories = Arc::new(AtomicUsize::new(0));
    let auths = Arc::new(AtomicUsize::new(0));
    let made = factories.clone();
    let authenticated = auths.clone();
    let params = fixture
        .params()
        .with_headers(std::collections::BTreeMap::from([
            ("x-static".into(), "static".into()),
            ("authorization".into(), "Bearer static".into()),
        ]))
        .with_auth(move |request| {
            authenticated.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(request
                    .basic_auth("user", Some("password"))
                    .header("x-auth", "hook"))
            }
        })
        .with_http_client_factory(move |builder| {
            made.fetch_add(1, Ordering::SeqCst);
            builder.user_agent("factory-agent").build()
        });
    let server = McpServerStreamableHttp::new(params);
    for _ in 0..2 {
        server.connect().await.unwrap();
        server.list_tools().await.unwrap();
        fixture
            .wait_for("GET", factories.load(Ordering::SeqCst))
            .await;
        server.cleanup().await.unwrap();
    }
    assert_eq!(factories.load(Ordering::SeqCst), 2);
    let seen = fixture.state.seen.lock().unwrap();
    assert_eq!(auths.load(Ordering::SeqCst), seen.len());
    for request in seen.iter() {
        assert_eq!(request.headers["x-static"], "static");
        assert_eq!(request.headers["x-auth"], "hook");
        assert_eq!(request.headers["user-agent"], "factory-agent");
        assert_eq!(
            request.headers["authorization"],
            "Basic dXNlcjpwYXNzd29yZA=="
        );
    }
    assert!(seen.iter().any(|request| request.method == Method::DELETE));
}

#[tokio::test]
async fn explicit_environment_token_overrides_authentication_hook_and_static_headers() {
    let fixture = Server::new().await;
    let params = fixture
        .params()
        .with_auth(|request| async { Ok(request.bearer_auth("hook")) })
        .with_headers(std::collections::BTreeMap::from([(
            "authorization".into(),
            "Bearer static".into(),
        )]))
        .with_bearer_token_env_var("PATH");
    let client = McpClient::connect_http("auth", params, McpClientOptions::default())
        .await
        .unwrap();
    client.list_tools().await.unwrap();
    client.cleanup().await.unwrap();
    for request in fixture.state.seen.lock().unwrap().iter() {
        assert_eq!(
            request.headers["authorization"],
            format!("Bearer {}", std::env::var("PATH").unwrap())
        );
    }
}

#[tokio::test]
async fn initialized_failure_is_strict_by_default_and_tolerance_is_scoped_to_that_notification() {
    for tolerate in [false, true] {
        let fixture = Server::new().await;
        fixture.state.behavior.lock().unwrap().initialized_status =
            Some(StatusCode::SERVICE_UNAVAILABLE);
        let server = McpServerStreamableHttp::new(
            fixture
                .params()
                .with_ignore_initialized_notification_failure(tolerate),
        );
        let result = server.connect().await;
        if tolerate {
            result.unwrap();
            server.list_tools().await.unwrap();
            fixture.state.behavior.lock().unwrap().request_status =
                Some(StatusCode::SERVICE_UNAVAILABLE);
            assert!(server.list_tools().await.is_err());
            server.cleanup().await.unwrap();
        } else {
            assert!(result.is_err());
            assert_eq!(server.session_id(), None);
            fixture.wait_for("DELETE", 1).await;
            assert_eq!(fixture.requests("initialize").len(), 1);
        }
    }
}

#[tokio::test]
async fn http_requests_are_serialized_and_cancellation_releases_the_request_lock() {
    let fixture = Server::new().await;
    fixture.state.behavior.lock().unwrap().request_delay = Duration::from_millis(70);
    let client = McpClient::connect_http("serial", fixture.params(), McpClientOptions::default())
        .await
        .unwrap();
    let (tools, prompts, resource) = tokio::join!(
        client.list_tools(),
        client.list_prompts(),
        client.read_resource("memo://note")
    );
    tools.unwrap();
    prompts.unwrap();
    resource.unwrap();
    assert_eq!(fixture.state.peak.load(Ordering::SeqCst), 1);
    fixture.state.behavior.lock().unwrap().request_delay = Duration::from_secs(30);
    let in_flight = tokio::spawn({
        let client = client.clone();
        async move { client.call_tool("echo", None, None).await }
    });
    fixture.wait_for("tools/call", 1).await;
    in_flight.abort();
    assert!(in_flight.await.unwrap_err().is_cancelled());
    fixture.state.behavior.lock().unwrap().request_delay = Duration::ZERO;
    tokio::time::timeout(Duration::from_secs(1), client.list_tools())
        .await
        .unwrap()
        .unwrap();
    client.cleanup().await.unwrap();
}

#[tokio::test]
async fn http_stream_read_timeout_and_session_deadline_are_independent() {
    let fixture = Server::new().await;
    let client = McpClient::connect_http(
        "read-timeout",
        fixture
            .params()
            .with_sse_read_timeout(Duration::from_millis(30)),
        McpClientOptions::default().with_session_timeout(None),
    )
    .await
    .unwrap();
    fixture.state.behavior.lock().unwrap().request_delay = Duration::from_millis(200);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), client.list_tools())
            .await
            .unwrap()
            .is_err()
    );
    client.cleanup().await.unwrap();

    fixture.state.behavior.lock().unwrap().request_delay = Duration::ZERO;
    let client = McpClient::connect_http(
        "session-timeout",
        fixture.params(),
        McpClientOptions::default().with_session_timeout(Duration::from_millis(30)),
    )
    .await
    .unwrap();
    fixture.state.behavior.lock().unwrap().request_delay = Duration::from_millis(200);
    assert_eq!(
        client.list_tools().await.unwrap_err().code(),
        "tool.timeout"
    );
    client.cleanup().await.unwrap();
}

#[tokio::test]
async fn expired_session_is_not_reinitialized_and_unsupported_delete_is_accepted() {
    let fixture = Server::new().await;
    let client = McpClient::connect_http("expired", fixture.params(), McpClientOptions::default())
        .await
        .unwrap();
    fixture.state.behavior.lock().unwrap().request_status = Some(StatusCode::NOT_FOUND);
    fixture.state.behavior.lock().unwrap().delete_status = Some(StatusCode::METHOD_NOT_ALLOWED);
    assert!(client.call_tool("echo", None, None).await.is_err());
    assert_eq!(fixture.requests("initialize").len(), 1);
    assert_eq!(fixture.requests("tools/call").len(), 1);
    client.cleanup().await.unwrap();
    assert_eq!(fixture.requests("DELETE").len(), 1);
}

#[tokio::test]
async fn cleanup_cancels_initialization_and_allows_reconnect() {
    let fixture = Server::new().await;
    fixture.state.behavior.lock().unwrap().initialize_delay = Duration::from_millis(200);
    let server = Arc::new(McpServerStreamableHttp::new(fixture.params()));
    let connecting = tokio::spawn({
        let server = server.clone();
        async move { server.connect().await }
    });
    fixture.wait_for("initialize", 1).await;
    server.cleanup().await.unwrap();
    assert!(connecting.await.unwrap().is_err());
    assert_eq!(server.session_id(), None);
    fixture.state.behavior.lock().unwrap().initialize_delay = Duration::ZERO;
    server.connect().await.unwrap();
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_client_does_not_replay_headers_to_redirect_targets() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};
    let target = MockServer::start().await;
    let original = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(307).insert_header("location", format!("{}/mcp", target.uri())),
        )
        .mount(&original)
        .await;
    let result = McpClient::connect_http(
        "redirect",
        McpHttpParams::new(format!("{}/mcp", original.uri())).with_headers(
            std::collections::BTreeMap::from([("x-api-key".into(), "secret".into())]),
        ),
        McpClientOptions::default(),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(original.received_requests().await.unwrap().len(), 1);
    assert!(target.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cancelling_initialize_after_session_assignment_terminates_the_owned_session() {
    let fixture = Server::new().await;
    fixture.state.behavior.lock().unwrap().initialized_delay = Duration::from_secs(30);
    let server = Arc::new(McpServerStreamableHttp::new(fixture.params()));
    let connecting = tokio::spawn({
        let server = server.clone();
        async move { server.connect().await }
    });
    fixture.wait_for("notifications/initialized", 1).await;
    server.cleanup().await.unwrap();
    assert!(connecting.await.unwrap().is_err());
    assert_eq!(server.session_id(), None);
    fixture.wait_for("DELETE", 1).await;
    assert_eq!(fixture.requests("DELETE").len(), 1);
    fixture.state.behavior.lock().unwrap().initialized_delay = Duration::ZERO;
    server.connect().await.unwrap();
    server.cleanup().await.unwrap();
    assert_eq!(fixture.requests("DELETE").len(), 2);
}

#[tokio::test]
async fn failed_connection_awaits_cleanup_and_keeps_the_connection_error() {
    let fixture = Server::new().await;
    {
        let mut behavior = fixture.state.behavior.lock().unwrap();
        behavior.initialized_status = Some(StatusCode::SERVICE_UNAVAILABLE);
        behavior.delete_status = Some(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let server = McpServerStreamableHttp::new(fixture.params());
    let error = server.connect().await.unwrap_err();
    // Python's transport only logs a failed DELETE; the connection failure stays the error.
    assert_eq!(error.code(), "tool.execution_failed");
    assert!(error.to_string().contains("initialize failed"));
    assert!(!error.to_string().contains("cleanup"));
    assert_eq!(fixture.requests("DELETE").len(), 1);
    assert_eq!(server.session_id(), None);
    {
        let mut behavior = fixture.state.behavior.lock().unwrap();
        behavior.initialized_status = None;
        behavior.delete_status = None;
    }
    server.connect().await.unwrap();
    server.cleanup().await.unwrap();
    assert_eq!(fixture.requests("DELETE").len(), 2);
}

#[test]
fn pending_connect_can_be_dropped_outside_its_runtime_context() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = runtime.block_on(Server::new());
    fixture.state.behavior.lock().unwrap().initialized_delay = Duration::from_secs(30);
    let mut connecting = Box::pin(McpClient::connect_http(
        "dropped",
        fixture.params(),
        McpClientOptions::default(),
    ));
    runtime.block_on(async {
        tokio::select! {
            _ = &mut connecting => panic!("initialization should still be pending"),
            () = fixture.wait_for("notifications/initialized", 1) => {},
        }
    });
    // The runtime is alive, but there is no current Tokio context here.
    drop(connecting);
    runtime.block_on(fixture.wait_for("DELETE", 1));
    assert_eq!(fixture.requests("DELETE").len(), 1);
}

#[tokio::test]
async fn protocol_answers_survive_http_status_media_type_case_and_control_frames() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};
    let server = MockServer::start().await;
    let mode = Arc::new(AtomicUsize::new(0));
    let response_mode = mode.clone();
    Mock::given(any())
        .respond_with(move |request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let method = payload["method"].as_str().unwrap_or("");
            match (request.method.as_str(), method) {
                ("GET", _) => return ResponseTemplate::new(405),
                ("DELETE", _) => return ResponseTemplate::new(200),
                (_, "initialize") => {
                    return ResponseTemplate::new(200)
                        .insert_header("mcp-session-id", "session")
                        .set_body_json(json!({"jsonrpc":"2.0","id":payload["id"],"result":{
                            "protocolVersion":"2025-11-25","capabilities":{"tools":{}},
                            "serverInfo":{"name":"fixture","version":"1"}}}));
                }
                (_, method) if method.starts_with("notifications/") => {
                    return ResponseTemplate::new(202);
                }
                _ => {}
            }
            // A server may reject a request before reading its ID.
            let rejected = json!({"jsonrpc":"2.0","id":null,"error":{
                "code":-32602,"message":"invalid arguments"}});
            let listed = json!({"jsonrpc":"2.0","id":payload["id"],"result":{"tools":[]}});
            match response_mode.load(Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(rejected),
                1 => ResponseTemplate::new(404).set_body_json(rejected),
                2 => ResponseTemplate::new(400).set_body_json(json!({"detail":"not json-rpc"})),
                // Wiremock's string body setter replaces the content type; raw bodies keep it.
                3 => ResponseTemplate::new(200)
                    .set_body_raw(listed.to_string(), "Application/JSON; charset=utf-8"),
                _ => ResponseTemplate::new(200).set_body_raw(
                    format!("event: ping\ndata: keep-alive\n\nevent: message\ndata: {listed}\n\n"),
                    "text/event-stream",
                ),
            }
        })
        .mount(&server)
        .await;
    let client = McpClient::connect_http(
        "answers",
        McpHttpParams::new(format!("{}/mcp", server.uri())),
        McpClientOptions::default(),
    )
    .await
    .unwrap();
    for status_mode in [0, 1] {
        mode.store(status_mode, Ordering::SeqCst);
        let error = client.list_tools().await.unwrap_err();
        assert_eq!(error.code(), "tool.invalid_input");
        assert!(error.to_string().contains("-32602: invalid arguments"));
    }
    for success_mode in [3, 4] {
        mode.store(success_mode, Ordering::SeqCst);
        assert!(client.list_tools().await.unwrap().is_empty());
    }
    mode.store(2, Ordering::SeqCst);
    let error = client.list_tools().await.unwrap_err();
    assert_eq!(error.code(), "tool.execution_failed");
    assert!(!error.to_string().contains("not json-rpc"));
    mode.store(3, Ordering::SeqCst);
    assert!(client.list_tools().await.unwrap().is_empty());
    client.cleanup().await.unwrap();
}

#[tokio::test]
async fn initialize_sse_ignores_control_frames_and_keeps_message_validation() {
    let fixture = Server::new().await;
    {
        let mut behavior = fixture.state.behavior.lock().unwrap();
        behavior.sse = true;
        behavior.initialize_sse_prefix = "event: ping\ndata: keep-alive\n\n".into();
    }
    let server = McpServerStreamableHttp::new(fixture.params());
    server.connect().await.unwrap();
    assert_eq!(server.session_id().as_deref(), Some("session-0"));
    server.list_tools().await.unwrap();
    server.cleanup().await.unwrap();
    assert_eq!(fixture.requests("DELETE").len(), 1);

    // Only control frames are ignored; malformed message data must still fail initialization.
    fixture.state.behavior.lock().unwrap().initialize_sse_prefix =
        "event: message\ndata: invalid-json\n\n".into();
    assert!(server.connect().await.is_err());
    assert_eq!(server.session_id(), None);
    assert_eq!(fixture.requests("DELETE").len(), 2);
}

#[tokio::test]
async fn cancelling_failed_initialize_cleanup_keeps_the_in_flight_delete() {
    use ra_mcp::client::http::reqwest;

    let fixture = Server::new().await;
    fixture.state.behavior.lock().unwrap().initialized_status =
        Some(StatusCode::SERVICE_UNAVAILABLE);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let delete_auths = Arc::new(AtomicUsize::new(0));
    let params = fixture.params().with_auth({
        let entered = entered.clone();
        let release = release.clone();
        let delete_auths = delete_auths.clone();
        move |builder| {
            let entered = entered.clone();
            let release = release.clone();
            let delete_auths = delete_auths.clone();
            async move {
                let (client, request) = builder.build_split();
                let request = request.unwrap();
                if request.method() == reqwest::Method::DELETE
                    && delete_auths.fetch_add(1, Ordering::SeqCst) == 0
                {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(reqwest::RequestBuilder::from_parts(client, request))
            }
        }
    });
    let server = Arc::new(McpServerStreamableHttp::new(params));
    let connecting = tokio::spawn({
        let server = server.clone();
        async move { server.connect().await }
    });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), server.cleanup())
        .await
        .unwrap()
        .unwrap();
    assert!(connecting.await.unwrap().is_err());
    assert_eq!(server.session_id(), None);
    assert!(fixture.requests("DELETE").is_empty());

    // The same auth future must resume after the caller is cancelled, without a second DELETE.
    release.notify_one();
    fixture.wait_for("DELETE", 1).await;
    assert_eq!(delete_auths.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.requests("DELETE")[0].headers["mcp-session-id"],
        "session-0"
    );
    fixture.state.behavior.lock().unwrap().initialized_status = None;
    server.connect().await.unwrap();
    assert_eq!(server.session_id().as_deref(), Some("session-1"));
    server.cleanup().await.unwrap();
    assert_eq!(fixture.requests("DELETE").len(), 2);
    assert_eq!(delete_auths.load(Ordering::SeqCst), 2);
}
