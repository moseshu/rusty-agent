//! trait Model / trait `ModelProvider` —— provider 无关的模型契约。

pub(crate) mod duration;
pub mod protocol;
pub mod request;
pub mod retry;
pub mod settings;
pub mod stream;

pub use protocol::{
    ApiProtocol, PromptCacheSupport, ProtocolCapabilities, ReasoningCarrier, ReasoningReplay,
    ServerConversationSupport, StablePrefixLocation, StructuredOutputLocation, ToolCallCarrier,
    ToolResultCarrier,
};
pub use retry::{ModelRetrySettings, RetryBackoffSettings};
pub use settings::{
    Effort, JsonMap, McpToolChoice, ModelSettings, ProviderKey, ResolvedModelSettings,
    ThinkingConfig, ToolChoice,
};
