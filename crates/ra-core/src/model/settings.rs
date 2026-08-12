//! Provider-neutral model settings and immutable four-layer resolution.
//!
//! Resolution order is provider registration, agent defaults, resolved-model defaults, then run
//! overrides. Scalar settings use an explicit `Option<T>` so an unset value remains distinct from
//! an explicit `0`, `false`, or enum default. Transport extras merge without entering the
//! trace-safe projection.
//!
//! # `max_tokens` and `timeout` are constrained differently
//!
//! `max_tokens` is the one setting where the four layers do not simply stack, because three
//! different kinds of statement share the field:
//!
//! | Role | Layers | Rule |
//! | --- | --- | --- |
//! | registry default — "use this unless told otherwise" | provider, then model | later wins, so the model-specific value beats the provider-wide one |
//! | user intent — "I want this much" | agent, then run | later wins, and any intent beats a registry default |
//! | hard ceiling — "more than this only buys a 400" | model | clamps whatever the two rules above produced |
//!
//! Two failure modes this avoids, both of which silently discard a value someone wrote down:
//!
//! - Clamping to the minimum of every layer would make an agent's short-answer default an
//!   unraisable ceiling, so a run that legitimately needs a long answer could never ask for one.
//! - Letting the provider layer supply the value would make the coarsest registration beat the most
//!   specific one. A provider-wide fallback exists because some endpoints require the field at all
//!   — Anthropic does — and it must not override a per-model limit that was registered precisely
//!   because that model is different.
//!
//! `timeout` is the opposite and does take the minimum of all four layers. It is a latency bound
//! rather than a capability: any layer that wants to wait less has standing to say so, and a
//! longer request still completes — it is just abandoned earlier.
//!
//! # Two `max_tokens` obligations that land outside this module
//!
//! - **Anthropic requires it.** Both `OpenAI` protocols treat the output limit as optional and
//!   omit the field when unset, but Anthropic Messages rejects a request without `max_tokens`. So
//!   for that provider the resolved-model layer is not only a ceiling, it is a mandatory fallback:
//!   all four layers resolving to `None` must still produce a value. Supplying it belongs to
//!   provider registration, not here — `ra-core` does not know which endpoint is in play.
//! - **It is coupled to the thinking budget.** Anthropic requires `max_tokens` to exceed
//!   `thinking.budget_tokens`, so [`ThinkingConfig::Enabled`] with a large budget and a small
//!   `max_tokens` is a request that can only 400. The check is not performed here on purpose: the
//!   constraint is Anthropic's, `OpenAI`'s `Effort` has no equivalent relationship, and encoding it
//!   in a protocol-neutral type would violate the rule that protocol-specific facts stay out of
//!   the neutral layer. It belongs to the adapter.

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
pub const MODEL_SETTINGS_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);
/// Current MCP-tool-choice schema version.
pub const MCP_TOOL_CHOICE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// A deterministic JSON object used by request extras.
pub type JsonMap = BTreeMap<String, Value>;

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
/// defaults, and run overrides. Call [`Self::resolve`] on the provider layer.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    extra_headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    extra_query: JsonMap,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    extra_body: BTreeMap<ProviderKey, JsonMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry: Option<ModelRetrySettings>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
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
            metadata: BTreeMap::new(),
            extra_headers: BTreeMap::new(),
            extra_query: BTreeMap::new(),
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
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Adds or replaces one transport header in this layer.
    #[must_use]
    pub fn with_extra_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.insert(key.into(), value.into());
        self
    }

    /// Adds or replaces one query parameter in this layer.
    #[must_use]
    pub fn with_extra_query(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extra_query.insert(key.into(), value.into());
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

    /// Resolves four immutable layers for one active provider.
    ///
    /// `self` is the provider-registration layer. Later arguments have increasing precedence.
    /// Only the active provider's request-body bucket is copied into the result.
    ///
    /// `max_tokens` resolves through three roles rather than plain layer stacking, and `timeout`
    /// takes the shortest of all four; see the module documentation for both rules.
    #[must_use]
    pub fn resolve(
        &self,
        provider: &ProviderKey,
        agent_defaults: &Self,
        model_defaults: &Self,
        run_overrides: &Self,
    ) -> ResolvedModelSettings {
        let layers = [self, agent_defaults, model_defaults, run_overrides];
        // See the module docs: registry defaults and user intent are separate lanes, and the model
        // layer additionally clamps whichever of them produced a value.
        let registry_default = last_some([self, model_defaults].map(|layer| layer.max_tokens));
        let user_intent = last_some([agent_defaults, run_overrides].map(|layer| layer.max_tokens));

        ResolvedModelSettings {
            provider: provider.clone(),
            temperature: last_some(layers.map(|layer| layer.temperature)),
            top_p: last_some(layers.map(|layer| layer.top_p)),
            frequency_penalty: last_some(layers.map(|layer| layer.frequency_penalty)),
            presence_penalty: last_some(layers.map(|layer| layer.presence_penalty)),
            max_tokens: clamp_to_limit(user_intent.or(registry_default), model_defaults.max_tokens),
            timeout: strictest(layers.map(|layer| layer.timeout)),
            tool_choice: layers
                .iter()
                .rev()
                .find_map(|layer| layer.tool_choice.clone()),
            parallel_tool_calls: last_some(layers.map(|layer| layer.parallel_tool_calls)),
            thinking: last_some(layers.map(|layer| layer.thinking)),
            effort: last_some(layers.map(|layer| layer.effort)),
            metadata: merge_string_maps(layers.map(|layer| &layer.metadata)),
            extra_headers: merge_string_maps(layers.map(|layer| &layer.extra_headers)),
            extra_query: merge_json_maps(layers.map(|layer| &layer.extra_query), false),
            extra_body: merge_json_maps(
                layers
                    .iter()
                    .filter_map(|layer| layer.extra_body.get(provider)),
                true,
            ),
            retry: ModelRetrySettings::merge(layers.map(|layer| layer.retry.as_ref())),
        }
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
    pub const fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// Transport headers in this unresolved layer.
    #[must_use]
    pub const fn extra_headers(&self) -> &BTreeMap<String, String> {
        &self.extra_headers
    }

    /// Transport query in this unresolved layer.
    #[must_use]
    pub const fn extra_query(&self) -> &JsonMap {
        &self.extra_query
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

    /// Strictest output-token limit across all layers.
    #[must_use]
    pub const fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    /// Shortest timeout across all layers.
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

    /// Merged response metadata.
    #[must_use]
    pub const fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// Merged transport headers. These are excluded from trace projection.
    #[must_use]
    pub const fn extra_headers(&self) -> &BTreeMap<String, String> {
        &self.extra_headers
    }

    /// Merged transport query. This is excluded from trace projection.
    #[must_use]
    pub const fn extra_query(&self) -> &JsonMap {
        &self.extra_query
    }

    /// Deep-merged request extras for only the active provider.
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
            schema_version: MODEL_SETTINGS_SCHEMA_VERSION,
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

fn strictest<T: Ord>(values: impl IntoIterator<Item = Option<T>>) -> Option<T> {
    values.into_iter().flatten().min()
}

/// Applies a hard capability limit to a requested value.
///
/// An absent limit leaves the request alone, and an absent request falls back to the limit, so a
/// model registration that states only a ceiling still supplies a usable value.
fn clamp_to_limit<T: Ord>(requested: Option<T>, limit: Option<T>) -> Option<T> {
    match (requested, limit) {
        (Some(requested), Some(limit)) => Some(requested.min(limit)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn merge_string_maps<'a>(
    maps: impl IntoIterator<Item = &'a BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    let mut merged = BTreeMap::new();
    for map in maps {
        merged.extend(map.iter().map(|(key, value)| (key.clone(), value.clone())));
    }
    merged
}

fn merge_json_maps<'a>(maps: impl IntoIterator<Item = &'a JsonMap>, deep: bool) -> JsonMap {
    let mut merged = JsonMap::new();
    for map in maps {
        for (key, value) in map {
            if deep && let Some(existing) = merged.get_mut(key) {
                deep_merge_value(existing, value);
                continue;
            }
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn deep_merge_value(target: &mut Value, incoming: &Value) {
    if let (Value::Object(target), Value::Object(incoming)) = (&mut *target, incoming) {
        for (key, value) in incoming {
            if let Some(existing) = target.get_mut(key) {
                deep_merge_value(existing, value);
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    } else {
        *target = incoming.clone();
    }
}
