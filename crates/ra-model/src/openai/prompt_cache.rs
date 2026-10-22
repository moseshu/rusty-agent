//! Explicit `OpenAI` prompt-cache configuration shared by Responses and Chat Completions.
//!
//! Settings use the selected provider's extras bucket. Explicit values are sent unchanged,
//! independently of endpoint quirks and automatic cache-key generation. Content metadata uses
//! the family namespace rather than a registration key so compatible registrations can replay it.
//!
//! Ported from the Agents SDK settings and converter, and openai-python wire types; see NOTICE
//! for pinned revisions, original files and retained license texts.

use std::collections::BTreeMap;

use ra_core::model::{ModelSettings, ProviderKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Namespace interpreted by both `OpenAI` protocol adapters.
pub const OPENAI_METADATA_NAMESPACE: &str = "openai";

/// Whether the server also chooses an implicit cache breakpoint.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMode {
    /// The server chooses an implicit breakpoint in addition to explicit marks.
    Implicit,
    /// Only caller-selected breakpoints are used.
    Explicit,
}

/// Minimum cache lifetime supported by the pinned `OpenAI` SDK contract.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheTtl {
    /// A minimum lifetime of thirty minutes.
    #[serde(rename = "30m")]
    ThirtyMinutes,
}

/// Request-level prompt-cache options; omitted members retain server defaults.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<PromptCacheMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ttl: Option<PromptCacheTtl>,
}

impl PromptCacheOptions {
    /// Leaves mode and TTL unspecified instead of injecting server defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mode: None,
            ttl: None,
        }
    }

    /// Selects implicit or explicit-only caching.
    #[must_use]
    pub const fn with_mode(mut self, mode: PromptCacheMode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Sets minimum lifetime, independently of maximum retention policy.
    #[must_use]
    pub const fn with_ttl(mut self, ttl: PromptCacheTtl) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Explicitly selected caching mode.
    #[must_use]
    pub const fn mode(self) -> Option<PromptCacheMode> {
        self.mode
    }

    /// Explicitly selected minimum lifetime.
    #[must_use]
    pub const fn ttl(self) -> Option<PromptCacheTtl> {
        self.ttl
    }
}

/// Maximum retention policy, deprecated by openai-python in favor of `prompt_cache_options.ttl`.
///
/// Retained for Agents SDK compatibility. Retention is a maximum policy and TTL a minimum
/// lifetime; the fields are independent. The pinned API types specify that GPT-5.5 and later
/// accept only `24h`. Model support remains the endpoint's responsibility.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheRetention {
    /// In-memory retention.
    InMemory,
    /// Extended retention up to twenty-four hours.
    #[serde(rename = "24h")]
    TwentyFourHours,
}

/// Sets one provider's complete options object; later layers replace it as a whole.
#[must_use]
pub fn with_prompt_cache_options(
    settings: ModelSettings,
    provider: ProviderKey,
    options: PromptCacheOptions,
) -> ModelSettings {
    settings.with_extra_body_value(provider, "prompt_cache_options", json!(options))
}

/// Sets one provider's retention policy without changing its options or TTL.
///
/// The wire field is deprecated by openai-python and retained for Agents SDK compatibility.
/// Prefer [`with_prompt_cache_options`] for new configuration.
#[must_use]
pub fn with_prompt_cache_retention(
    settings: ModelSettings,
    provider: ProviderKey,
    retention: PromptCacheRetention,
) -> ModelSettings {
    settings.with_extra_body_value(provider, "prompt_cache_retention", json!(retention))
}

/// An exact content boundary, with lifetime inherited from the request's options.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PromptCacheBreakpoint {
    /// A caller-selected prefix boundary.
    Explicit,
}

impl PromptCacheBreakpoint {
    /// Metadata for `TextBlock`, `ImageBlock` or `FileBlock::with_provider_metadata`.
    #[must_use]
    pub fn metadata(self) -> Value {
        json!({"prompt_cache_breakpoint": self})
    }
}

pub(crate) fn breakpoint(metadata: &BTreeMap<String, Value>) -> Option<&Value> {
    metadata
        .get(OPENAI_METADATA_NAMESPACE)?
        .get("prompt_cache_breakpoint")
        .filter(|value| !value.is_null())
}

pub(crate) fn apply_breakpoint(metadata: &BTreeMap<String, Value>, part: &mut Value) {
    if let Some(mark) = breakpoint(metadata) {
        part["prompt_cache_breakpoint"] = mark.clone();
    }
}

/// Restores a non-null wire mark into the provider family's opaque metadata.
pub(crate) fn lift_breakpoint(part: &Value) -> Option<Value> {
    part.get("prompt_cache_breakpoint")
        .filter(|mark| !mark.is_null())
        .map(|mark| json!({"prompt_cache_breakpoint": mark}))
}
