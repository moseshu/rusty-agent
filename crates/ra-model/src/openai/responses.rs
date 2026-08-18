//! `/v1/responses` protocol implementation.

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
        Model, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent, RawResponseEvent,
        RunItemStreamEvent,
    },
};

use super::auth::OpenAiAuth;
use crate::provider::quirks::ProviderQuirks;

pub(crate) mod cache;
pub(crate) mod convert;
pub(crate) mod request;
pub(crate) mod stream;

/// `OpenAI` Responses provider with a shared HTTP client and per-model instance cache.
pub struct OpenAiResponsesProvider {
    auth: Arc<OpenAiAuth>,
    client: reqwest::Client,
    default_model: String,
    quirks: ProviderQuirks,
    models: Mutex<BTreeMap<String, Arc<dyn Model>>>,
}

impl OpenAiResponsesProvider {
    /// Creates a provider using the supplied default model.
    pub fn new(auth: OpenAiAuth, default_model: impl Into<String>) -> Result<Self> {
        auth.validate()?;
        let default_model = default_model.into();
        if default_model.trim().is_empty() {
            return Err(Error::config("OpenAI default model must not be empty"));
        }
        let client = reqwest::Client::builder()
            .build()
            .map_err(super::error::transport_error)?;
        Ok(Self {
            auth: Arc::new(auth),
            client,
            default_model,
            quirks: ProviderQuirks::new(),
            models: Mutex::new(BTreeMap::new()),
        })
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
            .field("base_url", &self.auth.base_url())
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
        let model: Arc<dyn Model> = Arc::new(OpenAiResponsesModel::from_parts(
            model_name.to_owned(),
            Arc::clone(&self.auth),
            self.client.clone(),
            self.quirks,
        ));
        models.insert(model_name.to_owned(), Arc::clone(&model));
        Ok(model)
    }
}

/// One model accessed through `OpenAI` Responses.
#[derive(Clone)]
pub struct OpenAiResponsesModel {
    model: String,
    auth: Arc<OpenAiAuth>,
    client: reqwest::Client,
    quirks: ProviderQuirks,
}

impl OpenAiResponsesModel {
    /// Creates a directly usable model without a provider registry.
    pub fn new(model: impl Into<String>, auth: OpenAiAuth) -> Result<Self> {
        auth.validate()?;
        let model = model.into();
        if model.trim().is_empty() {
            return Err(Error::config("OpenAI model name must not be empty"));
        }
        let client = reqwest::Client::builder()
            .build()
            .map_err(super::error::transport_error)?;
        Ok(Self::from_parts(
            model,
            Arc::new(auth),
            client,
            ProviderQuirks::new(),
        ))
    }

    /// Declares the endpoint capabilities this model may use.
    #[must_use]
    pub const fn with_quirks(mut self, quirks: ProviderQuirks) -> Self {
        self.quirks = quirks;
        self
    }

    fn from_parts(
        model: String,
        auth: Arc<OpenAiAuth>,
        client: reqwest::Client,
        quirks: ProviderQuirks,
    ) -> Self {
        Self {
            model,
            auth,
            client,
            quirks,
        }
    }

    /// Provider-facing model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    async fn fetch(&self, request: ModelRequest) -> Result<convert::ConvertedResponse> {
        let body = request::build_request_body(&self.model, &request, self.quirks).await?;
        let mut http_request = self
            .client
            .post(format!("{}/responses", self.auth.base_url()))
            .bearer_auth(self.auth.api_key())
            .header("content-type", "application/json")
            .header(
                "user-agent",
                concat!("rusty-agent/", env!("CARGO_PKG_VERSION")),
            );

        if let Some(organization) = self.auth.organization() {
            http_request = http_request.header("openai-organization", organization);
        }
        if let Some(project) = self.auth.project() {
            http_request = http_request.header("openai-project", project);
        }
        for (name, value) in self.auth.default_headers() {
            if !name.eq_ignore_ascii_case("authorization") {
                http_request = http_request.header(name, value);
            }
        }
        for (name, value) in request.model_settings().extra_headers() {
            if !name.eq_ignore_ascii_case("authorization") {
                http_request = http_request.header(name, value);
            }
        }

        let query = request
            .model_settings()
            .extra_query()
            .iter()
            .map(|(key, value)| {
                let value = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned);
                (key.clone(), value)
            })
            .collect::<Vec<_>>();
        if !query.is_empty() {
            http_request = http_request.query(&query);
        }
        if let Some(timeout) = request.model_settings().timeout() {
            http_request = http_request.timeout(timeout);
        }

        let response = http_request
            .json(&body)
            .send()
            .await
            .map_err(super::error::transport_error)?;
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let status = response.status();
        let payload = match response.json::<serde_json::Value>().await {
            Ok(payload) => payload,
            Err(error) if !status.is_success() => {
                return Err(super::error::response_error(
                    status,
                    &serde_json::Value::Null,
                    request_id.as_deref(),
                )
                .with_source(error));
            }
            Err(error) => return Err(super::error::decode_error(error)),
        };
        if !status.is_success() {
            return Err(super::error::response_error(
                status,
                &payload,
                request_id.as_deref(),
            ));
        }
        convert::convert_response(
            payload,
            request_id,
            request.handoffs(),
            request.model_settings().provider(),
        )
    }
}

impl fmt::Debug for OpenAiResponsesModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesModel")
            .field("model", &self.model)
            .field("base_url", &self.auth.base_url())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Model for OpenAiResponsesModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ra_core::item::ModelResponse> {
        self.fetch(request)
            .await
            .map(|converted| converted.response)
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        let model = self.clone();
        futures_stream::once(async move {
            match model.fetch(request).await {
                Ok(converted) => {
                    let provider = converted.provider;
                    let mut events =
                        vec![Ok(ModelStreamEvent::RawResponse(RawResponseEvent::new(
                            provider,
                            "response.completed",
                            converted.raw_response,
                        )))];
                    events.extend(converted.response.output().iter().cloned().map(|item| {
                        let name = item.kind().label();
                        Ok(ModelStreamEvent::RunItem(RunItemStreamEvent::new(
                            name, item,
                        )))
                    }));
                    events
                }
                Err(error) => vec![Err(error)],
            }
        })
        .flat_map(futures_stream::iter)
        .boxed()
    }
}
