//! Provider-neutral model requests.
//!
//! The request owns only values that cross the runtime/model boundary. Runtime tool objects,
//! output parsers, agent graphs, credentials, and provider SDK types do not belong here. Rich
//! runtime types project into the lightweight model-facing definitions below before a call.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ResolvedModelSettings;
use crate::item::{AgentId, ModelInputItem};

/// Tracing visibility for one model call.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTracing {
    /// Do not create model-call trace topology.
    Disabled,
    /// Record trace topology and model input/output data.
    Enabled,
    /// Record trace topology without model input/output data.
    EnabledWithoutData,
}

impl ModelTracing {
    /// Whether tracing is completely disabled.
    #[must_use]
    pub const fn is_disabled(self) -> bool {
        matches!(self, Self::Disabled)
    }

    /// Whether model input/output data may be included.
    #[must_use]
    pub const fn include_data(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// Optional server-managed conversation continuation.
///
/// `previous_response_id` and `conversation_id` are mutually exclusive by construction. Adapters
/// still validate the selected variant against [`super::ApiProtocol::capabilities`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum ConversationContinuation {
    /// Replay the client-owned input history.
    #[default]
    None,
    /// Continue from a provider response ID.
    PreviousResponseId(String),
    /// Append to a provider-managed conversation.
    ConversationId(String),
}

impl ConversationContinuation {
    /// Previous response ID, when selected.
    #[must_use]
    pub fn previous_response_id(&self) -> Option<&str> {
        match self {
            Self::PreviousResponseId(id) => Some(id),
            Self::None | Self::ConversationId(_) => None,
        }
    }

    /// Conversation ID, when selected.
    #[must_use]
    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            Self::ConversationId(id) => Some(id),
            Self::None | Self::PreviousResponseId(_) => None,
        }
    }

    /// Whether the request relies on provider-managed conversation state.
    #[must_use]
    pub const fn is_server_managed(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Model-facing projection of an executable tool.
///
/// [`Tool`](crate::tool::Tool) and its [`ToolOptions`](crate::tool::ToolOptions) own invocation,
/// approval, timeout, guards, and identity. None of those are provider request fields, so the
/// model boundary receives only this advertiseable definition.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ModelToolDefinition {
    name: String,
    description: Option<String>,
    input_schema: Value,
    strict: bool,
}

impl ModelToolDefinition {
    /// Creates a model-facing tool definition.
    #[must_use]
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            input_schema,
            strict: false,
        }
    }

    /// Adds the model-facing description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets strict JSON-schema handling.
    #[must_use]
    pub const fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Tool name advertised to the model.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Optional model-facing description.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Provider-neutral JSON input schema.
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// Whether strict JSON-schema handling is requested.
    #[must_use]
    pub const fn strict(&self) -> bool {
        self.strict
    }
}

/// Model-facing projection of an agent handoff.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ModelHandoffDefinition {
    target_agent: AgentId,
    name: String,
    description: Option<String>,
    input_schema: Value,
    strict: bool,
}

impl ModelHandoffDefinition {
    /// Creates a model-facing handoff definition.
    #[must_use]
    pub fn new(target_agent: AgentId, name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            target_agent,
            name: name.into(),
            description: None,
            input_schema,
            strict: false,
        }
    }

    /// Adds the model-facing description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets strict JSON-schema handling.
    #[must_use]
    pub const fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Stable identity of the handoff target.
    #[must_use]
    pub const fn target_agent(&self) -> &AgentId {
        &self.target_agent
    }

    /// Handoff name advertised to the model.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Optional model-facing description.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Provider-neutral JSON input schema.
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// Whether strict JSON-schema handling is requested.
    #[must_use]
    pub const fn strict(&self) -> bool {
        self.strict
    }
}

/// Model-facing structured-output schema.
///
/// `None` on [`ModelRequest::output_schema`] means ordinary text output. A future milestone will
/// add parsing and validation above this projection without leaking those runtime concerns to
/// adapters.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ModelOutputSchema {
    name: String,
    schema: Value,
    strict: bool,
}

impl ModelOutputSchema {
    /// Creates a structured-output definition.
    #[must_use]
    pub fn new(name: impl Into<String>, schema: Value) -> Self {
        Self {
            name: name.into(),
            schema,
            strict: false,
        }
    }

    /// Sets strict JSON-schema handling.
    #[must_use]
    pub const fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Schema name exposed to providers that require one.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Provider-neutral JSON schema.
    #[must_use]
    pub const fn schema(&self) -> &Value {
        &self.schema
    }

    /// Whether strict JSON-schema handling is requested.
    #[must_use]
    pub const fn strict(&self) -> bool {
        self.strict
    }
}

/// Complete input to one model call.
///
/// The request is intentionally not serializable: resolved settings may contain transport headers
/// and provider extras that must not accidentally enter traces or persisted run state.
///
/// # What is deliberately absent
///
/// **Reusable prompt objects.** The reference `Model.get_response` takes a `prompt` parameter, and
/// its Chat implementation rejects it outright because only Responses supports reusable prompts.
/// That makes it protocol-specific, so it travels in the `OpenAI` provider's `extra_body` bucket
/// rather than here — the same treatment as `store` and `response_include`. Its absence is a
/// decision, not an oversight.
///
/// **A per-session prompt cache key.** A future milestone wants `prompt_cache_key` held constant
/// for a whole session, equal to the thread id. That is a runtime value with a session lifetime,
/// so `extra_body` is the wrong home for it — that bucket is static provider registration data.
/// When that lands, the key belongs on this type as a new field; the type is `#[non_exhaustive]`
/// precisely so adding one is not a breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    system_instructions: Option<String>,
    input: Vec<ModelInputItem>,
    model_settings: ResolvedModelSettings,
    tools: Vec<ModelToolDefinition>,
    output_schema: Option<ModelOutputSchema>,
    handoffs: Vec<ModelHandoffDefinition>,
    tracing: ModelTracing,
    continuation: ConversationContinuation,
}

impl ModelRequest {
    /// Creates a request with no tools, handoffs, output schema, or server continuation.
    #[must_use]
    pub fn new(input: Vec<ModelInputItem>, model_settings: ResolvedModelSettings) -> Self {
        Self {
            system_instructions: None,
            input,
            model_settings,
            tools: Vec::new(),
            output_schema: None,
            handoffs: Vec::new(),
            tracing: ModelTracing::Disabled,
            continuation: ConversationContinuation::None,
        }
    }

    /// Sets stable system instructions.
    #[must_use]
    pub fn with_system_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.system_instructions = Some(instructions.into());
        self
    }

    /// Sets model-facing tool definitions.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ModelToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    /// Sets the structured-output schema.
    #[must_use]
    pub fn with_output_schema(mut self, output_schema: ModelOutputSchema) -> Self {
        self.output_schema = Some(output_schema);
        self
    }

    /// Sets model-facing handoff definitions.
    #[must_use]
    pub fn with_handoffs(mut self, handoffs: Vec<ModelHandoffDefinition>) -> Self {
        self.handoffs = handoffs;
        self
    }

    /// Sets tracing visibility for this call.
    #[must_use]
    pub const fn with_tracing(mut self, tracing: ModelTracing) -> Self {
        self.tracing = tracing;
        self
    }

    /// Sets server-managed continuation, replacing any previous mode.
    #[must_use]
    pub fn with_continuation(mut self, continuation: ConversationContinuation) -> Self {
        self.continuation = continuation;
        self
    }

    /// Continues from a provider response ID, replacing any conversation ID.
    #[must_use]
    pub fn with_previous_response_id(mut self, response_id: impl Into<String>) -> Self {
        self.continuation = ConversationContinuation::PreviousResponseId(response_id.into());
        self
    }

    /// Uses a provider-managed conversation, replacing any previous response ID.
    #[must_use]
    pub fn with_conversation_id(mut self, conversation_id: impl Into<String>) -> Self {
        self.continuation = ConversationContinuation::ConversationId(conversation_id.into());
        self
    }

    /// Stable system instructions.
    #[must_use]
    pub fn system_instructions(&self) -> Option<&str> {
        self.system_instructions.as_deref()
    }

    /// Provider-neutral input items.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// Effective settings resolved for this provider and model.
    #[must_use]
    pub const fn model_settings(&self) -> &ResolvedModelSettings {
        &self.model_settings
    }

    /// Model-facing tool definitions.
    #[must_use]
    pub fn tools(&self) -> &[ModelToolDefinition] {
        &self.tools
    }

    /// Structured-output schema, or `None` for ordinary text output.
    #[must_use]
    pub const fn output_schema(&self) -> Option<&ModelOutputSchema> {
        self.output_schema.as_ref()
    }

    /// Model-facing handoff definitions.
    #[must_use]
    pub fn handoffs(&self) -> &[ModelHandoffDefinition] {
        &self.handoffs
    }

    /// Tracing visibility for this call.
    #[must_use]
    pub const fn tracing(&self) -> ModelTracing {
        self.tracing
    }

    /// Server-managed continuation mode.
    #[must_use]
    pub const fn continuation(&self) -> &ConversationContinuation {
        &self.continuation
    }
}
