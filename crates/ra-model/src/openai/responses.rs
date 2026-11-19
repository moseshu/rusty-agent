//! `/v1/responses` protocol implementation.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::{StreamExt, stream as futures_stream};
use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    item::{ContentBlock, Message, MessageRole, TextBlock},
    model::{
        Model, ModelProvider, ModelRequest, ModelRetryAdviceRequest, ModelStream, RetryAdvice,
        stamp_replay_safety,
    },
    session::SessionId,
};

use ra_http::{HttpClient, HttpError, HttpResponse, RetryAfter};
use rand::Rng;

use super::{auth::OpenAiAuth, error::ResponseFacts, sse::Terminator};
use crate::{provider::quirks::ProviderQuirks, retry::unstarted_replay_safety};

pub(crate) mod cache;
pub(crate) mod convert;
pub(crate) mod mcp;
pub(crate) mod request;
pub(crate) mod stream;
pub mod web_search;
mod websocket;

const PREFIX_ITEM_ID: &str = "prefix_item_id";

/// Builds a developer prefix whose Responses item ID is stable within a session
/// for the same text, following Codex's prompt-only item identity.
#[must_use]
pub fn developer_prefix(session: &SessionId, text: String) -> Message {
    let namespace = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, session.as_str().as_bytes());
    let id = uuid::Uuid::new_v5(&namespace, text.as_bytes());
    Message::new(
        MessageRole::Developer,
        vec![ContentBlock::Text(
            TextBlock::new(text).with_provider_metadata(
                super::prompt_cache::OPENAI_METADATA_NAMESPACE,
                serde_json::json!({(PREFIX_ITEM_ID): format!("msg_{id}")}),
            ),
        )],
    )
}

/// Supplies a per-provider bearer credential and refreshes it after a rejected request.
#[async_trait]
pub trait ResponsesBearerToken: Send + Sync {
    /// Obtains the current token or refreshes the token identified by `rejected`.
    async fn token(&self, rejected: Option<&str>) -> Result<String>;
}

/// Adds credentials that accompany the provider's primary authentication, such as a gateway's
/// OAuth token, to each Responses request.
///
/// Codex composes these onto the primary credential when it builds a request's authentication:
/// the primary bearer stays in `Authorization` and the extra credential travels in its own header
/// or cookie. The source is applied once per request, to HTTP requests and to the WebSocket
/// handshake alike. As in Codex, an open WebSocket connection stays in use when only this
/// credential changes; a new connection's handshake carries the current value. An HTTP request
/// that already has one of the source's headers fails, while a WebSocket handshake keeps the
/// header it already has, as Codex adds the credential to a handshake only where it is absent.
/// Either fails when the source supplies `Authorization` alongside a primary bearer.
#[async_trait]
pub trait ResponsesAuthHeaders: Send + Sync {
    /// Adds this source's credentials to the request headers. Fails, without sending anything,
    /// when a header it owns is already present or the credential cannot be obtained.
    async fn apply_auth_headers(&self, headers: &mut reqwest::header::HeaderMap) -> Result<()>;
}

/// The final HTTP request a [`ResponsesRequestSigner`] authenticates.
///
/// The body is the exact byte sequence that will be sent and the URL already carries the query.
#[derive(Clone, Debug)]
pub struct ResponsesSignableRequest {
    method: reqwest::Method,
    url: String,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

impl ResponsesSignableRequest {
    /// Creates the parts of one request attempt.
    #[must_use]
    pub const fn new(
        method: reqwest::Method,
        url: String,
        headers: reqwest::header::HeaderMap,
        body: Vec<u8>,
    ) -> Self {
        Self {
            method,
            url,
            headers,
            body,
        }
    }

    /// Request method.
    #[must_use]
    pub const fn method(&self) -> &reqwest::Method {
        &self.method
    }

    /// Absolute URL including the query string.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Headers that would be sent without signing.
    #[must_use]
    pub const fn headers(&self) -> &reqwest::header::HeaderMap {
        &self.headers
    }

    /// Request body bytes.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Takes the method, URL, headers, and body.
    #[must_use]
    pub fn into_parts(self) -> (reqwest::Method, String, reqwest::header::HeaderMap, Vec<u8>) {
        (self.method, self.url, self.headers, self.body)
    }
}

/// A signed request: these exact headers replace the unsigned ones.
#[derive(Clone, Debug)]
pub struct ResponsesSignedRequest {
    url: String,
    headers: reqwest::header::HeaderMap,
}

impl ResponsesSignedRequest {
    /// Creates signed request parts.
    #[must_use]
    pub const fn new(url: String, headers: reqwest::header::HeaderMap) -> Self {
        Self { url, headers }
    }

    /// URL to send to.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Complete header set to send.
    #[must_use]
    pub const fn headers(&self) -> &reqwest::header::HeaderMap {
        &self.headers
    }
}

/// An authentication failure a [`ResponsesRequestSigner`] may recover from.
#[derive(Debug)]
#[non_exhaustive]
pub enum ResponsesAuthFailure<'a> {
    /// Signing the request failed.
    Signing(&'a Error),
    /// The endpoint rejected the signed request with 401 or 403.
    Response {
        /// The signed request the endpoint rejected, including the credential it carried.
        request: &'a ResponsesSignedRequest,
        /// Response status.
        status: reqwest::StatusCode,
        /// Response body text.
        body: &'a str,
    },
}

/// Authenticates each Responses HTTP request from its final bytes, such as AWS `SigV4`.
///
/// A signer replaces bearer authentication for the provider it is installed on.
#[async_trait]
pub trait ResponsesRequestSigner: Send + Sync {
    /// Signs one request attempt.
    async fn sign(&self, request: ResponsesSignableRequest) -> Result<ResponsesSignedRequest>;

    /// Attempts credential recovery after `failure`; `Ok(true)` retries the request once.
    async fn recover(&self, failure: ResponsesAuthFailure<'_>) -> Result<bool>;

    /// Replaces the provider message of an auth failure the user can act on.
    fn failure_message(&self, _status: reqwest::StatusCode, _body: &str) -> Option<String> {
        None
    }
}

/// Receives a Responses provider's advertised model-catalog revision.
#[async_trait]
pub trait ResponsesCatalogObserver: Send + Sync {
    /// Revalidates the catalog against the revision advertised by the provider.
    async fn observe_models_etag(&self, etag: &str);
}

#[derive(Clone)]
struct ResponsesTransport {
    auth: OpenAiAuth,
    client: HttpClient,
    stream_idle_timeout: Option<Duration>,
    request_max_retries: u32,
    query: BTreeMap<String, String>,
    bearer: Option<Arc<dyn ResponsesBearerToken>>,
    auth_headers: Option<Arc<dyn ResponsesAuthHeaders>>,
    signer: Option<Arc<dyn ResponsesRequestSigner>>,
    catalog_observer: Option<Arc<dyn ResponsesCatalogObserver>>,
    websocket: Option<Arc<websocket::WebSocketTransport>>,
}

impl ResponsesTransport {
    /// Adds the accompanying credentials to `headers`, which must not carry `Authorization`.
    ///
    /// Codex composes them onto the primary credential and rejects one that would use a header
    /// the primary credential sends; here that is the bearer's `Authorization`.
    async fn apply_auth_headers(&self, headers: &mut reqwest::header::HeaderMap) -> Result<()> {
        let Some(source) = &self.auth_headers else {
            return Ok(());
        };
        source.apply_auth_headers(headers).await?;
        if headers.contains_key(reqwest::header::AUTHORIZATION)
            && (self.bearer.is_some() || self.auth.api_key().is_some())
        {
            return Err(Error::config(
                "accompanying credentials conflict with the primary Authorization header",
            ));
        }
        Ok(())
    }
}

/// `OpenAI` Responses provider with a shared HTTP client and per-model instance cache.
pub struct OpenAiResponsesProvider {
    transport: Arc<ResponsesTransport>,
    default_model: String,
    quirks: ProviderQuirks,
    models: Mutex<BTreeMap<String, Arc<dyn Model>>>,
}

impl OpenAiResponsesProvider {
    /// Creates a provider using the supplied default model.
    pub fn new(auth: OpenAiAuth, default_model: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(super::error::transport_error)?;
        Self::from_transport(
            auth,
            default_model.into(),
            HttpClient::new_without_request_logging(client),
            None,
            0,
        )
    }

    /// Creates a provider on a supplied HTTP route, with SSE inactivity and HTTP retry limits.
    pub fn with_http_client(
        auth: OpenAiAuth,
        default_model: impl Into<String>,
        client: HttpClient,
        stream_idle_timeout: Duration,
        request_max_retries: u32,
    ) -> Result<Self> {
        Self::from_transport(
            auth,
            default_model.into(),
            client,
            Some(stream_idle_timeout),
            request_max_retries,
        )
    }

    fn from_transport(
        auth: OpenAiAuth,
        default_model: String,
        client: HttpClient,
        stream_idle_timeout: Option<Duration>,
        request_max_retries: u32,
    ) -> Result<Self> {
        auth.validate()?;
        if default_model.trim().is_empty() {
            return Err(Error::config("OpenAI default model must not be empty"));
        }
        Ok(Self {
            transport: Arc::new(ResponsesTransport {
                auth,
                client,
                stream_idle_timeout,
                request_max_retries,
                query: BTreeMap::new(),
                bearer: None,
                auth_headers: None,
                signer: None,
                catalog_observer: None,
                websocket: None,
            }),
            default_model,
            quirks: ProviderQuirks::new(),
            models: Mutex::new(BTreeMap::new()),
        })
    }

    /// Prefers Responses over WebSocket for streamed calls, with HTTP/SSE fallback.
    #[must_use]
    pub fn with_websocket(
        mut self,
        connector: ra_http::websocket::WebSocketConnector,
        connect_timeout: Duration,
    ) -> Self {
        if self.transport.signer.is_some() {
            return self;
        }
        Arc::make_mut(&mut self.transport).websocket = Some(Arc::new(
            websocket::WebSocketTransport::new(connector, connect_timeout),
        ));
        self
    }

    /// Sets endpoint query parameters. Request settings can override individual keys.
    #[must_use]
    pub fn with_default_query(mut self, query: BTreeMap<String, String>) -> Self {
        Arc::make_mut(&mut self.transport).query = query;
        self
    }

    /// Installs a consumer for model-catalog revision notices from Responses.
    #[must_use]
    pub fn with_catalog_observer(mut self, observer: Arc<dyn ResponsesCatalogObserver>) -> Self {
        Arc::make_mut(&mut self.transport).catalog_observer = Some(observer);
        self
    }

    /// Supplies credentials dynamically for this endpoint.
    #[must_use]
    pub fn with_bearer_token(mut self, bearer: Arc<dyn ResponsesBearerToken>) -> Self {
        Arc::make_mut(&mut self.transport).bearer = Some(bearer);
        self
    }

    /// Adds credentials that accompany the primary authentication to every request.
    #[must_use]
    pub fn with_auth_headers(mut self, source: Arc<dyn ResponsesAuthHeaders>) -> Self {
        Arc::make_mut(&mut self.transport).auth_headers = Some(source);
        self
    }

    /// Authenticates HTTP requests by signing their final bytes instead of with a bearer token.
    ///
    /// Signed providers do not use Responses over WebSocket, whose upgrade request cannot carry a
    /// body signature.
    #[must_use]
    pub fn with_request_signer(mut self, signer: Arc<dyn ResponsesRequestSigner>) -> Self {
        let transport = Arc::make_mut(&mut self.transport);
        transport.signer = Some(signer);
        transport.websocket = None;
        self
    }

    /// Declares the endpoint capabilities this provider may use.
    ///
    /// A registry passes the registration's declaration here. Constructed directly, the default is
    /// every capability off, so a provider pointed at an unknown gateway sends nothing beyond what
    /// the protocol itself defines.
    #[must_use]
    pub const fn with_quirks(mut self, quirks: ProviderQuirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// Default provider-facing model identifier.
    #[must_use]
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    fn lock_models(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Arc<dyn Model>>>> {
        self.models
            .lock()
            .map_err(|_| Error::provider(ProviderErrorKind::Behavior, "model cache lock poisoned"))
    }
}

impl fmt::Debug for OpenAiResponsesProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesProvider")
            .field("base_url", &self.transport.auth.base_url())
            .field("default_model", &self.default_model)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelProvider for OpenAiResponsesProvider {
    fn get_model(&self, model_name: Option<&str>) -> Result<Arc<dyn Model>> {
        let model_name = model_name.unwrap_or(&self.default_model).trim();
        if model_name.is_empty() {
            return Err(Error::config("OpenAI model name must not be empty"));
        }

        let mut models = self.lock_models()?;
        if let Some(model) = models.get(model_name) {
            return Ok(Arc::clone(model));
        }
        let model: Arc<dyn Model> = Arc::new(OpenAiResponsesModel {
            model: model_name.to_owned(),
            transport: Arc::clone(&self.transport),
            quirks: self.quirks,
        });
        models.insert(model_name.to_owned(), Arc::clone(&model));
        Ok(model)
    }

    async fn close(&self) -> Result<()> {
        self.transport
            .websocket
            .as_ref()
            .map_or(Ok(()), |websocket| websocket.close())
    }
}

/// One model accessed through `OpenAI` Responses.
#[derive(Clone)]
pub struct OpenAiResponsesModel {
    model: String,
    transport: Arc<ResponsesTransport>,
    quirks: ProviderQuirks,
}

impl OpenAiResponsesModel {
    /// Creates a directly usable model without a provider registry.
    pub fn new(model: impl Into<String>, auth: OpenAiAuth) -> Result<Self> {
        let provider = OpenAiResponsesProvider::new(auth, model)?;
        Ok(Self {
            model: provider.default_model,
            transport: provider.transport,
            quirks: provider.quirks,
        })
    }

    /// Declares the endpoint capabilities this model may use.
    #[must_use]
    pub const fn with_quirks(mut self, quirks: ProviderQuirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// Provider-facing model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Sends the Responses POST. A replay of a stored response could advance provider state.
    async fn send(&self, request: &ModelRequest, streaming: bool) -> Result<HttpResponse> {
        let body =
            request::build_request_body(&self.model, request, self.quirks, streaming).await?;
        let retries = if body["store"] == false
            && matches!(
                request.continuation(),
                ra_core::model::ConversationContinuation::None
            ) {
            self.transport.request_max_retries
        } else {
            0
        };
        let mut headers = super::transport_headers(
            &self.transport.auth,
            request.model_settings().extra_headers(),
        )?;
        self.transport.apply_auth_headers(&mut headers).await?;
        if let Some(signer) = &self.transport.signer {
            return self
                .send_signed(signer.as_ref(), request, &body, headers, retries)
                .await;
        }
        let mut token = match &self.transport.bearer {
            Some(bearer) => Some(bearer.token(None).await?),
            None => None,
        };
        let mut attempt = 0;
        let mut refreshed = false;
        loop {
            let key = token.as_deref().or(self.transport.auth.api_key());
            let response = self.send_http(request, &body, headers.clone(), key).await;
            if !refreshed
                && let Some(bearer) = &self.transport.bearer
                && response
                    .as_ref()
                    .is_ok_and(|response| response.status() == reqwest::StatusCode::UNAUTHORIZED)
            {
                drop(response);
                token = Some(bearer.token(token.as_deref()).await?);
                refreshed = true;
                continue;
            }
            let server_delay = match &response {
                Ok(response) if response.status().is_server_error() => {
                    RetryAfter::from_headers(response.headers())
                }
                Err(HttpError::Request(error)) if !error.is_builder() => None,
                Err(HttpError::Timeout) => None,
                _ => return response.map_err(super::error::http_transport_error),
            };
            if attempt >= retries {
                return response.map_err(super::error::http_transport_error);
            }
            drop(response);
            backoff(attempt, server_delay).await;
            attempt += 1;
        }
    }

    /// Sends with a request signer, following Codex's per-attempt `apply_auth` and its single
    /// provider recovery after a refreshable auth failure.
    async fn send_signed(
        &self,
        signer: &dyn ResponsesRequestSigner,
        request: &ModelRequest,
        body: &serde_json::Value,
        headers: reqwest::header::HeaderMap,
        retries: u32,
    ) -> Result<HttpResponse> {
        let body = serde_json::to_vec(body).map_err(|error| {
            Error::config("failed to encode the Responses request").with_source(error)
        })?;
        let (url, unsigned) = self.signable_target(request, headers)?;
        let mut attempt = 0;
        let mut recovered = false;
        loop {
            let authorized = signer
                .sign(ResponsesSignableRequest::new(
                    reqwest::Method::POST,
                    url.clone(),
                    unsigned.clone(),
                    body.clone(),
                ))
                .await;
            let authorized = match authorized {
                Ok(authorized) => authorized,
                Err(error) => {
                    if !recovered
                        && signer
                            .recover(ResponsesAuthFailure::Signing(&error))
                            .await?
                    {
                        recovered = true;
                        continue;
                    }
                    if error.is_retryable() && attempt < retries {
                        backoff(attempt, None).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(error);
                }
            };
            let mut http_request = self
                .transport
                .client
                .post(authorized.url.clone())
                .headers(authorized.headers.clone())
                .body(body.clone());
            if let Some(timeout) = request.model_settings().timeout() {
                http_request = http_request.timeout(timeout);
            }
            let response = match http_request.send().await {
                Ok(response)
                    if matches!(
                        response.status(),
                        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
                    ) =>
                {
                    let facts = ResponseFacts::from_headers(response.status(), response.headers());
                    let status = facts.status();
                    let text = match response.bytes().await {
                        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                        Err(error) => {
                            return Err(super::error::response_failure(
                                &facts,
                                &serde_json::Value::Null,
                            )
                            .with_source(error.without_url())
                            .into_error());
                        }
                    };
                    if !recovered
                        && signer
                            .recover(ResponsesAuthFailure::Response {
                                request: &authorized,
                                status,
                                body: &text,
                            })
                            .await?
                    {
                        recovered = true;
                        continue;
                    }
                    return Err(signed_auth_failure(signer, &facts, &text));
                }
                response => response,
            };
            let server_delay = match &response {
                Ok(response) if response.status().is_server_error() => {
                    RetryAfter::from_headers(response.headers())
                }
                Err(HttpError::Request(error)) if !error.is_builder() => None,
                Err(HttpError::Timeout) => None,
                _ => return response.map_err(super::error::http_transport_error),
            };
            if attempt >= retries {
                return response.map_err(super::error::http_transport_error);
            }
            drop(response);
            backoff(attempt, server_delay).await;
            attempt += 1;
        }
    }

    /// Builds the URL with its query and the headers a signer authenticates.
    fn signable_target(
        &self,
        request: &ModelRequest,
        headers: reqwest::header::HeaderMap,
    ) -> Result<(String, reqwest::header::HeaderMap)> {
        let mut url = reqwest::Url::parse(&format!("{}/responses", self.transport.auth.base_url()))
            .map_err(|error| Error::config("invalid Responses endpoint URL").with_source(error))?;
        let query = self.query(request);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(&query);
        }
        let mut unsigned = reqwest::header::HeaderMap::new();
        unsigned.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        unsigned.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(concat!(
                "rusty-agent/",
                env!("CARGO_PKG_VERSION")
            )),
        );
        // Like the unsigned path, configured and per-request headers replace the defaults.
        unsigned.extend(headers);
        Ok((url.to_string(), unsigned))
    }

    async fn send_http(
        &self,
        request: &ModelRequest,
        body: &serde_json::Value,
        headers: reqwest::header::HeaderMap,
        key: Option<&str>,
    ) -> std::result::Result<HttpResponse, HttpError> {
        let mut http_request = self
            .transport
            .client
            .post(format!("{}/responses", self.transport.auth.base_url()))
            .header("content-type", "application/json")
            .header(
                "user-agent",
                concat!("rusty-agent/", env!("CARGO_PKG_VERSION")),
            )
            .headers(headers);
        if let Some(api_key) = key {
            http_request = http_request.bearer_auth(api_key);
        }
        let query = self.query(request);
        if !query.is_empty() {
            http_request = http_request.query(&query);
        }
        if let Some(timeout) = request.model_settings().timeout() {
            http_request = http_request.timeout(timeout);
        }
        http_request.json(body).send().await
    }

    fn query(&self, request: &ModelRequest) -> BTreeMap<String, String> {
        let mut query = self.transport.query.clone();
        query.extend(
            request
                .model_settings()
                .extra_query()
                .iter()
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned);
                    (key.clone(), value)
                }),
        );
        query
    }

    async fn observe_catalog_header(&self, response: &HttpResponse) {
        if let Some(observer) = &self.transport.catalog_observer
            && let Some(etag) = response
                .headers()
                .get("x-models-etag")
                .and_then(|value| value.to_str().ok())
        {
            observer.observe_models_etag(etag).await;
        }
    }

    async fn fetch(&self, request: ModelRequest) -> Result<ra_core::item::ModelResponse> {
        let response = self.send(&request, false).await?;
        let facts = ResponseFacts::read(&response);
        let status = facts.status();
        if status.is_success() {
            self.observe_catalog_header(&response).await;
        }
        let payload = match response.json::<serde_json::Value>().await {
            Ok(payload) => payload,
            Err(error) if !status.is_success() => {
                return Err(
                    super::error::response_failure(&facts, &serde_json::Value::Null)
                        .with_source(error)
                        .into_error(),
                );
            }
            Err(error) => return Err(super::error::http_decode_error(error)),
        };
        if !status.is_success() {
            return Err(super::error::response_failure(&facts, &payload).into_error());
        }
        convert::convert_response(
            &payload,
            facts.into_request_id(),
            request.handoffs(),
            request.model_settings().provider(),
            &self.model,
        )
    }
}

impl fmt::Debug for OpenAiResponsesModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesModel")
            .field("model", &self.model)
            .field("base_url", &self.transport.auth.base_url())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Model for OpenAiResponsesModel {
    fn supports_default_prompt_cache_key(
        &self,
        settings: &ra_core::model::ResolvedModelSettings,
    ) -> bool {
        self.quirks.prompt_cache_key() && !settings.extra_body().contains_key("prompt_cache_key")
    }

    async fn get_response(&self, request: ModelRequest) -> Result<ra_core::item::ModelResponse> {
        // A non-streaming call publishes nothing until it returns, so no consumer saw a partial
        // turn. What the endpoint may have recorded on its own side is the other half of the
        // question, and the continuation is what answers it — this protocol is the one that can
        // carry a server-managed conversation, so it is also the one where that matters.
        let unstarted = unstarted_replay_safety(request.continuation());
        self.fetch(request)
            .await
            .map_err(|error| stamp_replay_safety(error, unstarted))
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        let model = self.clone();
        futures_stream::once(async move {
            let provider = request.model_settings().provider().clone();
            let handoffs = request.handoffs().to_vec();
            let unstarted = unstarted_replay_safety(request.continuation());
            if let Some(websocket) = &model.transport.websocket {
                match websocket.start(&model, &request).await {
                    Ok(Some(frames)) => {
                        return stream::events(
                            websocket::observe_catalog_etags(
                                frames,
                                model.transport.catalog_observer.clone(),
                            ),
                            provider,
                            model.model.clone(),
                            handoffs,
                            None,
                            unstarted,
                        )
                        .map(|event| {
                            // The first response event already arrived, as the SDK marks a
                            // WebSocket failure after any received event.
                            event.map_err(|error| {
                                ra_core::model::stamp_response_started(stamp_replay_safety(
                                    error,
                                    ra_core::model::ReplaySafety::Unsafe,
                                ))
                            })
                        })
                        .boxed();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let safety = ra_core::model::replay_safety_of(&error);
                        return super::sse::error_stream(error, safety);
                    }
                }
            }
            match model.send(&request, true).await {
                Ok(response) if response.status().is_success() => {
                    model.observe_catalog_header(&response).await;
                    // Read before the body is consumed: the terminal response carries it, and no
                    // frame in the stream mentions it.
                    let request_id = ResponseFacts::read(&response).into_request_id();
                    match super::sse::ensure_event_stream(&response, "Responses") {
                        Ok(()) => stream::events(
                            super::sse::frames(
                                response,
                                Terminator::default(),
                                model.transport.stream_idle_timeout,
                            ),
                            provider,
                            model.model.clone(),
                            handoffs,
                            request_id,
                            unstarted,
                        ),
                        Err(error) => super::sse::error_stream(error, unstarted),
                    }
                }
                Ok(response) => {
                    futures_stream::once(super::sse::failed_stream(response, unstarted)).boxed()
                }
                Err(error) => super::sse::error_stream(error, unstarted),
            }
        })
        .flatten()
        .boxed()
    }

    fn get_retry_advice(&self, request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
        super::retry::retry_advice(request)
    }
}

async fn backoff(attempt: u32, server_delay: Option<RetryAfter>) {
    if let Some(retry_after) = server_delay {
        tokio::time::sleep_until(retry_after.deadline()).await;
    } else {
        // Codex HTTP retries use a 200 ms exponential backoff with ten percent jitter.
        let delay = 200u128.saturating_mul(2u128.saturating_pow(attempt));
        let jitter = rand::rng().random_range(900..1100);
        let millis = u64::try_from(delay.saturating_mul(jitter) / 1000).unwrap_or(u64::MAX);
        tokio::time::sleep(Duration::from_millis(millis)).await;
    }
}

fn signed_auth_failure(
    signer: &dyn ResponsesRequestSigner,
    facts: &ResponseFacts,
    text: &str,
) -> Error {
    let mut payload =
        serde_json::from_str::<serde_json::Value>(text).unwrap_or(serde_json::Value::Null);
    if let Some(message) = signer.failure_message(facts.status(), text) {
        match payload
            .get_mut("error")
            .and_then(serde_json::Value::as_object_mut)
        {
            Some(error) => {
                error.insert("message".to_owned(), serde_json::Value::String(message));
            }
            None => payload = serde_json::json!({"error": {"message": message}}),
        }
    }
    super::error::response_failure(facts, &payload).into_error()
}
