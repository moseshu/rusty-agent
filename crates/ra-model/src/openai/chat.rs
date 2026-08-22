//! `/v1/chat/completions` protocol implementation. A first-class citizen, not an appendage of the compat layer.
//!
//! Chat Completions is **not** a reduced Responses. It has no first-class reasoning item, no
//! server-side conversation, and it carries a tool call inside an assistant message rather than as
//! an item of its own. Treating it as a downgrade is what produces the two failures this module is
//! built around: a history that cannot be replayed, and a streamed turn that has to be consumed by
//! a second, protocol-specific code path.
//!
//! The internal item model stays Responses-shaped — reasoning and tool calls are separate items —
//! and this adapter lowers into, and lifts out of, the message shape on both sides.

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

use self::reasoning::ReasoningReplayPolicy;
use super::{auth::OpenAiAuth, error::ResponseFacts, sse::Terminator};
use crate::{provider::quirks::ProviderQuirks, retry::unstarted_replay_safety};

pub(crate) mod convert;
pub mod reasoning;
pub(crate) mod request;
pub(crate) mod stream;

/// The item identifier attached to synthesized stream events.
///
/// Chat Completions assigns no identifier to an output item, while the Responses-shaped event
/// envelope this adapter emits has a field for one. A fixed sentinel is used rather than a
/// generated identifier: a random value would make the same conversation serialize differently on
/// every call, which breaks snapshot tests and jitters the cached prefix for no benefit — nothing
/// downstream can resolve a Chat item identifier anyway.
pub(crate) const FAKE_ITEM_ID: &str = "__fake_id__";

/// Caller policy for features Chat Completions cannot express.
///
/// # What is deliberately not here
///
/// Endpoint capabilities. Whether a gateway accepts non-text tool results, or round-trips
/// Anthropic thinking blocks, is a fact about the endpoint, and it is declared once on the
/// provider registration through [`ProviderQuirks`] alongside credentials and the `extra_body`
/// bucket. The reference implementation carries both as converter arguments; splitting them means
/// onboarding a gateway stays a one-place change, and a caller cannot ask for a shape the endpoint
/// was never declared to accept.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChatLoweringOptions {
    strict_feature_validation: bool,
}

impl ChatLoweringOptions {
    /// Creates the default policy, which refuses to degrade silently.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            strict_feature_validation: true,
        }
    }

    /// Chooses whether an unrepresentable feature fails the call or is dropped with a warning.
    ///
    /// The default is to fail. A silent downgrade is discovered later as a model that ignored an
    /// instruction nobody can find in the transcript, and the request that dropped it left no
    /// trace; the reference implementation defaults the other way and its own logs say so. Turn it
    /// off to send a history through an endpoint that cannot carry all of it, accepting the loss.
    #[must_use]
    pub const fn with_strict_feature_validation(mut self, strict: bool) -> Self {
        self.strict_feature_validation = strict;
        self
    }

    /// Whether an unrepresentable feature fails the call.
    #[must_use]
    pub const fn strict_feature_validation(self) -> bool {
        self.strict_feature_validation
    }
}

impl Default for ChatLoweringOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything below the request that a lowering or lifting decision may consult.
#[derive(Clone)]
pub(crate) struct ChatCodec {
    pub(crate) model: String,
    pub(crate) base_url: String,
    pub(crate) quirks: ProviderQuirks,
    pub(crate) options: ChatLoweringOptions,
    pub(crate) replay: ReasoningReplayPolicy,
    pub(crate) terminator: Terminator,
}

impl ChatCodec {
    /// Reports something this protocol cannot carry: either fails the call or records the loss.
    ///
    /// Every site that cannot represent a neutral value routes through here, so the choice between
    /// failing and degrading is made once and is visible in one place rather than being decided
    /// again at each `if`.
    pub(crate) fn degrade(&self, message: &str) -> Result<()> {
        if self.options.strict_feature_validation() {
            return Err(Error::caller(message.to_owned()));
        }
        tracing::warn!(model = %self.model, "{message}");
        Ok(())
    }
}

/// `OpenAI` Chat Completions provider with a shared HTTP client and per-model instance cache.
pub struct OpenAiChatProvider {
    auth: Arc<OpenAiAuth>,
    client: reqwest::Client,
    default_model: String,
    quirks: ProviderQuirks,
    options: ChatLoweringOptions,
    replay: ReasoningReplayPolicy,
    terminator: Terminator,
    buffer_tool_calls: bool,
    models: Mutex<BTreeMap<String, Arc<dyn Model>>>,
}

impl OpenAiChatProvider {
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
            options: ChatLoweringOptions::new(),
            replay: ReasoningReplayPolicy::default(),
            terminator: Terminator::default(),
            buffer_tool_calls: false,
            models: Mutex::new(BTreeMap::new()),
        })
    }

    /// Declares the endpoint capabilities this provider may use.
    #[must_use]
    pub const fn with_quirks(mut self, quirks: ProviderQuirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// Declares how this endpoint terminates a stream.
    ///
    /// Deliberately not public: the protocol has one terminator, and only a compatible endpoint
    /// can be in a position to deviate from it. The knob therefore belongs to the compat layer's
    /// endpoint configuration rather than to this adapter, where a first-party caller would have
    /// to decide about a question that has only one answer for them.
    #[must_use]
    pub(crate) fn with_terminator(mut self, terminator: Terminator) -> Self {
        self.terminator = terminator;
        self
    }

    /// Sets the caller policy for unrepresentable features.
    #[must_use]
    pub const fn with_lowering_options(mut self, options: ChatLoweringOptions) -> Self {
        self.options = options;
        self
    }

    /// Replaces the per-item reasoning replay policy.
    #[must_use]
    pub fn with_reasoning_replay(mut self, replay: ReasoningReplayPolicy) -> Self {
        self.replay = replay;
        self
    }

    /// Buffers streamed tool-call fragments until each call is complete.
    #[must_use]
    pub const fn with_buffered_tool_calls(mut self, buffered: bool) -> Self {
        self.buffer_tool_calls = buffered;
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

impl fmt::Debug for OpenAiChatProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiChatProvider")
            .field("base_url", &self.auth.base_url())
            .field("default_model", &self.default_model)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelProvider for OpenAiChatProvider {
    fn get_model(&self, model_name: Option<&str>) -> Result<Arc<dyn Model>> {
        let model_name = model_name.unwrap_or(&self.default_model).trim();
        if model_name.is_empty() {
            return Err(Error::config("OpenAI model name must not be empty"));
        }

        let mut models = self.lock_models()?;
        if let Some(model) = models.get(model_name) {
            return Ok(Arc::clone(model));
        }
        let model: Arc<dyn Model> = Arc::new(OpenAiChatModel {
            codec: ChatCodec {
                model: model_name.to_owned(),
                base_url: self.auth.base_url().to_owned(),
                quirks: self.quirks,
                options: self.options,
                replay: self.replay.clone(),
                terminator: self.terminator.clone(),
            },
            auth: Arc::clone(&self.auth),
            client: self.client.clone(),
            buffer_tool_calls: self.buffer_tool_calls,
        });
        models.insert(model_name.to_owned(), Arc::clone(&model));
        Ok(model)
    }
}

/// One model accessed through `OpenAI` Chat Completions.
#[derive(Clone)]
pub struct OpenAiChatModel {
    codec: ChatCodec,
    auth: Arc<OpenAiAuth>,
    client: reqwest::Client,
    buffer_tool_calls: bool,
}

impl OpenAiChatModel {
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
        Ok(Self {
            codec: ChatCodec {
                model,
                base_url: auth.base_url().to_owned(),
                quirks: ProviderQuirks::new(),
                options: ChatLoweringOptions::new(),
                replay: ReasoningReplayPolicy::default(),
                terminator: Terminator::default(),
            },
            auth: Arc::new(auth),
            client,
            buffer_tool_calls: false,
        })
    }

    /// Declares the endpoint capabilities this model may use.
    #[must_use]
    pub const fn with_quirks(mut self, quirks: ProviderQuirks) -> Self {
        self.codec.quirks = quirks;
        self
    }

    /// Sets the caller policy for unrepresentable features.
    #[must_use]
    pub const fn with_lowering_options(mut self, options: ChatLoweringOptions) -> Self {
        self.codec.options = options;
        self
    }

    /// Replaces the per-item reasoning replay policy.
    #[must_use]
    pub fn with_reasoning_replay(mut self, replay: ReasoningReplayPolicy) -> Self {
        self.codec.replay = replay;
        self
    }

    /// Buffers streamed tool-call fragments until each call is complete.
    ///
    /// Off by default, matching the reference implementation: a consumer that renders arguments as
    /// they arrive wants the fragments. Turn it on for an endpoint whose fragments arrive out of
    /// order, or for a consumer that cannot act on a partial argument string.
    #[must_use]
    pub const fn with_buffered_tool_calls(mut self, buffered: bool) -> Self {
        self.buffer_tool_calls = buffered;
        self
    }

    /// Provider-facing model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.codec.model
    }

    /// Issues one request and returns the raw HTTP response.
    async fn send(&self, request: &ModelRequest, streaming: bool) -> Result<reqwest::Response> {
        let body = request::build_request_body(&self.codec, request, streaming).await?;
        let mut http_request = self
            .client
            .post(format!("{}/chat/completions", self.auth.base_url()))
            .header("content-type", "application/json")
            .header(
                "user-agent",
                concat!("rusty-agent/", env!("CARGO_PKG_VERSION")),
            );

        if let Some(api_key) = self.auth.api_key() {
            http_request = http_request.bearer_auth(api_key);
        }
        http_request = super::apply_transport_headers(
            http_request,
            &self.auth,
            request.model_settings().extra_headers(),
        )?;

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

        http_request
            .json(&body)
            .send()
            .await
            .map_err(super::error::transport_error)
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
            Err(error) => return Err(super::error::decode_error(error)),
        };
        if !status.is_success() {
            return Err(super::error::response_failure(&facts, &payload).into_error());
        }
        convert::convert_completion(
            &self.codec,
            &payload,
            facts.into_request_id(),
            request.handoffs(),
            request.model_settings().provider(),
        )
    }
}

impl fmt::Debug for OpenAiChatModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiChatModel")
            .field("model", &self.codec.model)
            .field("base_url", &self.auth.base_url())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Model for OpenAiChatModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ra_core::item::ModelResponse> {
        // A non-streaming call publishes nothing until it returns, so no consumer saw a partial
        // turn. What the endpoint may have recorded on its own side is the other half of the
        // question, and the continuation is what answers it.
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
                    // Read before the body is consumed: the terminal response carries it, and the
                    // frames it is assembled from never mention it.
                    let request_id = ResponseFacts::read(&response).into_request_id();
                    match super::sse::ensure_event_stream(&response, "Chat Completions") {
                        Ok(()) => stream::events(
                            model.codec.clone(),
                            super::sse::frames(response, model.codec.terminator.clone()),
                            provider,
                            handoffs,
                            model.buffer_tool_calls,
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
