//! Provider-neutral model settings and immutable four-layer resolution.
//!
//! Resolution order is provider registration, resolved-model defaults, agent defaults, then run
//! overrides. Scalar settings use an explicit `Option<T>` so an unset value remains distinct from
//! an explicit `0`, `false`, or enum default. Metadata, headers, and query maps are optional
//! whole-map overrides; request-body extras merge at the top level for the active provider.
//! Transport extras stay out of the trace-safe projection.
//!
//! Model defaults fill fields that agent and run settings leave unset. This is a deliberate Rust
//! registry adaptation: model registrations carry explicit per-model defaults, and dropping all
//! of them because an agent set one unrelated field would discard settings such as that model's
//! reasoning default. Codex follows the same field-by-field pattern for explicit reasoning effort
//! and model metadata defaults. The `OpenAI` Agents SDK instead drops all implicit model-specific
//! defaults when any agent model setting is explicit.
//!
//! `max_tokens` is a request default and follows the same layer order. A model's hard output limit
//! is registered separately, so exceeding it is a configuration error rather than silent
//! truncation. Provider-required defaults, such as Anthropic's mandatory `max_tokens`, remain in
//! the provider layer. The Anthropic constraint that `max_tokens` exceed `thinking.budget_tokens`
//! stays in its adapter because it is not shared by the other protocols.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::duration::option_millis;
use super::retry::ModelRetrySettings;
use crate::compat::{SchemaVersion, Unknown};

/// Current model-settings schema version.
pub const MODEL_SETTINGS_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(2);
/// Current trace projection schema version for resolved model settings.
const TRACE_MODEL_SETTINGS_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);
/// Current MCP-tool-choice schema version.
pub const MCP_TOOL_CHOICE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// A deterministic JSON object used by request extras.
pub type JsonMap = BTreeMap<String, Value>;

static EMPTY_METADATA: BTreeMap<String, String> = BTreeMap::new();
static EMPTY_JSON_MAP: JsonMap = BTreeMap::new();

/// Provider registration identity used to isolate provider-specific request fields.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderKey(String);

impl ProviderKey {
    /// Creates a provider registration key.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// String representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Protocol-neutral tool-selection policy.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Let the model decide whether to call a tool.
    Auto,
    /// Require at least one tool call.
    Required,
    /// Disable tool calls.
    None,
    /// Require a tool by name.
    Tool(String),
    /// Require a tool hosted by a named MCP server.
    Mcp(McpToolChoice),
}

/// A specific MCP tool selection.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolChoice {
    schema_version: SchemaVersion,
    server: String,
    name: String,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl McpToolChoice {
    /// Creates an MCP tool selection.
    #[must_use]
    pub fn new(server: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema_version: MCP_TOOL_CHOICE_SCHEMA_VERSION,
            server: server.into(),
            name: name.into(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// MCP server registration name.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Tool name within the server.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Provider-neutral model thinking configuration.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// Let the provider adapt the thinking budget.
    Adaptive,
    /// Enable thinking with an explicit token budget.
    Enabled {
        /// Maximum thinking tokens.
        budget_tokens: u64,
    },
    /// Disable model thinking.
    Disabled,
}

/// User-selected model effort. Adapters lower it without automatic routing.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// Extra-high effort.
    #[serde(rename = "xhigh")]
    XHigh,
    /// Maximum effort.
    Max,
}

impl Effort {
    /// Stable provider-facing label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Optional settings contributed by one resolution layer.
///
/// The same type represents provider registration defaults, agent defaults, resolved-model
/// defaults, and run overrides. Resolve them with [`SettingsLayers`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(default)]
pub struct ModelSettings {
    schema_version: SchemaVersion,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(with = "option_millis", skip_serializing_if = "Option::is_none")]
    timeout: Option<Duration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<Effort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_query: Option<JsonMap>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    extra_body: BTreeMap<ProviderKey, JsonMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry: Option<ModelRetrySettings>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ModelSettingsData {
    schema_version: Option<SchemaVersion>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    frequency_penalty: Option<f64>,
    presence_penalty: Option<f64>,
    max_tokens: Option<u64>,
    #[serde(with = "option_millis")]
    timeout: Option<Duration>,
    tool_choice: Option<ToolChoice>,
    parallel_tool_calls: Option<bool>,
    thinking: Option<ThinkingConfig>,
    effort: Option<Effort>,
    metadata: Option<BTreeMap<String, String>>,
    extra_headers: Option<BTreeMap<String, String>>,
    extra_query: Option<JsonMap>,
    extra_body: BTreeMap<ProviderKey, JsonMap>,
    retry: Option<ModelRetrySettings>,
    #[serde(flatten)]
    unknown: Unknown,
}

impl<'de> Deserialize<'de> for ModelSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let data = ModelSettingsData::deserialize(deserializer)?;
        let schema_version = data.schema_version.unwrap_or(MODEL_SETTINGS_SCHEMA_VERSION);
        let is_legacy = schema_version.get() == 1;
        Ok(Self {
            // Legacy empty maps are normalized to unset below, so subsequent writes must use the
            // schema whose empty-map semantics match the normalized in-memory representation.
            schema_version: if is_legacy {
                MODEL_SETTINGS_SCHEMA_VERSION
            } else {
                schema_version
            },
            temperature: data.temperature,
            top_p: data.top_p,
            frequency_penalty: data.frequency_penalty,
            presence_penalty: data.presence_penalty,
            max_tokens: data.max_tokens,
            timeout: data.timeout,
            tool_choice: data.tool_choice,
            parallel_tool_calls: data.parallel_tool_calls,
            thinking: data.thinking,
            effort: data.effort,
            metadata: data.metadata.filter(|map| !is_legacy || !map.is_empty()),
            extra_headers: data
                .extra_headers
                .filter(|map| !is_legacy || !map.is_empty()),
            extra_query: data.extra_query.filter(|map| !is_legacy || !map.is_empty()),
            extra_body: data.extra_body,
            retry: data.retry,
            unknown: data.unknown,
        })
    }
}

impl ModelSettings {
    /// Creates a layer with every setting unset.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: MODEL_SETTINGS_SCHEMA_VERSION,
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: None,
            timeout: None,
            tool_choice: None,
            parallel_tool_calls: None,
            thinking: None,
            effort: None,
            metadata: None,
            extra_headers: None,
            extra_query: None,
            extra_body: BTreeMap::new(),
            retry: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets sampling temperature.
    #[must_use]
    pub const fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Sets nucleus-sampling probability.
    #[must_use]
    pub const fn with_top_p(mut self, top_p: f64) -> Self {
        self.top_p = Some(top_p);
        self
    }

    /// Sets frequency penalty.
    #[must_use]
    pub const fn with_frequency_penalty(mut self, penalty: f64) -> Self {
        self.frequency_penalty = Some(penalty);
        self
    }

    /// Sets presence penalty.
    #[must_use]
    pub const fn with_presence_penalty(mut self, penalty: f64) -> Self {
        self.presence_penalty = Some(penalty);
        self
    }

    /// Constrains maximum output tokens.
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Constrains request timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets tool-selection policy.
    #[must_use]
    pub fn with_tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }

    /// Explicitly enables or disables parallel tool calls.
    #[must_use]
    pub const fn with_parallel_tool_calls(mut self, enabled: bool) -> Self {
        self.parallel_tool_calls = Some(enabled);
        self
    }

    /// Sets model thinking configuration.
    #[must_use]
    pub const fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// Sets model effort.
    #[must_use]
    pub const fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Adds or replaces one metadata entry in this layer.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata
            .get_or_insert_with(BTreeMap::new)
            .insert(key.into(), value.into());
        self
    }

    /// Replaces this layer's metadata map, including with an empty map to clear inherited values.
    #[must_use]
    pub fn with_metadata_map(mut self, metadata: BTreeMap<String, String>) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Adds or replaces one transport header in this layer.
    #[must_use]
    pub fn with_extra_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers
            .get_or_insert_with(BTreeMap::new)
            .insert(key.into(), value.into());
        self
    }

    /// Replaces this layer's transport headers, including with an empty map to clear inherited values.
    #[must_use]
    pub fn with_extra_headers_map(mut self, headers: BTreeMap<String, String>) -> Self {
        self.extra_headers = Some(headers);
        self
    }

    /// Adds or replaces one query parameter in this layer.
    #[must_use]
    pub fn with_extra_query(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extra_query
            .get_or_insert_with(JsonMap::new)
            .insert(key.into(), value.into());
        self
    }

    /// Replaces this layer's query parameters, including with an empty map to clear inherited values.
    #[must_use]
    pub fn with_extra_query_map(mut self, query: JsonMap) -> Self {
        self.extra_query = Some(query);
        self
    }

    /// Replaces one provider's request-body bucket in this layer.
    #[must_use]
    pub fn with_extra_body(mut self, provider: ProviderKey, body: JsonMap) -> Self {
        self.extra_body.insert(provider, body);
        self
    }

    /// Adds or replaces one field in a provider's request-body bucket.
    #[must_use]
    pub fn with_extra_body_value(
        mut self,
        provider: ProviderKey,
        key: impl Into<String>,
        value: impl Into<Value>,
    ) -> Self {
        self.extra_body
            .entry(provider)
            .or_default()
            .insert(key.into(), value.into());
        self
    }

    /// Sets runner-managed retry settings.
    #[must_use]
    pub fn with_retry(mut self, retry: ModelRetrySettings) -> Self {
        self.retry = Some(retry);
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Sampling temperature in this unresolved layer.
    #[must_use]
    pub const fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    /// Nucleus-sampling probability in this unresolved layer.
    #[must_use]
    pub const fn top_p(&self) -> Option<f64> {
        self.top_p
    }

    /// Frequency penalty in this unresolved layer.
    #[must_use]
    pub const fn frequency_penalty(&self) -> Option<f64> {
        self.frequency_penalty
    }

    /// Presence penalty in this unresolved layer.
    #[must_use]
    pub const fn presence_penalty(&self) -> Option<f64> {
        self.presence_penalty
    }

    /// Output-token constraint in this unresolved layer.
    #[must_use]
    pub const fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    /// Timeout constraint in this unresolved layer.
    #[must_use]
    pub const fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Tool-selection policy in this unresolved layer.
    #[must_use]
    pub const fn tool_choice(&self) -> Option<&ToolChoice> {
        self.tool_choice.as_ref()
    }

    /// Parallel-tool-call setting in this unresolved layer.
    #[must_use]
    pub const fn parallel_tool_calls(&self) -> Option<bool> {
        self.parallel_tool_calls
    }

    /// Thinking configuration in this unresolved layer.
    #[must_use]
    pub const fn thinking(&self) -> Option<ThinkingConfig> {
        self.thinking
    }

    /// Effort setting in this unresolved layer.
    #[must_use]
    pub const fn effort(&self) -> Option<Effort> {
        self.effort
    }

    /// Response metadata in this unresolved layer.
    #[must_use]
    pub fn metadata(&self) -> &BTreeMap<String, String> {
        self.metadata.as_ref().unwrap_or(&EMPTY_METADATA)
    }

    /// Transport headers in this unresolved layer.
    #[must_use]
    pub fn extra_headers(&self) -> &BTreeMap<String, String> {
        self.extra_headers.as_ref().unwrap_or(&EMPTY_METADATA)
    }

    /// Transport query in this unresolved layer.
    #[must_use]
    pub fn extra_query(&self) -> &JsonMap {
        self.extra_query.as_ref().unwrap_or(&EMPTY_JSON_MAP)
    }

    /// Per-provider request-body buckets in this unresolved layer.
    #[must_use]
    pub const fn extra_body(&self) -> &BTreeMap<ProviderKey, JsonMap> {
        &self.extra_body
    }

    /// Retry configuration in this unresolved layer.
    #[must_use]
    pub const fn retry(&self) -> Option<&ModelRetrySettings> {
        self.retry.as_ref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self::new()
    }
}

/// The four named settings layers for one model-resolution operation.
///
/// Start with [`Self::new`] and set only the named layers that have values. Unset layers resolve as
/// empty settings.
#[derive(Debug, Clone, Copy, Default)]
pub struct SettingsLayers<'a> {
    provider: Option<&'a ModelSettings>,
    model: Option<&'a ModelSettings>,
    agent: Option<&'a ModelSettings>,
    run: Option<&'a ModelSettings>,
}

impl<'a> SettingsLayers<'a> {
    /// Creates empty provider, model, agent, and run settings layers.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            provider: None,
            model: None,
            agent: None,
            run: None,
        }
    }

    /// Sets provider registration defaults.
    #[must_use]
    pub const fn with_provider(mut self, provider: &'a ModelSettings) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Sets defaults registered for the selected model.
    #[must_use]
    pub const fn with_model(mut self, model: &'a ModelSettings) -> Self {
        self.model = Some(model);
        self
    }

    /// Sets agent-level settings.
    #[must_use]
    pub const fn with_agent(mut self, agent: &'a ModelSettings) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Sets settings for this run.
    #[must_use]
    pub const fn with_run(mut self, run: &'a ModelSettings) -> Self {
        self.run = Some(run);
        self
    }

    /// Resolves the named layers for the active provider.
    #[must_use]
    pub fn resolve(self, provider: &ProviderKey) -> ResolvedModelSettings {
        let empty = ModelSettings::new();
        let layers = [
            self.provider.unwrap_or(&empty),
            self.model.unwrap_or(&empty),
            self.agent.unwrap_or(&empty),
            self.run.unwrap_or(&empty),
        ];

        ResolvedModelSettings {
            provider: provider.clone(),
            temperature: last_some(layers.map(|layer| layer.temperature)),
            top_p: last_some(layers.map(|layer| layer.top_p)),
            frequency_penalty: last_some(layers.map(|layer| layer.frequency_penalty)),
            presence_penalty: last_some(layers.map(|layer| layer.presence_penalty)),
            max_tokens: last_some(layers.map(|layer| layer.max_tokens)),
            timeout: last_some(layers.map(|layer| layer.timeout)),
            tool_choice: layers
                .iter()
                .rev()
                .find_map(|layer| layer.tool_choice.clone()),
            parallel_tool_calls: last_some(layers.map(|layer| layer.parallel_tool_calls)),
            thinking: last_some(layers.map(|layer| layer.thinking)),
            effort: last_some(layers.map(|layer| layer.effort)),
            metadata: last_some(layers.map(|layer| layer.metadata.as_ref()))
                .cloned()
                .unwrap_or_default(),
            extra_headers: last_some(layers.map(|layer| layer.extra_headers.as_ref()))
                .cloned()
                .unwrap_or_default(),
            extra_query: last_some(layers.map(|layer| layer.extra_query.as_ref()))
                .cloned()
                .unwrap_or_default(),
            extra_body: merge_json_maps(
                layers
                    .iter()
                    .filter_map(|layer| layer.extra_body.get(provider)),
            ),
            retry: ModelRetrySettings::merge(layers.map(|layer| layer.retry.as_ref())),
        }
    }
}

/// Effective model settings for one provider and one model request.
///
/// This type deliberately does not implement `Serialize`; tracing must go through
/// [`Self::to_traceable_value`], which omits transport extras.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModelSettings {
    provider: ProviderKey,
    temperature: Option<f64>,
    top_p: Option<f64>,
    frequency_penalty: Option<f64>,
    presence_penalty: Option<f64>,
    max_tokens: Option<u64>,
    timeout: Option<Duration>,
    tool_choice: Option<ToolChoice>,
    parallel_tool_calls: Option<bool>,
    thinking: Option<ThinkingConfig>,
    effort: Option<Effort>,
    metadata: BTreeMap<String, String>,
    extra_headers: BTreeMap<String, String>,
    extra_query: JsonMap,
    extra_body: JsonMap,
    retry: Option<ModelRetrySettings>,
}

impl ResolvedModelSettings {
    /// Drops tool-selection settings that the turn's advertised tool surface cannot satisfy.
    ///
    /// `tool_choice` and `parallel_tool_calls` are the two settings whose meaning depends on which
    /// tools exist, and the surface is not final until dynamic availability has been resolved.
    /// This is why turn preparation resolves settings *after* tools rather than before: without
    /// this step a selector that survived the four-layer merge can name a tool that this turn does
    /// not advertise, and every provider has to reject the request on its own.
    ///
    /// Unsatisfiable selections **degrade to the provider default instead of erroring**. A tool
    /// switched off for one turn is the intended use of dynamic availability, and ending the run
    /// over it would make that feature unusable. Two selections survive an empty surface:
    /// [`ToolChoice::None`] still says something true, and [`ToolChoice::Mcp`] names a
    /// server-hosted tool that never appears in the neutral surface.
    #[must_use]
    pub fn reconcile_tool_surface<'a>(
        mut self,
        advertised: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let advertised: BTreeSet<&str> = advertised.into_iter().collect();

        self.tool_choice = match self.tool_choice.take() {
            Some(choice @ (ToolChoice::None | ToolChoice::Mcp(_))) => Some(choice),
            Some(ToolChoice::Tool(name)) => advertised
                .contains(name.as_str())
                .then_some(ToolChoice::Tool(name)),
            Some(_) if advertised.is_empty() => None,
            other => other,
        };
        if advertised.is_empty() {
            self.parallel_tool_calls = None;
        }

        self
    }

    /// Resets the resolved tool choice to its default after tool use.
    ///
    /// The runtime applies this to the resolved value only when the agent enables resets and has
    /// used a resettable tool during this run, matching the framework's `maybe_reset_tool_choice`.
    #[must_use]
    pub fn reset_tool_choice(mut self) -> Self {
        self.tool_choice = None;
        self
    }

    /// Active provider registration key.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    /// Sampling temperature.
    #[must_use]
    pub const fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    /// Nucleus-sampling probability.
    #[must_use]
    pub const fn top_p(&self) -> Option<f64> {
        self.top_p
    }

    /// Frequency penalty.
    #[must_use]
    pub const fn frequency_penalty(&self) -> Option<f64> {
        self.frequency_penalty
    }

    /// Presence penalty.
    #[must_use]
    pub const fn presence_penalty(&self) -> Option<f64> {
        self.presence_penalty
    }

    /// Effective output-token default or override.
    #[must_use]
    pub const fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    /// Effective timeout after layer precedence.
    #[must_use]
    pub const fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Tool-selection policy.
    #[must_use]
    pub const fn tool_choice(&self) -> Option<&ToolChoice> {
        self.tool_choice.as_ref()
    }

    /// Explicit parallel-tool-call setting.
    #[must_use]
    pub const fn parallel_tool_calls(&self) -> Option<bool> {
        self.parallel_tool_calls
    }

    /// Model thinking configuration.
    #[must_use]
    pub const fn thinking(&self) -> Option<ThinkingConfig> {
        self.thinking
    }

    /// Model effort.
    #[must_use]
    pub const fn effort(&self) -> Option<Effort> {
        self.effort
    }

    /// Effective response metadata map.
    #[must_use]
    pub const fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// Effective transport headers. These are excluded from trace projection.
    #[must_use]
    pub const fn extra_headers(&self) -> &BTreeMap<String, String> {
        &self.extra_headers
    }

    /// Effective transport query. This is excluded from trace projection.
    #[must_use]
    pub const fn extra_query(&self) -> &JsonMap {
        &self.extra_query
    }

    /// Top-level merged request extras for only the active provider.
    #[must_use]
    pub const fn extra_body(&self) -> &JsonMap {
        &self.extra_body
    }

    /// Deep-merged retry configuration.
    #[must_use]
    pub const fn retry(&self) -> Option<&ModelRetrySettings> {
        self.retry.as_ref()
    }

    /// Serializes only protocol-neutral, trace-safe settings.
    ///
    /// Provider request body, headers, and query are intentionally absent because they may contain
    /// credentials or provider-private values.
    pub fn to_traceable_value(&self) -> serde_json::Result<Value> {
        // `schema_version` describes this projection's own shape, not any input layer's. A reader
        // needs to know how to parse what it just received; the layers it was derived from are
        // gone by now and their unknown fields never reach here.
        #[derive(Serialize)]
        struct Traceable<'a> {
            schema_version: SchemaVersion,
            #[serde(skip_serializing_if = "Option::is_none")]
            temperature: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            top_p: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            frequency_penalty: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            presence_penalty: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            max_tokens: Option<u64>,
            #[serde(with = "option_millis", skip_serializing_if = "Option::is_none")]
            timeout: Option<Duration>,
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_choice: Option<&'a ToolChoice>,
            #[serde(skip_serializing_if = "Option::is_none")]
            parallel_tool_calls: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            thinking: Option<ThinkingConfig>,
            #[serde(skip_serializing_if = "Option::is_none")]
            effort: Option<Effort>,
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            metadata: &'a BTreeMap<String, String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            retry: Option<&'a ModelRetrySettings>,
        }

        serde_json::to_value(Traceable {
            schema_version: TRACE_MODEL_SETTINGS_SCHEMA_VERSION,
            temperature: self.temperature,
            top_p: self.top_p,
            frequency_penalty: self.frequency_penalty,
            presence_penalty: self.presence_penalty,
            max_tokens: self.max_tokens,
            timeout: self.timeout,
            tool_choice: self.tool_choice.as_ref(),
            parallel_tool_calls: self.parallel_tool_calls,
            thinking: self.thinking,
            effort: self.effort,
            metadata: &self.metadata,
            retry: self.retry.as_ref(),
        })
    }
}

fn last_some<T>(values: impl IntoIterator<Item = Option<T>>) -> Option<T> {
    values.into_iter().flatten().last()
}

fn merge_json_maps<'a>(maps: impl IntoIterator<Item = &'a JsonMap>) -> JsonMap {
    let mut merged = JsonMap::new();
    for map in maps {
        merged.extend(map.iter().map(|(key, value)| (key.clone(), value.clone())));
    }
    merged
}
