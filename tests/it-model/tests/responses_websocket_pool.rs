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
    responses::{OpenAiResponsesProvider, ResponsesBearerToken},
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
