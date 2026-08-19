//! Token totals for a single model response.
//!
//! # Tokens only
//!
//! There is no money here, and that is a decision rather than a gap. No provider reports a charge
//! next to its usage, so any amount this type carried could only come from a built-in price table —
//! per provider, per model, per token class, revised without notice, and wrong silently. Every
//! response keeps its own totals, which is what a host needs to apply its own contracted rates.
//!
//! # Details are subsets, not additions
//!
//! `cached_input_tokens` and `cache_write_tokens` are the part of `input_tokens` that was read from
//! or written to the provider's prompt cache; `reasoning_tokens` is the part of `output_tokens`
//! spent thinking. None of them is separate spend, which is why
//! [`total_tokens`](Usage::total_tokens) is `input + output` and adding a detail would double count.
//!
//! **An adapter is responsible for normalizing a provider that reports otherwise.** Anthropic
//! returns cache-read and cache-creation counts *outside* `input_tokens`; mapping those straight
//! through would leave them out of every total, and the token budget reads exactly that total.

use serde::{Deserialize, Serialize};

use crate::compat::{SchemaVersion, Unknown};

/// Current usage schema version.
pub const USAGE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Token totals for one model response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default = "usage_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

const fn usage_schema_version() -> SchemaVersion {
    USAGE_SCHEMA_VERSION
}

impl Usage {
    /// Creates token totals.
    #[must_use]
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            schema_version: USAGE_SCHEMA_VERSION,
            input_tokens,
            output_tokens,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            unknown: Unknown::new(),
        }
    }

    /// Sets the number of cached input tokens.
    #[must_use]
    pub const fn with_cached_input_tokens(mut self, tokens: u64) -> Self {
        self.cached_input_tokens = tokens;
        self
    }

    /// Sets the number of input tokens written into the provider's prompt cache.
    #[must_use]
    pub const fn with_cache_write_tokens(mut self, tokens: u64) -> Self {
        self.cache_write_tokens = tokens;
        self
    }

    /// Sets the number of reasoning tokens.
    #[must_use]
    pub const fn with_reasoning_tokens(mut self, tokens: u64) -> Self {
        self.reasoning_tokens = tokens;
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Input tokens.
    #[must_use]
    pub const fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    /// Output tokens.
    #[must_use]
    pub const fn output_tokens(&self) -> u64 {
        self.output_tokens
    }

    /// Sum of input and output tokens.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Input tokens served from cache.
    #[must_use]
    pub const fn cached_input_tokens(&self) -> u64 {
        self.cached_input_tokens
    }

    /// Input tokens written into the cache, which providers commonly price above a plain read.
    ///
    /// It is what separates a cache miss that seeded the cache from one that simply paid full
    /// price, so a hit rate computed without it cannot explain its own denominator.
    #[must_use]
    pub const fn cache_write_tokens(&self) -> u64 {
        self.cache_write_tokens
    }

    /// Reasoning tokens.
    #[must_use]
    pub const fn reasoning_tokens(&self) -> u64 {
        self.reasoning_tokens
    }

    /// Adds `delta` onto these totals, saturating on overflow.
    ///
    /// The known counters are summed. Unknown fields are **retained, not added**: they are opaque
    /// JSON values here, so there is nothing to sum them with, and `delta` wins on a key both
    /// sides carry — the later observation is the more specific one. Retaining them still matters,
    /// because a running total that dropped the counters this build does not recognize would
    /// describe less than the responses it was built from. Do not read a retained unknown counter
    /// as a total across `accumulate` calls; only the last one survives.
    ///
    /// Accumulation lives here rather than in whichever crate happens to keep the ledger so the
    /// unknown-field merge stays a `Usage` concern. Exposing a public setter for `unknown` instead
    /// would let any caller mint fields that never came from a payload, which is precisely what
    /// [`Unknown`]'s crate-internal merge is there to prevent.
    #[must_use]
    pub fn accumulate(&self, delta: &Self) -> Self {
        let mut merged = self.clone();

        merged.input_tokens = self.input_tokens.saturating_add(delta.input_tokens);
        merged.output_tokens = self.output_tokens.saturating_add(delta.output_tokens);
        merged.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(delta.cached_input_tokens);
        merged.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(delta.cache_write_tokens);
        merged.reasoning_tokens = self.reasoning_tokens.saturating_add(delta.reasoning_tokens);
        merged.unknown.extend_from(&delta.unknown);

        merged
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

impl Default for Usage {
    fn default() -> Self {
        Self::new(0, 0)
    }
}
