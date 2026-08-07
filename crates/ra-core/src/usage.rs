//! Token totals for a single model response.
//!
//! R1-1 only requires `ModelResponse` to carry stable totals. R1-8 will add per-request
//! `RequestUsage`, cache-write tokens, and cost aggregation. This type does not claim that later
//! work is complete.

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

    /// Reasoning tokens.
    #[must_use]
    pub const fn reasoning_tokens(&self) -> u64 {
        self.reasoning_tokens
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
