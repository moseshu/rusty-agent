// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Session-owned Responses WebSocket connections and HTTP fallback.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{
    FutureExt, SinkExt, StreamExt,
    stream::{self, BoxStream},
};
use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    model::{ModelRequest, NormalizedProviderError, ReplaySafety, stamp_replay_safety},
    session::SessionId,
};
use ra_http::websocket::{WebSocketConnection, WebSocketConnector, network_policy_denial};
use rand::Rng;
use reqwest::{
    StatusCode, Url,
    header::{self, HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex as AsyncMutex, OwnedMutexGuard, watch},
    time::{sleep, timeout},
};
use tokio_tungstenite::tungstenite::{
    Error as WsError, Message, client::IntoClientRequest, protocol::WebSocketConfig,
};

use super::{OpenAiResponsesModel, convert, request};
use crate::openai::{
    error::{ResponseFacts, behavior_error, response_failure, response_failure_as},
    sse::SseFrame,
};

use crate::retry::RetryHints;

const DEFAULT_STREAM_MAX_RETRIES: u32 = 5;
const DEFAULT_POOL_TIMEOUT: Duration = Duration::from_mins(10);
const MAX_IDLE_CONNECTIONS: usize = 8;

type Frames = BoxStream<'static, Result<SseFrame>>;

pub(super) struct WebSocketTransport {
    connector: WebSocketConnector,
    connect_timeout: Duration,
    cache: Arc<Mutex<SessionCache>>,
    closed: watch::Sender<bool>,
}

#[derive(Default)]
struct Session {
    fallback: bool,
}

#[derive(Default)]
struct SessionCache {
    closed: bool,
    sessions: BTreeMap<Option<SessionId>, Arc<AsyncMutex<Session>>>,
    // Oldest first. In-flight calls lease their socket out of this queue.
    idle: VecDeque<(Option<SessionId>, Connection)>,
}

impl SessionCache {
    fn lane(&mut self, id: Option<&SessionId>) -> Arc<AsyncMutex<Session>> {
        let id = id.cloned();
        // Failed or evicted sessions need no lane once their in-flight callers are gone.
        // Preserve the session's sticky fallback decision for the provider's lifetime.
        self.sessions.retain(|key, lane| {
            *key == id
                || Arc::get_mut(lane).is_none_or(|lane| lane.get_mut().fallback)
                || self.idle.iter().any(|(cached, _)| cached == key)
        });
        Arc::clone(self.sessions.entry(id).or_default())
    }

    fn take_connection(&mut self, id: Option<&SessionId>) -> Option<Connection> {
        let index = self.idle.iter().position(|(key, _)| key.as_ref() == id)?;
        self.idle.remove(index).map(|(_, connection)| connection)
    }

    fn put_connection(&mut self, id: Option<&SessionId>, connection: Connection) {
        if self.closed {
            return;
        }
        if self.idle.len() == MAX_IDLE_CONNECTIONS {
            self.idle.pop_front();
        }
        self.idle.push_back((id.cloned(), connection));
    }
}

#[derive(PartialEq, Eq)]
struct ConnectionKey {
    url: Url,
    headers: HeaderMap,
}

struct Connection {
    socket: WebSocketConnection,
    key: ConnectionKey,
    previous: Option<PreviousResponse>,
}

struct PreviousResponse {
    body: Value,
    output: Vec<Value>,
    id: String,
}

struct AttemptError {
    error: Error,
    during_upgrade: bool,
}

struct Call {
    model: OpenAiResponsesModel,
    request: ModelRequest,
    body: Value,
    idle_timeout: Duration,
    replayable: bool,
}

struct ActiveResponse {
    cache: Arc<Mutex<SessionCache>>,
    connection: Option<Connection>,
    call: Call,
    _session: OwnedMutexGuard<Session>,
}

impl WebSocketTransport {
    pub(super) fn new(connector: WebSocketConnector, connect_timeout: Duration) -> Self {
        Self {
            connector,
            connect_timeout,
            cache: Arc::new(Mutex::new(SessionCache::default())),
            closed: watch::channel(false).0,
        }
    }

    pub(super) fn close(&self) -> Result<()> {
        let mut cache = self.cache.lock().map_err(|_| poisoned_cache())?;
        cache.closed = true;
        self.closed.send_replace(true);
        cache.idle.clear();
        cache.sessions.clear();
        Ok(())
    }

    async fn lock_session(&self, request: &ModelRequest) -> Result<OwnedMutexGuard<Session>> {
        let session = {
            let mut cache = self.cache.lock().map_err(|_| poisoned_cache())?;
            if cache.closed {
                return Err(closed_error());
            }
            cache.lane(request.session_id())
        };
        let limit = request
            .model_settings()
            .timeout()
            .unwrap_or(DEFAULT_POOL_TIMEOUT);
        timeout(limit, session.lock_owned()).await.map_err(|_| {
            stamp_replay_safety(
                timeout_error("waiting for a Responses WebSocket session"),
                ReplaySafety::Safe,
            )
        })
    }

    pub(super) async fn start(
        &self,
        model: &OpenAiResponsesModel,
        request: &ModelRequest,
    ) -> Result<Option<Frames>> {
        let mut closed = self.closed.subscribe();
        let frames = tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => return Err(closed_error()),
            result = self.start_inner(model, request) => result?,
        };
        Ok(frames.map(|frames| {
            stream::unfold(Some((frames, closed)), |state| async move {
                let (mut frames, mut closed) = state?;
                let event = tokio::select! {
                    biased;
                    _ = closed.wait_for(|closed| *closed) => return Some((Err(closed_error()), None)),
                    event = frames.next() => event?,
                };
                Some((event, Some((frames, closed))))
            })
            .boxed()
        }))
    }

    async fn start_inner(
        &self,
        model: &OpenAiResponsesModel,
        request: &ModelRequest,
    ) -> Result<Option<Frames>> {
        let mut session = self.lock_session(request).await?;
        if session.fallback {
            return Ok(None);
        }
        let call = Call::new(model, request).await?;
        let headers = crate::openai::transport_headers(
            &model.transport.auth,
            request.model_settings().extra_headers(),
        )?;
        let bearer = model.transport.bearer.as_ref();
        let mut token = match bearer {
            Some(bearer) => Some(bearer.token(None).await?),
            None => None,
        };
        let mut refreshed = false;
        let mut retries = 0;
        let max_retries = request
            .model_settings()
            .retry()
            .and_then(ra_core::model::ModelRetrySettings::max_retries)
            .unwrap_or(DEFAULT_STREAM_MAX_RETRIES);
        loop {
            let key = call.connection_key(
                headers.clone(),
                token.as_deref().or(model.transport.auth.api_key()),
            )?;
            // A cancelled or failed request drops its socket. Only a completed response returns it.
            let cached = self
                .cache
                .lock()
                .map_err(|_| poisoned_cache())?
                .take_connection(request.session_id());
            match self.attempt(&call, cached, key).await {
                Ok((connection, first)) => {
                    let mut active = ActiveResponse {
                        cache: Arc::clone(&self.cache),
                        connection: Some(connection),
                        call,
                        _session: session,
                    };
                    active.finish_if_terminal(&first).await;
                    return Ok(Some(
                        stream::once(async { Ok(SseFrame::Data(first)) })
                            .chain(active.frames())
                            .boxed(),
                    ));
                }
                Err(AttemptError {
                    error,
                    during_upgrade,
                }) => {
                    let facts = NormalizedProviderError::from_error(&error);
                    let status = facts.and_then(NormalizedProviderError::status_code);
                    if status == Some(401)
                        && !refreshed
                        && let Some(bearer) = bearer
                    {
                        token = Some(bearer.token(token.as_deref()).await?);
                        refreshed = true;
                        continue;
                    }
                    let upgrade_required = during_upgrade && status == Some(426);
                    let retryable = error.is_retryable()
                        && facts.is_none_or(|facts| {
                            facts.should_retry() != Some(false)
                                && !matches!(
                                    facts.error_code(),
                                    Some("insufficient_quota" | "usage_limit_reached")
                                )
                        });
                    if !upgrade_required
                        && (!retryable
                            || ra_core::model::replay_safety_of(&error) != ReplaySafety::Safe)
                    {
                        return Err(error);
                    }
                    let delay = facts.and_then(NormalizedProviderError::retry_after);
                    if upgrade_required || retries >= max_retries {
                        session.fallback = true;
                        if let Some(delay) = delay {
                            sleep(delay).await;
                        }
                        tracing::warn!(
                            "falling back from Responses WebSocket to HTTP for this session"
                        );
                        return Ok(None);
                    }
                    sleep(delay.unwrap_or_else(|| retry_delay(retries))).await;
                    retries += 1;
                }
            }
        }
    }

    async fn attempt(
        &self,
        call: &Call,
        cached: Option<Connection>,
        key: ConnectionKey,
    ) -> std::result::Result<(Connection, Value), AttemptError> {
        let reusable = cached
            .filter(|connection| connection.key == key)
            .and_then(|mut connection| connection.available().then_some(connection));
        let mut connection = match reusable {
            Some(connection) => connection,
            None => self
                .connect(call, key)
                .await
                .map_err(|error| AttemptError {
                    error: stamp_replay_safety(error, ReplaySafety::Safe),
                    during_upgrade: true,
                })?,
        };
        let body = call.create_frame(connection.previous.as_ref());
        let sent = timeout(
            call.idle_timeout,
            connection
                .socket
                .send(Message::Text(body.to_string().into())),
        )
        .await
        .map_err(|_| timeout_error("sending a Responses WebSocket request"))
        .and_then(|result| result.map_err(|error| transport_error(&error)));
        let first = async {
            sent?;
            next_event(&mut connection.socket, call.idle_timeout).await
        }
        .await
        .map_err(|error| AttemptError {
            error: stamp_replay_safety(
                error,
                if call.replayable {
                    ReplaySafety::Safe
                } else {
                    ReplaySafety::Unsafe
                },
            ),
            during_upgrade: false,
        })?;
        Ok((connection, first))
    }

    async fn connect(&self, call: &Call, key: ConnectionKey) -> Result<Connection> {
        let mut upgrade = key
            .url
            .as_str()
            .into_client_request()
            .map_err(|_| Error::config("invalid Responses WebSocket handshake request"))?;
        upgrade.headers_mut().extend(key.headers.clone());
        let limit = call
            .request
            .model_settings()
            .timeout()
            .map_or(self.connect_timeout, |value| {
                value.min(self.connect_timeout)
            });
        let (socket, _) = timeout(
            limit,
            self.connector.connect(upgrade, WebSocketConfig::default()),
        )
        .await
        .map_err(|_| timeout_error("connecting to Responses WebSocket"))?
        .map_err(|error| upgrade_error(&error))?;
        Ok(Connection {
            socket,
            key,
            previous: None,
        })
    }
}

impl Connection {
    fn available(&mut self) -> bool {
        loop {
            match self.socket.next().now_or_never() {
                None => return true,
                Some(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
                _ => return false,
            }
        }
    }
}

impl Call {
    async fn new(model: &OpenAiResponsesModel, request: &ModelRequest) -> Result<Self> {
        let body = request::build_request_body(&model.model, request, model.quirks, true).await?;
        let idle = model
            .transport
            .stream_idle_timeout
            .unwrap_or(Duration::from_mins(5));
        Ok(Self {
            replayable: body["store"] == false && !request.continuation().is_server_managed(),
            idle_timeout: request
                .model_settings()
                .timeout()
                .map_or(idle, |value| value.min(idle)),
            model: model.clone(),
            request: request.clone(),
            body,
        })
    }

    fn connection_key(&self, mut headers: HeaderMap, token: Option<&str>) -> Result<ConnectionKey> {
        let mut url = Url::parse(&format!(
            "{}/responses",
            self.model.transport.auth.base_url()
        ))
        .map_err(|_| Error::config("invalid Responses WebSocket endpoint"))?;
        let scheme = match url.scheme() {
            "http" | "ws" => "ws",
            "https" | "wss" => "wss",
            _ => return Err(Error::config("unsupported Responses WebSocket URL scheme")),
        };
        url.set_scheme(scheme)
            .map_err(|()| Error::config("invalid Responses WebSocket URL scheme"))?;
        let query = self.model.query(&self.request);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        headers
            .entry(header::USER_AGENT)
            .or_insert(HeaderValue::from_static(concat!(
                "rusty-agent/",
                env!("CARGO_PKG_VERSION")
            )));
        headers.insert(
            "openai-beta",
            HeaderValue::from_static("responses_websockets=2026-02-06"),
        );
        if let Some(token) = token {
            let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| Error::config("invalid Responses WebSocket bearer credential"))?;
            authorization.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, authorization);
        }
        Ok(ConnectionKey { url, headers })
    }

    fn create_frame(&self, previous: Option<&PreviousResponse>) -> Value {
        let mut body = self.body.clone();
        if self.replayable
            && self.request.session_id().is_some()
            && !body.as_object().is_some_and(|body| {
                body.contains_key("previous_response_id") || body.contains_key("conversation")
            })
            && let Some(previous) = previous
            && let Some(delta) = previous.delta(&body)
        {
            body["input"] = json!(delta);
            body["previous_response_id"] = json!(previous.id);
        }
        body["type"] = json!("response.create");
        body
    }

    async fn baseline(&self, event: &Value) -> Option<PreviousResponse> {
        let payload = event.get("response")?;
        let response = convert::convert_response(
            payload,
            None,
            self.request.handoffs(),
            self.request.model_settings().provider(),
            &self.model.model,
        )
        .ok()?;
        let id = response
            .response_id()
            .filter(|id| !id.is_empty())?
            .to_owned();
        let mut output = Vec::new();
        for item in response
            .output()
            .iter()
            .filter_map(ra_core::item::RunItem::to_model_input)
        {
            output.push(
                request::lower_input_item(&item, self.request.handoffs())
                    .await
                    .ok()?,
            );
        }
        Some(PreviousResponse {
            body: self.body.clone(),
            output,
            id,
        })
    }
}

impl PreviousResponse {
    fn delta<'a>(&self, body: &'a Value) -> Option<&'a [Value]> {
        let settings = |name: &str| {
            !matches!(
                name,
                "input" | "client_metadata" | "stream_options" | "access_programs"
            )
        };
        let previous = self.body.as_object()?;
        let current = body.as_object()?;
        if !previous
            .iter()
            .filter(|(name, _)| settings(name))
            .eq(current.iter().filter(|(name, _)| settings(name)))
        {
            return None;
        }
        let prefix = self.body["input"].as_array()?;
        let rest = body["input"].as_array()?.strip_prefix(prefix.as_slice())?;
        rest.strip_prefix(self.output.as_slice())
    }
}

impl ActiveResponse {
    async fn finish_if_terminal(&mut self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("response.completed") => {
                if let Some(mut connection) = self.connection.take() {
                    connection.previous = self.call.baseline(event).await;
                    if let Ok(mut cache) = self.cache.lock() {
                        cache.put_connection(self.call.request.session_id(), connection);
                    }
                }
            }
            Some("response.failed" | "response.incomplete" | "response.error") => {
                self.connection = None;
            }
            _ => {}
        }
    }

    fn frames(self) -> Frames {
        stream::unfold(self, |mut active| async move {
            let connection = active.connection.as_mut()?;
            let event = match next_event(&mut connection.socket, active.call.idle_timeout).await {
                Ok(event) => {
                    active.finish_if_terminal(&event).await;
                    Ok(SseFrame::Data(event))
                }
                Err(error) => {
                    active.connection = None;
                    Err(error)
                }
            };
            Some((event, active))
        })
        .boxed()
    }
}

async fn next_event(socket: &mut WebSocketConnection, idle: Duration) -> Result<Value> {
    timeout(idle, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let event: Value = serde_json::from_str(text.as_str()).map_err(|error| {
                        behavior_error("invalid Responses WebSocket event JSON").with_source(error)
                    })?;
                    if event.get("type").and_then(Value::as_str) == Some("error") {
                        return Err(wrapped_error(&event));
                    }
                    return Ok(event);
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Err(error)) => return Err(transport_error(&error)),
                Some(Ok(Message::Close(_))) | None => {
                    return Err(Error::provider(
                        ProviderErrorKind::Network,
                        "Responses WebSocket closed before a terminal event",
                    ));
                }
                Some(Ok(_)) => return Err(behavior_error("unexpected Responses WebSocket frame")),
            }
        }
    })
    .await
    .map_err(|_| timeout_error("reading a Responses WebSocket event"))?
}

fn wrapped_error(event: &Value) -> Error {
    let code = event.pointer("/error/code").and_then(Value::as_str);
    let mut headers = HeaderMap::new();
    for values in [event.get("headers"), event.pointer("/error/headers")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        for (name, value) in values {
            if let Ok(name) = HeaderName::try_from(name)
                && let Some(value) = value.as_str()
                && let Ok(value) = HeaderValue::from_str(value)
            {
                headers.insert(name, value);
            }
        }
    }
    if matches!(
        code,
        Some("websocket_connection_limit_reached" | "previous_response_not_found")
    ) {
        let facts = ResponseFacts::from_headers(StatusCode::SERVICE_UNAVAILABLE, &headers);
        return response_failure(&facts, event).into_error();
    }
    let status = event
        .get("status")
        .or_else(|| event.get("status_code"))
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .and_then(|status| StatusCode::from_u16(status).ok());
    if let Some(status) = status {
        return response_failure(&ResponseFacts::from_headers(status, &headers), event)
            .into_error();
    }
    let kind = if code == Some("server_is_overloaded")
        || (code.is_none()
            && event.pointer("/error/type").and_then(Value::as_str) == Some("server_error"))
    {
        ProviderErrorKind::ServerError
    } else {
        ProviderErrorKind::BadRequest
    };
    let hints =
        RetryHints::from_headers(|name| headers.get(name).and_then(|value| value.to_str().ok()));
    let request_id = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok());
    response_failure_as(None, request_id, &hints, event, kind).into_error()
}

fn upgrade_error(error: &WsError) -> Error {
    let WsError::Http(response) = error else {
        return transport_error(error);
    };
    let status = response.status();
    if matches!(status.as_u16(), 400 | 401 | 426 | 429 | 500..=599) {
        return transport_error(error);
    }
    // Codex retries unexpected upgrade statuses, including gateway refusals of WebSockets.
    let body = response
        .body()
        .as_deref()
        .and_then(|body| serde_json::from_slice(body).ok())
        .unwrap_or(Value::Null);
    ResponseFacts::from_headers(status, response.headers())
        .failure_as(&body, ProviderErrorKind::Network)
        .into_error()
}

fn transport_error(error: &WsError) -> Error {
    if let WsError::Http(response) = error {
        let body = response
            .body()
            .as_deref()
            .and_then(|body| serde_json::from_slice(body).ok())
            .unwrap_or(Value::Null);
        return response_failure(
            &ResponseFacts::from_headers(response.status(), response.headers()),
            &body,
        )
        .into_error();
    }
    if network_policy_denial(error).is_some() {
        return Error::provider(
            ProviderErrorKind::BadRequest,
            "application network policy denied Responses WebSocket",
        );
    }
    Error::provider(
        ProviderErrorKind::Network,
        "Responses WebSocket transport failed",
    )
}

fn poisoned_cache() -> Error {
    Error::caller("Responses WebSocket session cache is poisoned")
}

fn closed_error() -> Error {
    Error::caller("Responses WebSocket provider has been closed")
}

fn timeout_error(operation: &str) -> Error {
    Error::provider(ProviderErrorKind::Timeout, format!("timed out {operation}"))
}

fn retry_delay(attempt: u32) -> Duration {
    let base = 200u128.saturating_mul(2u128.saturating_pow(attempt));
    let jitter = rand::rng().random_range(900..1100);
    Duration::from_millis(u64::try_from(base.saturating_mul(jitter) / 1000).unwrap_or(u64::MAX))
}

pub(super) fn observe_catalog_etags(
    frames: Frames,
    observer: Option<Arc<dyn super::ResponsesCatalogObserver>>,
) -> Frames {
    let Some(observer) = observer else {
        return frames;
    };
    frames
        .then(move |frame| {
            let observer = Arc::clone(&observer);
            async move {
                if let Ok(SseFrame::Data(event)) = &frame
                    && event["type"] == "codex.response.metadata"
                    && let Some(headers) = event["headers"].as_object()
                    && let Some(etag) = headers.iter().find_map(|(name, value)| {
                        name.eq_ignore_ascii_case("x-models-etag")
                            .then(|| value.as_str())
                            .flatten()
                    })
                {
                    observer.observe_models_etag(etag).await;
                }
                frame
            }
        })
        .boxed()
}
