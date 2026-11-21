//! Responses WebSocket request-lock and idle-connection behavior through the model provider.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use ra_core::{
    error::ProviderErrorKind,
    model::{
        Model, ModelProvider, ModelRequest, ModelRetrySettings, ModelSettings, ModelStreamEvent,
        NormalizedProviderError, ProviderKey, ReplaySafety, SettingsLayers, replay_safety_of,
    },
    session::SessionId,
};
use ra_http::{HttpClientFactory, OutboundProxyPolicy, websocket::WebSocketConnector};
use ra_model::openai::{
    auth::OpenAiAuth,
    responses::{OpenAiResponsesProvider, ResponsesAuthHeaders, ResponsesBearerToken},
};
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_tungstenite::{
    WebSocketStream, accept_async, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
        http::{HeaderMap, HeaderName, HeaderValue},
    },
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const PATIENCE: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(100);

fn provider(listener: &TcpListener, connect_timeout: Duration) -> OpenAiResponsesProvider {
    provider_at(
        format!("http://{}/v1", listener.local_addr().unwrap()),
        connect_timeout,
    )
}

fn provider_at(endpoint: String, connect_timeout: Duration) -> OpenAiResponsesProvider {
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    OpenAiResponsesProvider::new(OpenAiAuth::keyless(endpoint), "model")
        .unwrap()
        .with_websocket(WebSocketConnector::new(&factory).unwrap(), connect_timeout)
}

fn request(settings: &ModelSettings) -> ModelRequest {
    ModelRequest::new(
        Vec::new(),
        SettingsLayers::new()
            .with_run(settings)
            .resolve(&ProviderKey::new("openai")),
    )
}

async fn send(socket: &mut WebSocketStream<TcpStream>, event: Value) {
    socket
        .send(Message::Text(event.to_string().into()))
        .await
        .unwrap();
}

fn completion() -> Value {
    json!({
        "type": "response.completed",
        "response": {"id": "resp_done", "status": "completed", "output": []}
    })
}

async fn completed(model: &dyn Model, request: ModelRequest) {
    let mut stream = model.stream_response(request);
    while let Some(event) = timeout(PATIENCE, stream.next()).await.unwrap() {
        if matches!(event.unwrap(), ModelStreamEvent::Completed(_)) {
            return;
        }
    }
    panic!("server never completed the response");
}

#[tokio::test]
async fn lock_uses_the_request_timeout_and_timeout_is_replay_safe() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = provider(&listener, CONNECT_TIMEOUT);
    let model = provider.get_model(None).unwrap();
    let peer = tokio::spawn(async move {
        let (first, _) = listener.accept().await.unwrap();
        let mut first = accept_async(first).await.unwrap();
        first.next().await.unwrap().unwrap();
        send(
            &mut first,
            json!({"type": "response.created", "response": {"id": "resp_held"}}),
        )
        .await;
        // The first caller abandons its response, so its socket is discarded.
        assert!(!matches!(first.next().await, Some(Ok(_))));
        let (second, _) = listener.accept().await.unwrap();
        let mut second = accept_async(second).await.unwrap();
        second.next().await.unwrap().unwrap();
        send(&mut second, completion()).await;
    });
    let mut first = model.stream_response(request(&ModelSettings::new()));
    assert!(matches!(
        timeout(PATIENCE, first.next()).await.unwrap(),
        Some(Ok(ModelStreamEvent::RawResponse(_)))
    ));

    let mut limited = model.stream_response(request(
        &ModelSettings::new().with_timeout(Duration::from_millis(20)),
    ));
    let error = timeout(PATIENCE, limited.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        NormalizedProviderError::from_error(&error).unwrap().kind(),
        ProviderErrorKind::Timeout
    );
    assert_eq!(replay_safety_of(&error), ReplaySafety::Safe);
    assert!(
        !NormalizedProviderError::from_error(&error)
            .unwrap()
            .response_started()
    );

    let advice = |error: &ra_core::error::Error| {
        model
            .get_retry_advice(&ra_core::model::ModelRetryAdviceRequest::new(
                error,
                1,
                true,
                &ra_core::model::ConversationContinuation::None,
            ))
            .unwrap()
    };
    assert_eq!(advice(&error).suggested(), Some(true));
    assert_eq!(advice(&error).replay_safety(), ReplaySafety::Safe);
    let restated = ra_core::model::stamp_replay_safety(error, ReplaySafety::Unsafe);
    assert_eq!(advice(&restated).suggested(), Some(false));
    assert_eq!(advice(&restated).replay_safety(), ReplaySafety::Unsafe);

    let mut waiting = model.stream_response(request(&ModelSettings::new()));
    // Waiting longer than the connection timeout still leaves the sessionless call queued.
    assert!(timeout(CONNECT_TIMEOUT * 2, waiting.next()).await.is_err());
    drop(first);
    assert!(matches!(
        timeout(PATIENCE, waiting.next()).await.unwrap(),
        Some(Ok(ModelStreamEvent::RawResponse(_)))
    ));
    assert!(matches!(
        timeout(PATIENCE, waiting.next()).await.unwrap(),
        Some(Ok(ModelStreamEvent::Completed(_)))
    ));
    timeout(PATIENCE, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn provider_evicts_the_oldest_idle_socket_when_the_cache_fills() {
    const SESSIONS: usize = 9;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = provider(&listener, PATIENCE);
    let model = provider.get_model(None).unwrap();
    let (closed, mut closures) = mpsc::unbounded_channel();
    let peer = tokio::spawn(async move {
        let mut sockets = Vec::new();
        for index in 0..SESSIONS {
            let (socket, _) = listener.accept().await.unwrap();
            let closed = closed.clone();
            sockets.push(tokio::spawn(async move {
                let mut socket = accept_async(socket).await.unwrap();
                while let Some(Ok(Message::Text(_))) = socket.next().await {
                    send(&mut socket, completion()).await;
                }
                closed.send(index).unwrap();
            }));
        }
        for socket in sockets {
            socket.await.unwrap();
        }
    });
    for index in 0..SESSIONS {
        let session = SessionId::new(format!("session-{index}"));
        completed(
            model.as_ref(),
            request(&ModelSettings::new()).with_session_id(session),
        )
        .await;
    }
    assert_eq!(timeout(PATIENCE, closures.recv()).await.unwrap(), Some(0));
    assert!(closures.try_recv().is_err());

    // The latest idle socket still serves new calls for its own session.
    completed(
        model.as_ref(),
        request(&ModelSettings::new())
            .with_session_id(SessionId::new(format!("session-{}", SESSIONS - 1))),
    )
    .await;
    provider.close().await.unwrap();
    timeout(PATIENCE, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn defaults_to_five_retries_and_honors_explicit_zero() {
    for (settings, upgrades) in [
        (ModelSettings::new(), 6),
        (
            ModelSettings::new().with_retry(ModelRetrySettings::new()),
            6,
        ),
        (
            ModelSettings::new().with_retry(ModelRetrySettings::new().with_max_retries(0)),
            1,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .expect(upgrades)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(format!("data: {}\n\n", completion()), "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let provider = provider_at(format!("{}/v1", server.uri()), PATIENCE);
        let model = provider.get_model(None).unwrap();
        completed(model.as_ref(), request(&settings)).await;
        server.verify().await;
    }
}

struct RotatingBearer(AtomicBool);

#[async_trait::async_trait]
impl ResponsesBearerToken for RotatingBearer {
    async fn token(&self, rejected: Option<&str>) -> ra_core::error::Result<String> {
        assert!(rejected.is_none(), "the endpoint accepted both credentials");
        let token = if self.0.load(Ordering::SeqCst) {
            "new-token"
        } else {
            "old-token"
        };
        Ok(token.to_owned())
    }
}

async fn accept_with_bearer(listener: &TcpListener, expected: &str) -> WebSocketStream<TcpStream> {
    let (socket, _) = listener.accept().await.unwrap();
    let expected = format!("Bearer {expected}");
    accept_hdr_async(socket, |request: &Request, response: Response| {
        assert_eq!(request.headers()["authorization"], expected);
        Ok(response)
    })
    .await
    .unwrap()
}

async fn receive_create(socket: &mut WebSocketStream<TcpStream>) -> Value {
    let frame = socket.next().await.unwrap().unwrap();
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

fn retry_disabled_session_request() -> ModelRequest {
    request(&ModelSettings::new().with_retry(ModelRetrySettings::new().with_max_retries(0)))
        .with_session_id(SessionId::new("test-session"))
}

#[tokio::test]
async fn changed_bearer_replaces_an_open_connection_and_resets_its_baseline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bearer = Arc::new(RotatingBearer(AtomicBool::new(false)));
    let provider = provider(&listener, PATIENCE).with_bearer_token(bearer.clone());
    let model = provider.get_model(None).unwrap();
    let peer = tokio::spawn(async move {
        let mut first = accept_with_bearer(&listener, "old-token").await;
        let original = receive_create(&mut first).await;
        send(&mut first, completion()).await;
        assert!(
            !matches!(first.next().await, Some(Ok(Message::Text(_)))),
            "old socket received a new request"
        );
        let mut second = accept_with_bearer(&listener, "new-token").await;
        let after_change = receive_create(&mut second).await;
        assert_eq!(
            after_change, original,
            "the new socket starts with a full create"
        );
        send(&mut second, completion()).await;
    });

    completed(model.as_ref(), retry_disabled_session_request()).await;
    bearer.0.store(true, Ordering::SeqCst);
    completed(model.as_ref(), retry_disabled_session_request()).await;
    timeout(PATIENCE, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn closed_cached_socket_is_replaced_even_with_no_retry_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = provider(&listener, PATIENCE);
    let model = provider.get_model(None).unwrap();
    let (closed, close_sent) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut first = accept_async(socket).await.unwrap();
        let original = receive_create(&mut first).await;
        send(&mut first, completion()).await;
        first.close(None).await.unwrap();
        drop(first);
        closed.send(()).unwrap();

        let (socket, _) = listener.accept().await.unwrap();
        let mut second = accept_async(socket).await.unwrap();
        let after_close = receive_create(&mut second).await;
        assert_eq!(
            after_close, original,
            "the new socket starts with a full create"
        );
        send(&mut second, completion()).await;
    });

    completed(model.as_ref(), retry_disabled_session_request()).await;
    timeout(PATIENCE, close_sent).await.unwrap().unwrap();
    completed(model.as_ref(), retry_disabled_session_request()).await;
    timeout(PATIENCE, peer).await.unwrap().unwrap();
}

/// Accompanying credentials that, like Codex's gateway composition, refuse a header the request
/// already has.
struct Accompanying(&'static [(&'static str, &'static str)]);

#[async_trait::async_trait]
impl ResponsesAuthHeaders for Accompanying {
    async fn apply_auth_headers(&self, headers: &mut HeaderMap) -> ra_core::error::Result<()> {
        for (name, value) in self.0 {
            let name = HeaderName::from_static(name);
            if headers.contains_key(&name) {
                return Err(ra_core::error::Error::config(
                    "conflicts with request headers",
                ));
            }
            headers.insert(name, HeaderValue::from_static(value));
        }
        Ok(())
    }
}

#[tokio::test]
async fn handshake_keeps_its_own_header_over_an_accompanying_credential() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = provider(&listener, PATIENCE).with_auth_headers(Arc::new(Accompanying(&[
        ("x-gateway-auth", "Bearer gateway"),
        ("x-gateway-extra", "extra"),
    ])));
    let model = provider.get_model(None).unwrap();
    // Codex adds the credential to a handshake only where it is absent, so the per-request header
    // keeps its value and the connection still opens; the other credential is still sent.
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = accept_hdr_async(socket, |request: &Request, response: Response| {
            let gateway: Vec<_> = request.headers().get_all("x-gateway-auth").iter().collect();
            assert_eq!(gateway, ["from-settings"]);
            assert_eq!(request.headers()["x-gateway-extra"], "extra");
            Ok(response)
        })
        .await
        .unwrap();
        socket.next().await.unwrap().unwrap();
        send(&mut socket, completion()).await;
    });
    completed(
        model.as_ref(),
        request(&ModelSettings::new().with_extra_header("x-gateway-auth", "from-settings")),
    )
    .await;
    timeout(PATIENCE, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn accompanying_authorization_conflicts_with_the_primary_bearer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    let source = Arc::new(Accompanying(&[("authorization", "Bearer gateway")]));
    let websocket = OpenAiResponsesProvider::new(
        OpenAiAuth::keyless(endpoint.clone()).with_api_key("primary"),
        "model",
    )
    .unwrap()
    .with_websocket(WebSocketConnector::new(&factory).unwrap(), PATIENCE)
    .with_auth_headers(source.clone());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let http = OpenAiResponsesProvider::new(
        OpenAiAuth::keyless(format!("{}/v1", server.uri())).with_api_key("primary"),
        "model",
    )
    .unwrap()
    .with_auth_headers(source);
    // Codex rejects a composed credential in a header the primary credential sends, on both
    // transports and before anything is sent.
    for provider in [websocket, http] {
        let model = provider.get_model(None).unwrap();
        let mut stream = model.stream_response(request(&ModelSettings::new()));
        let error = timeout(PATIENCE, stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code(), "config", "{error}");
    }
    assert!(
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "no handshake is attempted"
    );
}

/// A WebSocket failure after the first response event is unsafe to replay and records that the
/// provider had begun responding, as the SDK's WebSocket adapter marks it; the advice carries it
/// to the retry policy.
#[tokio::test]
async fn a_failure_after_the_first_event_records_that_the_response_started() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = provider(&listener, CONNECT_TIMEOUT);
    let model = provider.get_model(None).unwrap();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        socket.next().await.unwrap().unwrap();
        send(
            &mut socket,
            json!({"type": "response.created", "response": {"id": "resp_partial"}}),
        )
        .await;
        drop(socket);
    });
    let request = request(&ModelSettings::new());
    let mut stream = model.stream_response(request.clone());
    let error = loop {
        match timeout(PATIENCE, stream.next()).await.unwrap() {
            Some(Ok(_)) => {}
            Some(Err(error)) => break error,
            None => panic!("the dropped socket must end the stream with an error"),
        }
    };
    peer.await.unwrap();

    let normalized = NormalizedProviderError::from_error(&error).unwrap();
    assert_eq!(normalized.replay_safety(), ReplaySafety::Unsafe);
    assert!(normalized.response_started());
    let advice = model
        .get_retry_advice(&ra_core::model::ModelRetryAdviceRequest::new(
            &error,
            1,
            true,
            request.continuation(),
        ))
        .unwrap();
    assert!(advice.response_started());
    assert_eq!(advice.replay_safety(), ReplaySafety::Unsafe);
}

#[tokio::test]
async fn a_failed_first_response_frame_preserves_the_deadline_and_is_unsafe_to_replay() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = provider(&listener, CONNECT_TIMEOUT).with_runner_transport_fallback();
    let model = provider.get_model(None).unwrap();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        socket.next().await.unwrap().unwrap();
        send(
            &mut socket,
            json!({"type": "response.failed", "response": {"error": {
                "code": "rate_limit_exceeded", "message": "Try again in 30 seconds",
            }}}),
        )
        .await;
    });
    let before = tokio::time::Instant::now();
    let mut stream = model.stream_response(request(&ModelSettings::new()));
    let error = loop {
        match timeout(PATIENCE, stream.next()).await.unwrap() {
            Some(Ok(_)) => {}
            Some(Err(error)) => break error,
            None => panic!("the failed response must end the stream with an error"),
        }
    };
    let after = tokio::time::Instant::now();
    peer.await.unwrap();

    let delay = Duration::from_secs(30);
    let deadline = ra_model::openai::retry_after_deadline(&error).unwrap();
    assert!((before + delay..=after + delay).contains(&deadline));
    let normalized = NormalizedProviderError::from_error(&error).unwrap();
    assert!(normalized.response_started());
    assert_eq!(normalized.replay_safety(), ReplaySafety::Unsafe);
}

/// As the SDK's `test_websocket_pre_event_disconnect_retry_respects_websocket_retry_disable`: an
/// attempt whose retry the runtime owns returns its failure without reconnecting or sending HTTP.
/// A provider keeping independent transport retries, as Codex does, reconnects as configured.
/// HTTP 426 still negotiates the required HTTP transport without a reconnect.
#[tokio::test]
async fn a_runtime_owned_retry_returns_the_failure_without_an_http_replay() {
    for (independent, status, upgrades) in [(false, 503, 1), (true, 503, 6), (false, 426, 1)] {
        let falls_back = independent || status == 426;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(status).insert_header("retry-after", "0"))
            .expect(upgrades)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(format!("data: {}\n\n", completion()), "text/event-stream"),
            )
            .expect(if falls_back { 1 } else { 0 })
            .mount(&server)
            .await;
        let mut provider = provider_at(format!("{}/v1", server.uri()), PATIENCE);
        if independent {
            provider = provider.with_independent_transport_retries();
        }
        let model = provider.get_model(None).unwrap();
        let request =
            request(&ModelSettings::new()).with_websocket_pre_event_retries_disabled(true);
        if falls_back {
            completed(model.as_ref(), request).await;
        } else {
            let mut stream = model.stream_response(request);
            let error = stream.next().await.unwrap().unwrap_err();
            assert_eq!(
                NormalizedProviderError::from_error(&error)
                    .unwrap()
                    .status_code(),
                Some(503)
            );
            assert!(stream.next().await.is_none());
        }
        server.verify().await;
    }
}

/// As Codex's Responses WebSocket client: a provider that leaves the move to HTTP to the runner
/// returns an ordinary failure before the first event without reconnecting or replaying over HTTP,
/// even without the runner's per-attempt request. Once the runner switches the session, the
/// session stays on HTTP, and a second switch reports that the fallback was already active.
#[tokio::test]
async fn a_runner_owned_fallback_returns_websocket_failures_and_switches_on_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "30"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {}\n\n", completion()), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider =
        provider_at(format!("{}/v1", server.uri()), PATIENCE).with_runner_transport_fallback();
    let model = provider.get_model(None).unwrap();

    let request = request(&ModelSettings::new()).with_session_id(SessionId::new("failed"));
    assert!(provider.websocket_enabled(request.session_id()));
    let before = tokio::time::Instant::now();
    let mut stream = model.stream_response(request.clone());
    let error = stream.next().await.unwrap().unwrap_err();
    assert_eq!(
        NormalizedProviderError::from_error(&error)
            .unwrap()
            .status_code(),
        Some(503)
    );
    let deadline = ra_model::openai::retry_after_deadline(&error).unwrap();
    assert!(deadline >= before + Duration::from_secs(30));
    assert!(deadline <= tokio::time::Instant::now() + Duration::from_secs(30));
    assert!(stream.next().await.is_none());
    drop(stream);

    assert!(provider.switch_to_fallback_transport(&request).await);
    assert!(!provider.switch_to_fallback_transport(&request).await);
    assert!(!provider.websocket_enabled(request.session_id()));
    assert!(provider.websocket_enabled(Some(&SessionId::new("other"))));
    completed(model.as_ref(), request).await;
    server.verify().await;
}

/// A rejected HTTP upgrade can be safe to replay without the provider recommending a retry.
#[tokio::test]
async fn rejected_websocket_upgrades_use_http_retry_advice() {
    use ra_core::model::{ConversationContinuation, ModelRetryAdviceRequest};

    for (status, veto, expected) in [
        (400, false, None),
        (503, true, Some(false)),
        (503, false, Some(true)),
    ] {
        let server = MockServer::start().await;
        let mut refusal = ResponseTemplate::new(status);
        if veto {
            refusal = refusal
                .insert_header("x-should-retry", "false")
                .insert_header("retry-after", "12");
        }
        Mock::given(method("GET"))
            .and(path("/v1/responses"))
            .respond_with(refusal)
            .expect(1)
            .mount(&server)
            .await;
        let model = provider_at(format!("{}/v1", server.uri()), PATIENCE)
            .with_runner_transport_fallback()
            .get_model(None)
            .unwrap();
        let error = model
            .stream_response(request(&ModelSettings::new()))
            .next()
            .await
            .unwrap()
            .unwrap_err();
        let advice = model.get_retry_advice(&ModelRetryAdviceRequest::new(
            &error,
            1,
            true,
            &ConversationContinuation::None,
        ));
        assert_eq!(
            advice.is_some(),
            expected.is_some(),
            "{status}, veto={veto}"
        );
        if let Some(advice) = advice {
            assert_eq!(advice.suggested(), expected, "{status}, veto={veto}");
            assert_eq!(advice.replay_safety(), ReplaySafety::Unknown);
            assert_eq!(advice.retry_after(), veto.then(|| Duration::from_secs(12)));
        }
        server.verify().await;
    }
}

/// The SDK suggests retrying a stateless post-send disconnect, with no safety verdict.
#[tokio::test]
async fn post_send_websocket_disconnect_advice_depends_on_the_request_continuation() {
    use ra_core::model::{ConversationContinuation, ModelRetryAdviceRequest};

    for continuation in [
        ConversationContinuation::None,
        ConversationContinuation::PreviousResponseId("resp_prev".to_owned()),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model = provider(&listener, PATIENCE)
            .with_runner_transport_fallback()
            .get_model(None)
            .unwrap();
        let stateful = continuation.is_server_managed();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(socket).await.unwrap();
            let frame = socket.next().await.unwrap().unwrap();
            let sent: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            assert_eq!(sent["store"], true);
            assert_eq!(sent.get("previous_response_id").is_some(), stateful);
        });
        let settings = ModelSettings::new().with_extra_body_value(
            ProviderKey::new("openai"),
            "store",
            json!(true),
        );
        let request = request(&settings).with_continuation(continuation.clone());
        let error = model
            .stream_response(request)
            .next()
            .await
            .unwrap()
            .unwrap_err();
        peer.await.unwrap();
        // The transport still avoids replaying a potentially stored request internally.
        assert_eq!(replay_safety_of(&error), ReplaySafety::Unsafe);
        for streaming in [true, false] {
            let advice = model
                .get_retry_advice(&ModelRetryAdviceRequest::new(
                    &error,
                    1,
                    streaming,
                    &continuation,
                ))
                .unwrap();
            assert_eq!(advice.suggested(), Some(!stateful));
            assert_eq!(
                advice.replay_safety(),
                if stateful {
                    ReplaySafety::Unsafe
                } else {
                    ReplaySafety::Unknown
                }
            );
            assert!(!advice.response_started());
        }

        let started = ra_core::model::stamp_response_started(error);
        let advice = model
            .get_retry_advice(&ModelRetryAdviceRequest::new(
                &started,
                1,
                true,
                &continuation,
            ))
            .unwrap();
        assert_eq!(advice.suggested(), Some(false));
        assert_eq!(advice.replay_safety(), ReplaySafety::Unsafe);
        assert!(advice.response_started());
    }
}

/// Runs one streamed request against a peer that reads the request and answers with `replies`,
/// then holds the socket until the client leaves; returns the first failure.
async fn failure_after_replies(
    replies: Vec<Message>,
    settings: &ModelSettings,
    continuation: &ra_core::model::ConversationContinuation,
) -> (Arc<dyn Model>, ra_core::error::Error) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model = provider(&listener, PATIENCE)
        .with_runner_transport_fallback()
        .get_model(None)
        .unwrap();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        socket.next().await.unwrap().unwrap();
        for reply in replies {
            socket.send(reply).await.unwrap();
        }
        while let Some(Ok(_)) = socket.next().await {}
    });
    let request = request(settings).with_continuation(continuation.clone());
    let mut stream = model.stream_response(request);
    let error = loop {
        match timeout(PATIENCE, stream.next()).await.unwrap() {
            Some(Ok(_)) => {}
            Some(Err(error)) => break error,
            None => panic!("the stream must end with a failure"),
        }
    };
    drop(stream);
    timeout(PATIENCE, peer).await.unwrap().unwrap();
    (model, error)
}

fn advise(
    model: &dyn Model,
    error: &ra_core::error::Error,
    continuation: &ra_core::model::ConversationContinuation,
) -> Option<ra_core::model::RetryAdvice> {
    model.get_retry_advice(&ra_core::model::ModelRetryAdviceRequest::new(
        error,
        1,
        true,
        continuation,
    ))
}

fn continuations() -> [ra_core::model::ConversationContinuation; 2] {
    [
        ra_core::model::ConversationContinuation::None,
        ra_core::model::ConversationContinuation::PreviousResponseId("resp_prev".to_owned()),
    ]
}

/// As the SDK's `test_websocket_get_retry_advice_marks_ambiguous_replay_unsafe`,
/// `..._allows_stateless_ambiguous_disconnect_retry`, `..._marks_stateful_receive_timeout_unsafe`
/// and `..._allows_stateless_receive_timeout_retry`: a close frame or a read timeout after the
/// request was sent and before the first response event is retried without a verdict unless the
/// request continues provider-held state. The transport's own stamp for a stored request does not
/// change the advice.
#[tokio::test]
async fn pre_event_close_and_read_timeout_advice_depends_on_the_request_continuation() {
    let stored = ModelSettings::new().with_extra_body_value(
        ProviderKey::new("openai"),
        "store",
        json!(true),
    );
    let limited = stored.clone().with_timeout(Duration::from_millis(200));
    for (replies, settings, kind) in [
        (
            vec![Message::Close(None)],
            &stored,
            ProviderErrorKind::Network,
        ),
        (Vec::new(), &limited, ProviderErrorKind::Timeout),
    ] {
        for continuation in continuations() {
            let stateful = continuation.is_server_managed();
            let (model, error) =
                failure_after_replies(replies.clone(), settings, &continuation).await;
            let normalized = NormalizedProviderError::from_error(&error).unwrap();
            assert_eq!(normalized.kind(), kind);
            assert_eq!(normalized.replay_safety(), ReplaySafety::Unsafe);
            let advice = advise(model.as_ref(), &error, &continuation).unwrap();
            assert_eq!(advice.suggested(), Some(!stateful), "{kind:?}");
            assert_eq!(
                advice.replay_safety(),
                if stateful {
                    ReplaySafety::Unsafe
                } else {
                    ReplaySafety::Unknown
                },
                "{kind:?}"
            );
            assert!(!advice.response_started());
        }
    }
}

/// As the SDK's `test_websocket_get_retry_advice_marks_pre_response_overload_retryable`,
/// `..._marks_pre_response_server_error_retryable`,
/// `..._does_not_override_non_transient_error_code` and
/// `..._ignores_other_pre_response_error_codes`: an overloaded server, or a server error without a
/// code, reported before the first response event is retried without a verdict whatever the
/// continuation, including when the frame carries a status. Other codes get no advice, including
/// one Codex reads as a 503.
#[tokio::test]
async fn pre_response_error_frames_suggest_a_retry_only_when_transient() {
    let frame = |error: Value| json!({"type": "error", "error": error});
    let mut with_status = frame(json!({
        "type": "service_unavailable_error",
        "code": "server_is_overloaded",
        "message": "Our servers are currently overloaded. Please try again later.",
    }));
    with_status["status"] = json!(503);
    for (event, suggested) in [
        (
            frame(json!({
                "type": "service_unavailable_error",
                "code": "server_is_overloaded",
                "message": "Our servers are currently overloaded. Please try again later.",
            })),
            true,
        ),
        (
            frame(json!({
                "type": "server_error", "code": null, "message": "Sorry, something went wrong.",
            })),
            true,
        ),
        (with_status, true),
        (
            frame(json!({
                "type": "server_error", "code": "invalid_request_error",
                "message": "Invalid request.",
            })),
            false,
        ),
        (
            frame(json!({
                "type": "invalid_request_error", "code": "invalid_request",
                "message": "Invalid request.",
            })),
            false,
        ),
        // Codex reads this code as a 503, which the SDK's error type cannot see.
        (
            frame(json!({
                "type": "invalid_request_error", "code": "websocket_connection_limit_reached",
                "message": "Too many connections.",
            })),
            false,
        ),
    ] {
        for continuation in continuations() {
            let (model, error) = failure_after_replies(
                vec![Message::Text(event.to_string().into())],
                &ModelSettings::new(),
                &continuation,
            )
            .await;
            let advice = advise(model.as_ref(), &error, &continuation);
            if suggested {
                let advice = advice.unwrap();
                assert_eq!(advice.suggested(), Some(true), "{event}");
                assert_eq!(advice.replay_safety(), ReplaySafety::Unknown, "{event}");
            } else {
                assert!(advice.is_none(), "{event}");
            }
        }
    }
}

/// As the SDK's `test_websocket_get_retry_advice_keeps_partial_transient_error_unsafe`: a
/// transient error frame after the first response event is unsafe to replay.
#[tokio::test]
async fn a_transient_error_frame_after_the_first_event_is_unsafe_to_replay() {
    let continuation = ra_core::model::ConversationContinuation::None;
    let (model, error) = failure_after_replies(
        vec![
            Message::Text(
                json!({"type": "response.created", "response": {"id": "resp_partial"}})
                    .to_string()
                    .into(),
            ),
            Message::Text(
                json!({"type": "error", "error": {
                    "type": "server_error", "code": null, "message": "Transient provider error.",
                }})
                .to_string()
                .into(),
            ),
        ],
        &ModelSettings::new(),
        &continuation,
    )
    .await;
    let advice = advise(model.as_ref(), &error, &continuation).unwrap();
    assert_eq!(advice.suggested(), Some(false));
    assert_eq!(advice.replay_safety(), ReplaySafety::Unsafe);
    assert!(advice.response_started());
}

/// As the SDK's `test_websocket_get_retry_advice_marks_handshake_failure_replay_safe` and
/// `test_websocket_get_retry_advice_ignores_malformed_handshake`: a peer that leaves during the
/// handshake makes the request safe to retry, even one continuing provider-held state, while a
/// malformed handshake response gets no advice.
#[tokio::test]
async fn handshake_advice_separates_a_departed_peer_from_a_malformed_response() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let continuation =
        ra_core::model::ConversationContinuation::PreviousResponseId("resp_prev".to_owned());
    for malformed in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model = provider(&listener, PATIENCE)
            .with_runner_transport_fallback()
            .get_model(None)
            .unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut upgrade = Vec::new();
            let mut buffer = [0; 1024];
            while !upgrade.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0, "the client must send its upgrade request");
                upgrade.extend_from_slice(&buffer[..read]);
            }
            if malformed {
                socket
                    .write_all(b"HTTP/1.1 abc Switching\r\n\r\n")
                    .await
                    .unwrap();
            }
            socket.shutdown().await.unwrap();
        });
        let request = request(&ModelSettings::new()).with_continuation(continuation.clone());
        let error = timeout(PATIENCE, model.stream_response(request).next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        timeout(PATIENCE, peer).await.unwrap().unwrap();
        let advice = advise(model.as_ref(), &error, &continuation);
        if malformed {
            assert!(advice.is_none(), "{error}");
        } else {
            let advice = advice.unwrap();
            assert_eq!(advice.suggested(), Some(true));
            assert_eq!(advice.replay_safety(), ReplaySafety::Safe);
        }
    }
}

/// As the SDK's `test_websocket_get_retry_advice_marks_close_invalidation_non_retryable`: closing
/// the provider ends an in-flight request, and neither it nor a later request is suggested for a
/// retry.
#[tokio::test]
async fn closing_the_provider_refuses_a_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = provider(&listener, PATIENCE).with_runner_transport_fallback();
    let model = provider.get_model(None).unwrap();
    let (received, sent) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        socket.next().await.unwrap().unwrap();
        received.send(()).unwrap();
        while let Some(Ok(_)) = socket.next().await {}
    });
    let continuation = ra_core::model::ConversationContinuation::None;
    let mut stream = model.stream_response(request(&ModelSettings::new()));
    let (first, ()) = tokio::join!(timeout(PATIENCE, stream.next()), async {
        sent.await.unwrap();
        provider.close().await.unwrap();
    });
    let in_flight = first.unwrap().unwrap().unwrap_err();
    drop(stream);
    timeout(PATIENCE, peer).await.unwrap().unwrap();
    let later = timeout(
        PATIENCE,
        model.stream_response(request(&ModelSettings::new())).next(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap_err();

    for error in [in_flight, later] {
        assert!(
            matches!(error, ra_core::error::Error::Caller { .. }),
            "{error}"
        );
        let advice = advise(model.as_ref(), &error, &continuation).unwrap();
        assert_eq!(advice.suggested(), Some(false));
        assert_eq!(advice.replay_safety(), ReplaySafety::Unsafe);
    }
}
