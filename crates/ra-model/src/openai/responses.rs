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

#[derive(Clone)]
struct ResponsesTransport {
    auth: OpenAiAuth,
    client: HttpClient,
    stream_idle_timeout: Option<Duration>,
    request_max_retries: u32,
    query: BTreeMap<String, String>,
    bearer: Option<Arc<dyn ResponsesBearerToken>>,
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
            }),
            default_model,
            quirks: ProviderQuirks::new(),
            models: Mutex::new(BTreeMap::new()),
        })
    }

    /// Sets endpoint query parameters. Request settings can override individual keys.
    #[must_use]
    pub fn with_default_query(mut self, query: BTreeMap<String, String>) -> Self {
        Arc::make_mut(&mut self.transport).query = query;
        self
    }

    /// Supplies credentials dynamically for this endpoint.
    #[must_use]
    pub fn with_bearer_token(mut self, bearer: Arc<dyn ResponsesBearerToken>) -> Self {
        Arc::make_mut(&mut self.transport).bearer = Some(bearer);
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
        let headers = super::transport_headers(
            &self.transport.auth,
            request.model_settings().extra_headers(),
        )?;
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
            if let Some(retry_after) = server_delay {
                tokio::time::sleep_until(retry_after.deadline()).await;
            } else {
                // Codex HTTP retries use a 200 ms exponential backoff with ten percent jitter.
                let delay = 200u128.saturating_mul(2u128.saturating_pow(attempt));
                let jitter = rand::rng().random_range(900..1100);
                let millis = u64::try_from(delay.saturating_mul(jitter) / 1000).unwrap_or(u64::MAX);
                tokio::time::sleep(Duration::from_millis(millis)).await;
            }
            attempt += 1;
        }
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
        if !query.is_empty() {
            http_request = http_request.query(&query);
        }
        if let Some(timeout) = request.model_settings().timeout() {
            http_request = http_request.timeout(timeout);
        }
        http_request.json(body).send().await
    }

    async fn fetch(&self, request: ModelRequest) -> Result<ra_core::item::ModelResponse> {
        let response = self.send(&request, false).await?;
        let facts = ResponseFacts::read(&response);
        let status = facts.status();
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
            match model.send(&request, true).await {
                Ok(response) if response.status().is_success() => {
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
        crate::retry::retry_advice(request)
    }
}
