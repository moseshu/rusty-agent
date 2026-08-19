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
    ModelRetryAdviceRequest, ModelRetrySettings, ReplaySafety, RetryAdvice, RetryBackoffSettings,
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
/// Required methods are intentionally limited to the two call modes. Retry advice and resource
/// cleanup have defaults so future adapters only override behavior they own.
#[async_trait]
pub trait Model: Send + Sync + 'static {
    /// Executes one complete model call.
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse>;

    /// Starts one streaming model call.
    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_>;

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
