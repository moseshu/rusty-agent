//! Anthropic Messages provider.

pub mod auth;
pub mod betas;
pub(crate) mod cache;
pub(crate) mod convert;
pub(crate) mod request;
#[doc(hidden)]
pub mod smoke;
pub(crate) mod stream;

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::{StreamExt, stream as futures_stream};
use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    model::{
        Model, ModelProvider, ModelRequest, ModelRetryAdviceRequest, ModelStream, RetryAdvice,
        stamp_replay_safety,
    },
};

pub use auth::AnthropicAuth;

use crate::retry::unstarted_replay_safety;

/// Wire spelling of the model's own role in a Messages conversation.
///
/// Named once because the bare literal is also a product alias the repository's layering gate
/// watches for. The collision is genuine and belongs to Anthropic's vocabulary, so it is declared
/// here and referred to everywhere else.
pub(crate) const ASSISTANT_ROLE: &str = "assistant"; // layering-allow: assistant = Anthropic wire message role

/// Beta identifier that gates `output_config.format`.
///
/// Sent by the adapter rather than left to the caller because the request that needs it is one this
/// crate builds: a schema lowered without the header is answered with a 400, which would make
/// declaring structured output on an agent look like a framework defect.
const STRUCTURED_OUTPUT_BETA: &str = "structured-outputs-2025-11-13";
/// Prefix that identifies any dated revision of that beta, including one a caller picked itself.
const STRUCTURED_OUTPUT_BETA_PREFIX: &str = "structured-outputs-";

/// Anthropic Messages provider with a shared HTTP client and per-model cache.
pub struct AnthropicMessagesProvider {
    auth: Arc<AnthropicAuth>,
    client: reqwest::Client,
    default_model: String,
    models: Mutex<BTreeMap<String, Arc<dyn Model>>>,
}

impl AnthropicMessagesProvider {
    /// Creates a Messages provider using the supplied default model.
    pub fn new(auth: AnthropicAuth, default_model: impl Into<String>) -> Result<Self> {
        auth.validate()?;
        let default_model = default_model.into();
        if default_model.trim().is_empty() {
            return Err(Error::config("Anthropic default model must not be empty"));
        }
        let client = reqwest::Client::builder()
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            auth: Arc::new(auth),
            client,
            default_model,
            models: Mutex::new(BTreeMap::new()),
        })
    }

    /// Default provider-facing model identifier.
    #[must_use]
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    fn lock_models(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Arc<dyn Model>>>> {
        self.models.lock().map_err(|_| {
            Error::provider(
                ProviderErrorKind::Behavior,
                "Anthropic model cache lock poisoned",
            )
        })
    }
}

impl fmt::Debug for AnthropicMessagesProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicMessagesProvider")
            .field("base_url", &self.auth.base_url())
            .field("default_model", &self.default_model)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelProvider for AnthropicMessagesProvider {
    fn get_model(&self, model_name: Option<&str>) -> Result<Arc<dyn Model>> {
        let name = model_name.unwrap_or(&self.default_model).trim();
        if name.is_empty() {
            return Err(Error::config("Anthropic model name must not be empty"));
        }
        let mut models = self.lock_models()?;
        if let Some(model) = models.get(name) {
            return Ok(Arc::clone(model));
        }
        let model: Arc<dyn Model> = Arc::new(AnthropicMessagesModel::from_parts(
            name.to_owned(),
            Arc::clone(&self.auth),
            self.client.clone(),
        ));
        models.insert(name.to_owned(), Arc::clone(&model));
        Ok(model)
    }
}

/// One model accessed through Anthropic Messages.
#[derive(Clone)]
pub struct AnthropicMessagesModel {
    model: String,
    auth: Arc<AnthropicAuth>,
    client: reqwest::Client,
}

impl AnthropicMessagesModel {
    /// Creates a directly usable Messages model.
    pub fn new(model: impl Into<String>, auth: AnthropicAuth) -> Result<Self> {
        auth.validate()?;
        let model = model.into();
        if model.trim().is_empty() {
            return Err(Error::config("Anthropic model name must not be empty"));
        }
        let client = reqwest::Client::builder()
            .build()
            .map_err(transport_error)?;
        Ok(Self::from_parts(model, Arc::new(auth), client))
    }

    fn from_parts(model: String, auth: Arc<AnthropicAuth>, client: reqwest::Client) -> Self {
        Self {
            model,
            auth,
            client,
        }
    }

    /// Provider-facing model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    async fn send(&self, request: &ModelRequest, streaming: bool) -> Result<reqwest::Response> {
        let body = request::build_request_body(&self.model, request, streaming).await?;
        let mut http = self
            .client
            .post(format!("{}/messages", self.auth.base_url()))
            .header("content-type", "application/json")
            .header("anthropic-version", self.auth.version())
            .header(
                "user-agent",
                concat!("rusty-agent/", env!("CARGO_PKG_VERSION")),
            );
        if let Some(key) = self.auth.api_key() {
            http = http.header("x-api-key", key);
        }
        for beta in self.auth.betas() {
            http = http.header("anthropic-beta", beta);
        }
        if request.output_schema().is_some()
            && !self
                .auth
                .betas()
                .iter()
                .any(|beta| beta.starts_with(STRUCTURED_OUTPUT_BETA_PREFIX))
        {
            http = http.header("anthropic-beta", STRUCTURED_OUTPUT_BETA);
        }
        http = apply_headers(
            http,
            self.auth.default_headers(),
            request.model_settings().extra_headers(),
        )?;
        let query = request
            .model_settings()
            .extra_query()
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned),
                )
            })
            .collect::<Vec<_>>();
        if !query.is_empty() {
            http = http.query(&query);
        }
        if let Some(timeout) = request.model_settings().timeout() {
            http = http.timeout(timeout);
        }
        http.json(&body).send().await.map_err(transport_error)
    }

    async fn fetch(&self, request: ModelRequest) -> Result<ra_core::item::ModelResponse> {
        let response = self.send(&request, false).await?;
        let facts = ResponseFacts::read(&response);
        let status = facts.status;
        let payload = match response.json::<serde_json::Value>().await {
            Ok(value) => value,
            Err(error) if !status.is_success() => {
                return Err(response_failure(&facts, &serde_json::Value::Null)
                    .with_source(error)
                    .into_error());
            }
            Err(error) => return Err(decode_error(error)),
        };
        if !status.is_success() {
            return Err(response_failure(&facts, &payload).into_error());
        }
        convert::convert_response(
            &payload,
            facts.request_id,
            request.handoffs(),
            request.model_settings().provider(),
        )
    }
}

impl fmt::Debug for AnthropicMessagesModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicMessagesModel")
            .field("model", &self.model)
            .field("base_url", &self.auth.base_url())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Model for AnthropicMessagesModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ra_core::item::ModelResponse> {
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
                    let facts = ResponseFacts::read(&response);
                    if !is_event_stream(&response) { return futures_stream::once(async move { Err(Error::provider(ProviderErrorKind::Behavior, "Anthropic Messages was asked to stream but did not return text/event-stream")) }).boxed(); }
                    stream::events(response, provider, handoffs, facts.request_id, unstarted)
                }
                Ok(response) => futures_stream::once(failed_stream(response, unstarted)).boxed(),
                Err(error) => futures_stream::once(async move { Err(stamp_replay_safety(error, unstarted)) }).boxed(),
            }
        }).flatten().boxed()
    }

    fn get_retry_advice(&self, request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
        crate::retry::retry_advice(request)
    }
}

struct ResponseFacts {
    status: reqwest::StatusCode,
    request_id: Option<String>,
    hints: crate::retry::RetryHints,
}
impl ResponseFacts {
    fn read(response: &reqwest::Response) -> Self {
        let headers = response.headers();
        let request_id = headers
            .get("request-id")
            .or_else(|| headers.get("x-request-id"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let hints = crate::retry::RetryHints::from_headers(|name| {
            headers.get(name).and_then(|value| value.to_str().ok())
        });
        Self {
            status: response.status(),
            request_id,
            hints,
        }
    }
}

fn transport_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        ProviderErrorKind::Timeout
    } else {
        ProviderErrorKind::Network
    };
    ra_core::model::NormalizedProviderError::new(kind, "Anthropic request transport failed")
        .with_source(error)
        .into_error()
}
fn decode_error(error: reqwest::Error) -> Error {
    ra_core::model::NormalizedProviderError::new(
        ProviderErrorKind::Behavior,
        "Anthropic returned a non-JSON response",
    )
    .with_source(error)
    .into_error()
}
fn response_failure(
    facts: &ResponseFacts,
    payload: &serde_json::Value,
) -> ra_core::model::NormalizedProviderError {
    let error = payload.get("error").unwrap_or(payload);
    let code = error.get("type").and_then(serde_json::Value::as_str);
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Anthropic request failed");
    let kind = if matches!(code, Some("invalid_request_error") if convert::is_context_overflow(message))
    {
        ProviderErrorKind::ContextOverflow
    } else {
        match facts.status.as_u16() {
            401 | 403 => ProviderErrorKind::Auth,
            408 => ProviderErrorKind::Timeout,
            409 => ProviderErrorKind::Conflict,
            429 => ProviderErrorKind::RateLimit,
            500..=599 => ProviderErrorKind::ServerError,
            _ => ProviderErrorKind::BadRequest,
        }
    };
    let suffix = facts
        .request_id
        .as_ref()
        .map_or_else(String::new, |id| format!(" (request_id {id})"));
    let mut normalized = ra_core::model::NormalizedProviderError::new(
        kind,
        format!("Anthropic HTTP {}{suffix}: {message}", facts.status),
    )
    .with_status_code(facts.status.as_u16());
    if let Some(code) = code {
        normalized = normalized.with_error_code(code);
    }
    if let Some(id) = &facts.request_id {
        normalized = normalized.with_request_id(id);
    }
    if let Some(delay) = facts.hints.retry_after() {
        normalized = normalized.with_retry_after(delay);
    }
    if let Some(retry) = facts.hints.should_retry() {
        normalized = normalized.with_should_retry(retry);
    }
    normalized
}
fn apply_headers(
    request: reqwest::RequestBuilder,
    defaults: &BTreeMap<String, String>,
    overrides: &BTreeMap<String, String>,
) -> Result<reqwest::RequestBuilder> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in defaults.iter().chain(overrides) {
        if !name.eq_ignore_ascii_case("x-api-key")
            && !name.eq_ignore_ascii_case("anthropic-version")
            && !name.eq_ignore_ascii_case("anthropic-beta")
        {
            let name =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                    Error::caller(format!(
                        "Anthropic transport header name `{name}` is invalid"
                    ))
                    .with_source(error)
                })?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(|error| {
                Error::caller(format!(
                    "Anthropic transport header `{name}` has an invalid value"
                ))
                .with_source(error)
            })?;
            headers.insert(name, value);
        }
    }
    Ok(request.headers(headers))
}
fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        })
}
async fn failed_stream(
    response: reqwest::Response,
    unstarted: ra_core::model::ReplaySafety,
) -> Result<ra_core::model::ModelStreamEvent> {
    let facts = ResponseFacts::read(&response);
    let payload = response.json().await.unwrap_or(serde_json::Value::Null);
    Err(response_failure(&facts, &payload)
        .with_replay_safety(unstarted)
        .into_error())
}
