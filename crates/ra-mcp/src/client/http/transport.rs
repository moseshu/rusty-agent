// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Native HTTP boundary for the SDK worker. Errors never retain requests, URLs or response bodies.

use super::{McpHttpParams, safe_error};
use futures::{StreamExt, stream::BoxStream};
use ra_core::error::{Result, ToolErrorKind};
use reqwest::{
    Method, RequestBuilder, Response, StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use rmcp::{
    model::{ClientJsonRpcMessage, ClientNotification, ClientRequest, ServerJsonRpcMessage},
    transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use sse_stream::{Sse, SseStream};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

mod sse;

type HttpResult<T> = std::result::Result<T, StreamableHttpError<io::Error>>;
type Events = BoxStream<'static, std::result::Result<Sse, SseError>>;
const DEFAULT_MAX_SSE_EVENT_SIZE: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct HttpSession(Mutex<SessionState>);
#[derive(Default)]
struct SessionState {
    id: Option<String>,
    cleanup_id: Option<String>,
    headers: HashMap<HeaderName, HeaderValue>,
    closed: bool,
}
impl HttpSession {
    fn state(&self) -> MutexGuard<'_, SessionState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(super) fn id(&self) -> Option<String> {
        self.state().id.clone()
    }
    pub(crate) fn clear(&self) {
        let mut state = self.state();
        state.id = None;
        state.closed = true;
    }
    fn capture(&self, response: &Response) {
        let id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut state = self.state();
        state.cleanup_id.clone_from(&id);
        if !state.closed {
            state.id = id;
        }
    }
}

#[derive(Clone)]
pub(super) struct HttpClient {
    client: reqwest::Client,
    routed: Option<ra_http::RouteAwareClientPool>,
    // Validated static headers; the routed pool applies them as the client defaults would.
    default_headers: HeaderMap,
    params: McpHttpParams,
    bearer: Option<String>,
    pub(super) session: Arc<HttpSession>,
}
impl HttpClient {
    pub(super) fn new(params: &McpHttpParams) -> Result<Self> {
        let url = url::Url::parse(&params.url)
            .map_err(|_| safe_error(ToolErrorKind::InvalidInput, "invalid MCP HTTP URL"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(safe_error(
                ToolErrorKind::InvalidInput,
                "invalid MCP HTTP URL",
            ));
        }
        let mut headers = HeaderMap::new();
        for (name, value) in &params.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                safe_error(ToolErrorKind::InvalidInput, "invalid MCP HTTP header name")
            })?;
            let mut value = HeaderValue::from_str(value).map_err(|_| {
                safe_error(ToolErrorKind::InvalidInput, "invalid MCP HTTP header value")
            })?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let bearer = params
            .bearer_token_env_var
            .as_ref()
            .map(|name| {
                let token = std::env::var(name)
                    .ok()
                    .filter(|token| !token.trim().is_empty())
                    .ok_or_else(|| {
                        safe_error(
                            ToolErrorKind::InvalidInput,
                            "MCP bearer token environment variable is missing or empty",
                        )
                    })?;
                HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                    safe_error(ToolErrorKind::InvalidInput, "invalid MCP bearer token")
                })?;
                Ok::<_, ra_core::error::Error>(token)
            })
            .transpose()?;
        crate::rustls_provider::ensure_rustls_crypto_provider();
        let default_headers = headers.clone();
        let builder = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(params.timeout)
            .read_timeout(params.sse_read_timeout)
            .redirect(reqwest::redirect::Policy::none());
        let client = match &params.client_factory {
            Some(factory) => factory(builder),
            None => builder.build(),
        }
        .map_err(|_| {
            safe_error(
                ToolErrorKind::ExecutionFailed,
                "MCP HTTP client construction failed",
            )
        })?;
        let routed = params.route_factory.clone().map(|factory| {
            ra_http::RouteAwareClientPool::with_builder(
                factory,
                ra_http::ClientRouteClass::Api,
                ra_http::HttpClientBuilder::new()
                    .without_redirects()
                    .without_request_logging()
                    .connect_timeout(params.timeout)
                    .read_timeout(params.sse_read_timeout),
            )
            .with_legacy_custom_ca_fallback()
        });
        Ok(Self {
            client,
            routed,
            default_headers,
            params: params.clone(),
            bearer,
            session: Arc::default(),
        })
    }
    async fn request(
        &self,
        method: Method,
        uri: &str,
        session: Option<&str>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<RequestBuilder> {
        let mut request = self
            .client
            .request(method, uri)
            .headers(headers.into_iter().collect());
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        if let Some(auth) = &self.params.auth {
            request = auth(request)
                .await
                .map_err(|_| failure("MCP HTTP authentication failed"))?;
        }
        if let Some(token) = &self.bearer {
            // Header setters append. Insert on the built request so explicit credentials replace
            // an auth hook's Authorization rather than sending both values.
            let (client, built) = request.build_split();
            let mut built = built.map_err(http_error)?;
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| failure("invalid MCP bearer token"))?;
            value.set_sensitive(true);
            built.headers_mut().insert(AUTHORIZATION, value);
            request = RequestBuilder::from_parts(client, built);
        }
        Ok(request)
    }
    async fn send(&self, request: RequestBuilder) -> HttpResult<Response> {
        let Some(pool) = &self.routed else {
            return request.send().await.map_err(http_error);
        };
        let request = request.build().map_err(http_error)?;
        // The routed pool does not see this client's default headers. Static headers fill only
        // names the request does not set, as reqwest's defaults do, so session, hook and bearer
        // values keep their precedence.
        let mut headers = request.headers().clone();
        for (name, value) in &self.default_headers {
            if !headers.contains_key(name) {
                headers.insert(name.clone(), value.clone());
            }
        }
        let mut routed = pool
            .request(request.method().clone(), request.url().as_str())
            .headers(headers);
        if let Some(body) = request.body() {
            let body = body
                .as_bytes()
                .ok_or_else(|| failure("routed MCP requests require a buffered body"))?;
            routed = routed.body(body.to_vec());
        }
        if let Some(timeout) = request.timeout() {
            routed = routed.timeout(*timeout);
        }
        let response = routed
            .send()
            .await
            .map_err(|_| failure("MCP HTTP request failed"))?;
        let mut builder = http::Response::builder().status(response.status());
        if let Some(headers) = builder.headers_mut() {
            headers.clone_from(response.headers());
        }
        let body = reqwest::Body::wrap_stream(response.bytes_stream());
        let response = builder
            .body(body)
            .map_err(|_| failure("MCP HTTP response adaptation failed"))?;
        Ok(reqwest::Response::from(response))
    }

    async fn post(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        headers: HashMap<HeaderName, HeaderValue>,
        max_event_size: usize,
    ) -> HttpResult<StreamableHttpPostResponse> {
        let is_initialize = matches!(&message, ClientJsonRpcMessage::Request(request)
            if matches!(request.request, ClientRequest::InitializeRequest(_)));
        let is_initialized = matches!(&message, ClientJsonRpcMessage::Notification(notification)
            if matches!(notification.notification, ClientNotification::InitializedNotification(_)));
        if is_initialized {
            self.session.state().headers.clone_from(&headers);
        }
        let send = async {
            let request = self
                .request(Method::POST, &uri, session.as_deref(), headers)
                .await?
                .header(ACCEPT, "application/json, text/event-stream");
            let response = self.send(request.json(&message)).await?;
            let status = response.status();
            let session_id = response
                .headers()
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if status.is_client_error() || status.is_server_error() {
                // As in the MCP Python SDK, a request's non-2xx JSON body may be the server's
                // JSON-RPC error. It is a protocol answer, so only other bodies are discarded.
                if let ClientJsonRpcMessage::Request(request) = &message
                    && media_type(&response).as_deref() == Some("application/json")
                    && let Ok(body) = response.bytes().await
                    && let Ok(ServerJsonRpcMessage::Error(mut error)) =
                        serde_json::from_slice::<ServerJsonRpcMessage>(&body)
                {
                    // The server may omit the ID when it rejects a request before reading it.
                    error.id = Some(request.id.clone());
                    return Ok(StreamableHttpPostResponse::Json(
                        ServerJsonRpcMessage::Error(error),
                        session_id,
                    ));
                }
                if status == StatusCode::NOT_FOUND && session.is_some() {
                    return Err(StreamableHttpError::SessionExpired);
                }
                return Err(status_error(status));
            }
            check_status(status)?;
            if is_initialize {
                self.session.capture(&response);
            }
            if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
                return Ok(StreamableHttpPostResponse::Accepted);
            }
            let no_reply = !matches!(message, ClientJsonRpcMessage::Request(_));
            if no_reply && response.content_length() == Some(0) {
                return Ok(StreamableHttpPostResponse::Accepted);
            }
            match media_type(&response).as_deref() {
                Some("text/event-stream") => {
                    let stream = events(response, max_event_size, is_initialize);
                    if is_initialize {
                        initialize_response(stream, session_id).await
                    } else {
                        Ok(StreamableHttpPostResponse::Sse(stream, session_id))
                    }
                }
                Some("application/json") => {
                    let body = response.bytes().await.map_err(http_error)?;
                    match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                        Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session_id)),
                        Err(_) if no_reply => Ok(StreamableHttpPostResponse::Accepted),
                        Err(_) => Err(failure("invalid MCP HTTP JSON response")),
                    }
                }
                _ => Err(failure("unexpected MCP HTTP content type")),
            }
        }
        .await;
        match send {
            Err(_) if is_initialized && self.params.ignore_initialized_notification_failure => {
                tracing::warn!("Ignoring initialized notification HTTP failure");
                Ok(StreamableHttpPostResponse::Accepted)
            }
            result => result,
        }
    }
}

impl StreamableHttpClient for HttpClient {
    type Error = io::Error;
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        _auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<StreamableHttpPostResponse> {
        self.post(uri, message, session, headers, DEFAULT_MAX_SSE_EVENT_SIZE)
            .await
    }
    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        _auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max_event_size: usize,
    ) -> HttpResult<StreamableHttpPostResponse> {
        self.post(uri, message, session, headers, max_event_size)
            .await
    }
    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last_id: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<Events> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session,
            last_id,
            auth,
            headers,
            DEFAULT_MAX_SSE_EVENT_SIZE,
        )
        .await
    }
    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last_id: Option<String>,
        _auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max_event_size: usize,
    ) -> HttpResult<Events> {
        let mut request = self
            .request(Method::GET, &uri, session.as_deref(), headers)
            .await?
            .header(ACCEPT, "text/event-stream");
        if let Some(last_id) = last_id {
            request = request.header("last-event-id", last_id);
        }
        let response = self.send(request).await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        check_status(response.status())?;
        if media_type(&response).as_deref() != Some("text/event-stream") {
            return Err(failure("unexpected MCP HTTP stream content type"));
        }
        Ok(events(response, max_event_size, false))
    }
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        _auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<()> {
        // Claim deletion before awaiting: failed initialization and SDK shutdown share ownership.
        self.session.state().cleanup_id = None;
        if !self.params.terminate_on_close {
            return Ok(());
        }
        let request = self
            .request(Method::DELETE, &uri, Some(&session), headers)
            .await?;
        let response = self.send(request).await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        check_status(response.status())
    }
}

// Media types are case-insensitive; Python lower-cases the header before matching.
fn media_type(response: &Response) -> Option<String> {
    response
        .headers()
        .get(CONTENT_TYPE)?
        .to_str()
        .ok()?
        .split(';')
        .next()
        .map(|media_type| media_type.trim().to_ascii_lowercase())
}
fn failure(message: &'static str) -> StreamableHttpError<io::Error> {
    StreamableHttpError::Client(io::Error::other(message))
}
// Consume the original error so no credential-bearing request survives in its mapped source.
#[allow(clippy::needless_pass_by_value)]
fn http_error(error: reqwest::Error) -> StreamableHttpError<io::Error> {
    let (kind, message) = if error.is_timeout() {
        (io::ErrorKind::TimedOut, "MCP HTTP request timed out")
    } else if error.is_connect() {
        (
            io::ErrorKind::ConnectionRefused,
            "MCP HTTP connection failed",
        )
    } else {
        (io::ErrorKind::Other, "MCP HTTP request failed")
    };
    StreamableHttpError::Client(io::Error::new(kind, message))
}
fn check_status(status: StatusCode) -> HttpResult<()> {
    if status.is_success() {
        Ok(())
    } else {
        Err(status_error(status))
    }
}
fn status_error(status: StatusCode) -> StreamableHttpError<io::Error> {
    StreamableHttpError::Client(io::Error::other(format!(
        "MCP HTTP request failed with status {}",
        status.as_u16()
    )))
}

// Adapt the SDK's initialize SSE reader to accept error responses as Python does. The SDK only
// returns success responses here and otherwise drains protocol errors until the stream closes.
async fn initialize_response(
    mut stream: Events,
    session_id: Option<String>,
) -> HttpResult<StreamableHttpPostResponse> {
    while let Some(event) = stream.next().await {
        let event = event.map_err(StreamableHttpError::Sse)?;
        let payload = event.data.unwrap_or_default();
        if payload.trim().is_empty() {
            continue;
        }
        let message = serde_json::from_str::<ServerJsonRpcMessage>(&payload)
            .map_err(|_| failure("invalid MCP HTTP SSE message"))?;
        if matches!(
            message,
            ServerJsonRpcMessage::Response(_) | ServerJsonRpcMessage::Error(_)
        ) {
            return Ok(StreamableHttpPostResponse::Json(message, session_id));
        }
    }
    Err(failure(
        "MCP HTTP initialize stream ended without a response",
    ))
}

fn events(response: Response, maximum: usize, initializing: bool) -> Events {
    let bytes = response.bytes_stream().scan(
        (sse::EventSizeLimit::new(maximum), false),
        |(limit, failed), chunk| {
            if *failed {
                return futures::future::ready(None);
            }
            let chunk = match chunk {
                Ok(chunk) if limit.observe(&chunk).is_ok() => Ok(chunk),
                Ok(_) => Err(io::Error::other(
                    "MCP HTTP SSE event exceeded its size limit",
                )),
                Err(_) => Err(io::Error::other("MCP HTTP stream read failed")),
            };
            *failed = chunk.is_err();
            futures::future::ready(Some(chunk))
        },
    );
    SseStream::from_bytes_stream(bytes)
        .map(|event| {
            let event = event.map_err(|_| {
                SseError::Body(Box::new(io::Error::other(
                    "MCP HTTP stream read or parse failed",
                )))
            })?;
            // The SDK parses message events too; reject invalid payloads here so its parser cannot
            // attach credential-bearing server data to a transport diagnostic.
            if is_message_event(&event)
                && let Some(data) = &event.data
                && !data.trim().is_empty()
                && serde_json::from_str::<ServerJsonRpcMessage>(data).is_err()
            {
                return Err(SseError::Body(Box::new(io::Error::other(
                    "invalid MCP HTTP SSE message",
                ))));
            }
            Ok(event)
        })
        .filter(move |event| {
            // The SDK's initialize reader parses every nonempty data field without checking the
            // event type. Established streams have a connector that consumes control frames.
            futures::future::ready(!initializing || event.as_ref().map_or(true, is_message_event))
        })
        .boxed()
}

fn is_message_event(event: &Sse) -> bool {
    matches!(event.event.as_deref(), None | Some("" | "message"))
}

// The SDK's worker returns before its normal DELETE block when initialization fails. Python's
// transport context still terminates that session. Keep the same ownership on error or drop.
pub(super) struct ConnectGuard {
    client: Option<HttpClient>,
    runtime: Option<tokio::runtime::Handle>,
}
impl ConnectGuard {
    pub(super) fn new(client: HttpClient) -> Self {
        Self {
            client: Some(client),
            runtime: tokio::runtime::Handle::try_current().ok(),
        }
    }
    pub(super) fn disarm(&mut self) {
        self.client = None;
    }
    // Python's transport logs a failed DELETE and keeps the connection error, so this only
    // reports cleanup failure through the log.
    pub(super) async fn cleanup(&mut self) {
        if let Some(cleanup) = self.start_cleanup()
            && cleanup.await.is_err()
        {
            tracing::warn!("MCP HTTP initialize cleanup task failed");
        }
    }
    fn start_cleanup(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let client = self.client.take()?;
        client.session.clear();
        let (id, headers) = {
            let mut state = client.session.state();
            (state.cleanup_id.take(), state.headers.clone())
        };
        let id = id?;
        let Some(runtime) = &self.runtime else {
            tracing::warn!("MCP HTTP initialize cleanup requires an active runtime");
            return None;
        };
        // Transfer ownership before awaiting. Dropping the JoinHandle detaches this bounded task,
        // so cancellation of connect cannot interrupt an auth hook or abandon its session.
        Some(runtime.spawn(async move {
            let cleanup =
                client.delete_session(client.params.url.clone().into(), id.into(), None, headers);
            if !matches!(
                tokio::time::timeout(Duration::from_secs(5), cleanup).await,
                Ok(Ok(()))
            ) {
                tracing::warn!("MCP HTTP initialize cleanup failed");
            }
        }))
    }
}
impl Drop for ConnectGuard {
    fn drop(&mut self) {
        drop(self.start_cleanup());
    }
}
