//! Provider-neutral model requests.
//!
//! The request owns only values that cross the runtime/model boundary. Runtime tool objects,
//! output parsers, agent graphs, credentials, and provider SDK types do not belong here. Rich
//! runtime types project into the lightweight model-facing definitions below before a call.

use std::borrow::Borrow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ResolvedModelSettings;
use crate::{
    error::{Error, Result},
    item::{AgentId, ModelInputItem},
    prompt::{CachePlan, ContentHash},
    strict::canonicalize_json,
};

/// An opaque identifier for a provider-managed server-side conversation.
///
/// This is distinct from local authoritative conversation session history identifiers
/// ([`SessionId`](crate::session::SessionId)) and command execution session identifiers
/// ([`ExecSessionId`](crate::event::exec::ExecSessionId)).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderConversationId(String);

impl ProviderConversationId {
    /// Creates a provider conversation identifier from a string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ProviderConversationId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ProviderConversationId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ProviderConversationId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl AsRef<str> for ProviderConversationId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for ProviderConversationId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

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
    ConversationId(ProviderConversationId),
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
    pub fn conversation_id(&self) -> Option<&ProviderConversationId> {
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
        let mut input_schema = input_schema;
        canonicalize_json(&mut input_schema);
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

    /// What one advertised entry costs in a turn's tool table, in bytes.
    ///
    /// The three model-visible parts are counted — the input schema, the name, and the
    /// description — and each provider's envelope around them is not. That envelope differs per
    /// wire format, so including it would make the same tool measure differently depending on
    /// which endpoint the run happens to use, and a surface budget has to be comparable across
    /// providers to be worth stating.
    ///
    /// This is the single definition of the byte measure, and it sits on the projection rather
    /// than on [`ToolSchema`](crate::tool::ToolSchema) because the projection is what a provider
    /// is sent: a tool that advertises itself under a different name than it routes under is
    /// billed for the name it advertises. [`Self::advertised_chars`] counts the same rendering in
    /// the unit a token estimate needs; the two share one renderer, so which parts an entry
    /// advertises is still defined exactly once.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the input schema cannot be rendered.
    pub fn advertised_bytes(&self) -> Result<usize> {
        advertised_definition_bytes(&self.name, self.description.as_deref(), &self.input_schema)
    }

    /// The same advertised entry counted in characters rather than bytes.
    ///
    /// [`Self::advertised_bytes`] stays the measure a surface budget is stated in: a byte ceiling
    /// is what a transport actually bounds. A token estimate, in contrast, prices characters, and
    /// a multi-byte description would be charged several times over against a byte count. Both
    /// read the same rendered entry, so they can never disagree about which parts an entry
    /// advertises.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the input schema cannot be rendered.
    pub fn advertised_chars(&self) -> Result<usize> {
        advertised_definition_chars(&self.name, self.description.as_deref(), &self.input_schema)
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
        let mut input_schema = input_schema;
        canonicalize_json(&mut input_schema);
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

    /// What one advertised entry costs in a turn's model-action table.
    ///
    /// The measure matches [`ModelToolDefinition::advertised_bytes`]: tools and handoffs occupy
    /// the same provider namespace and use the same name, description, and input-schema fields.
    /// Provider envelopes are excluded because they vary by wire protocol.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the input schema cannot be rendered.
    pub fn advertised_bytes(&self) -> Result<usize> {
        advertised_definition_bytes(&self.name, self.description.as_deref(), &self.input_schema)
    }

    /// The same advertised entry counted in characters, matching
    /// [`ModelToolDefinition::advertised_chars`].
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the input schema cannot be rendered.
    pub fn advertised_chars(&self) -> Result<usize> {
        advertised_definition_chars(&self.name, self.description.as_deref(), &self.input_schema)
    }
}

/// Renders the model-visible text of one advertised entry.
///
/// The three parts an entry advertises are concatenated because only their combined length is
/// ever read. Keeping the rendering here rather than in each measure is what lets a byte budget
/// and a character-based token estimate disagree about the unit while provably counting the same
/// material: a part added to an entry reaches both measures at once.
fn render_advertised_definition(
    name: &str,
    description: Option<&str>,
    input_schema: &Value,
) -> Result<String> {
    let mut rendered = serde_json::to_string(input_schema).map_err(|error| {
        Error::config("failed to render advertised action schema").with_source(error)
    })?;
    rendered.push_str(name);
    if let Some(description) = description {
        rendered.push_str(description);
    }
    Ok(rendered)
}

fn advertised_definition_bytes(
    name: &str,
    description: Option<&str>,
    input_schema: &Value,
) -> Result<usize> {
    Ok(render_advertised_definition(name, description, input_schema)?.len())
}

fn advertised_definition_chars(
    name: &str,
    description: Option<&str>,
    input_schema: &Value,
) -> Result<usize> {
    Ok(
        render_advertised_definition(name, description, input_schema)?
            .chars()
            .count(),
    )
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
    ///
    /// The schema is canonicalized like a tool's, for the same reason: key order in a
    /// `serde_json::Value` depends on which feature set the dependency graph selected, and a
    /// request body that varies with that is neither reproducible in a snapshot nor stable across
    /// the calls a provider is being asked to cache.
    #[must_use]
    pub fn new(name: impl Into<String>, schema: Value) -> Self {
        let mut schema = schema;
        canonicalize_json(&mut schema);
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

    /// What this schema costs the model to read, in characters.
    ///
    /// A structured-output schema occupies the same request-time definition space as a tool, so it
    /// is measured the same way — minus a description, which this projection does not carry. It is
    /// deliberately absent from the byte-denominated tool-surface budget: a response format is not
    /// something a run trims to fit under a tool ceiling.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the schema cannot be rendered.
    pub fn advertised_chars(&self) -> Result<usize> {
        advertised_definition_chars(&self.name, None, &self.schema)
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
/// **Provider cache wire fields.** [`CachePlan`] states which bytes form the stable prefix and
/// which span of calls should share a cache entry. It stops there. `cache_control` breakpoints and
/// `prompt_cache_key` are wire vocabulary of one vendor each, and *whether a given endpoint honours
/// them* is a provider fact — not a protocol one — so both the field names and the decision to send
/// them belong to the adapter and to `ProviderQuirks`.
///
/// In particular `extra_body` is still the wrong home for a session cache key: that bucket is
/// static provider registration data, shared by every run of that provider, while a cache scope is
/// a runtime value with a session lifetime. A key placed there silently collapses every run onto
/// one routing bucket. When the typed runtime override lands it belongs on this type as a new
/// field; it is `#[non_exhaustive]` precisely so adding one is not a breaking change.
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
    cache_plan: Option<CachePlan>,
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
            cache_plan: None,
        }
    }

    /// Sets stable system instructions.
    #[must_use]
    pub fn with_system_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.system_instructions = Some(instructions.into());
        self
    }

    /// Attaches a provider-neutral cache plan for the stable system-instruction prefix.
    #[must_use]
    pub fn with_cache_plan(mut self, cache_plan: CachePlan) -> Self {
        self.cache_plan = Some(cache_plan);
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
    pub fn with_conversation_id(
        mut self,
        conversation_id: impl Into<ProviderConversationId>,
    ) -> Self {
        self.continuation = ConversationContinuation::ConversationId(conversation_id.into());
        self
    }

    /// Stable system instructions.
    #[must_use]
    pub fn system_instructions(&self) -> Option<&str> {
        self.system_instructions.as_deref()
    }

    /// Provider-neutral cache plan for the stable prefix, if caching is configured.
    #[must_use]
    pub const fn cache_plan(&self) -> Option<&CachePlan> {
        self.cache_plan.as_ref()
    }

    /// Verifies that the attached cache plan identifies these exact stable instructions.
    ///
    /// # Errors
    ///
    /// Returns an error if a cache plan is attached without system instructions or if its prefix
    /// hash differs from the instructions that will be sent to the provider.
    pub fn validate_cache_plan(&self) -> Result<()> {
        let Some(cache_plan) = &self.cache_plan else {
            return Ok(());
        };
        let instructions = self.system_instructions.as_deref().ok_or_else(|| {
            Error::caller("a prompt cache plan requires stable system instructions")
        })?;
        let actual_hash = ContentHash::compute(instructions);
        if cache_plan.prefix_hash() != &actual_hash {
            return Err(Error::caller(format!(
                "prompt cache plan names prefix hash `{}`, but stable system instructions hash to `{actual_hash}`",
                cache_plan.prefix_hash()
            )));
        }
        Ok(())
    }

    /// Replaces the input items, keeping every other resolved field.
    ///
    /// A context transform that reprojects the history needs exactly this and nothing else. It
    /// lives on the type rather than in the caller because this struct is `#[non_exhaustive]` so
    /// that fields can be added: a caller that rebuilt the request field by field would silently
    /// drop the next one, with no compile error to say so.
    #[must_use]
    pub fn with_input(mut self, input: Vec<ModelInputItem>) -> Self {
        self.input = input;
        self
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
