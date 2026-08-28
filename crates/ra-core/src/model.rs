//! Provider-neutral model and provider contracts.
//!
//! The runtime depends only on [`Model`]. Provider adapters own wire lowering/lifting and expose
//! normalized responses and stream events. [`ModelProvider`] resolves names and owns any shared
//! provider resources; neither trait exposes an HTTP SDK client.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::{error::Result, item::ModelResponse};

pub(crate) mod duration;
pub mod protocol;
pub mod request;
pub mod resolution;
pub mod retry;
pub mod settings;
pub mod stream;

pub use protocol::{
    ApiProtocol, PromptCacheSupport, ProtocolCapabilities, ReasoningCarrier, ReasoningReplay,
    ServerConversationSupport, StablePrefixLocation, StructuredOutputLocation, ToolCallCarrier,
    ToolResultCarrier,
};
pub use request::{
    ConversationContinuation, ModelHandoffDefinition, ModelOutputSchema, ModelRequest,
    ModelToolDefinition, ModelTracing, ProviderConversationId,
};
pub use resolution::{ModelSelector, ResolvedModel};
pub use retry::{
    JitterSample, ModelRetryAdviceRequest, ModelRetryPolicy, ModelRetrySettings,
    NetworkErrorRetryPolicy, NeverRetryPolicy, NormalizedProviderError,
    ProviderSuggestedRetryPolicy, ReplaySafety, RetryAdvice, RetryAfterPolicy, RetryBackoff,
    RetryBackoffSettings, RetryDecision, RetryPolicyContext, replay_safety_of, stamp_replay_safety,
};
pub use settings::{
    Effort, JsonMap, McpToolChoice, ModelSettings, ProviderKey, ResolvedModelSettings,
    ThinkingConfig, ToolChoice,
};
pub use stream::{ModelStreamEvent, RawResponseEvent, RunItemStreamEvent};

/// Sendable stream returned by a model adapter.
pub type ModelStream<'a> = BoxStream<'a, Result<ModelStreamEvent>>;

/// A resolved model capable of non-streaming and streaming calls.
///
/// One required method, [`Self::get_response`]. Everything else has a default so future adapters
/// only override behavior they own.
///
/// # The loop always streams, and that is not the same as "the host asked for streaming"
///
/// `ra-runtime` calls [`Self::stream_response`] for **every** model call. A function call the
/// adapter completes mid-stream starts its tool immediately, overlapping execution with the rest of
/// generation, and that is worth doing whether or not anyone is watching the narration;
/// `partial_messages` decides one separate thing, namely whether the raw provider events leave the
/// runtime.
///
/// So [`Self::get_response`] is the one an adapter must write and [`Self::stream_response`] is the
/// one the loop calls. That reads backwards until you look at the default: a model that implements
/// only the required method still gets driven through the streaming entry point, because the
/// default is written in terms of it. Making the streaming method required instead would put the
/// harder of the two on every mock, test double, and example in exchange for nothing — a real
/// adapter overrides it either way.
#[async_trait]
pub trait Model: Send + Sync + 'static {
    /// Executes one complete model call.
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse>;

    /// Starts one streaming model call.
    ///
    /// The default answers with [`Self::get_response`]'s result as a single
    /// [`ModelStreamEvent::Completed`], which is a correct stream and the shape the loop expects.
    /// What it forfeits is overlap: with no items arriving before the terminal response, no tool
    /// can start early and the turn executes its whole batch at settlement. That is a performance
    /// property, not a behavioral one, so **an adapter whose protocol can stream should override
    /// this** — the default exists for the models that have nothing to stream, not as an invitation
    /// to skip the work.
    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        Box::pin(futures::stream::once(async move {
            self.get_response(request)
                .await
                .map(|response| ModelStreamEvent::Completed(Box::new(response)))
        }))
    }

    /// Returns provider-specific evidence for the runtime retry policy.
    fn get_retry_advice(&self, _request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
        None
    }

    /// Releases persistent resources owned by this model.
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// Resolves model names and owns provider-level caches or connections.
#[async_trait]
pub trait ModelProvider: Send + Sync + 'static {
    /// Resolves a model name, or the provider default when `model_name` is `None`.
    fn get_model(&self, model_name: Option<&str>) -> Result<Arc<dyn Model>>;

    /// Releases resources owned by this provider.
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// Resolves an application-level model selector across provider registrations.
///
/// [`ModelProvider`] resolves a name inside one already-selected provider. This higher-level
/// contract also preserves the canonical provider identity, wire protocol, and the two
/// registration-owned settings layers needed by turn preparation. Keeping the contract in core
/// lets the loop kernel depend on an injected resolver without depending on `ra-model`.
pub trait ModelResolver: Send + Sync + 'static {
    /// Resolves a selector, or the resolver's configured defaults when it is absent.
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel>;
}
