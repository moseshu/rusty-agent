//! Token accounting: what one request reported, and what a sequence of them adds up to.
//!
//! # Two types, because they answer different questions
//!
//! [`RequestUsage`] is what a single provider request reported. [`Usage`] is a ledger over a
//! sequence of them: how many requests, their summed counters, and the individual entries it still
//! holds. A run that spends 100K, 150K and 80K input tokens has an aggregate of 330K, and only the
//! entries can say that the middle call was the expensive one — which is the difference between
//! seeing a cache problem and seeing a bill.
//!
//! # Tokens only
//!
//! There is no money here, and that is a decision rather than a gap. No provider reports a charge
//! next to its usage, so any amount this type carried could only come from a built-in price table —
//! per provider, per model, per token class, revised without notice, and wrong silently. Every
//! request keeps its own counters, which is what a host needs to apply its own contracted rates.
//!
//! # Details are subsets, not additions
//!
//! `cached_input_tokens` and `cache_write_tokens` are the part of `input_tokens` that was read from
//! or written to the provider's prompt cache; `reasoning_tokens` is the part of `output_tokens`
//! spent thinking. None of them is separate spend, which is why `total_tokens` is `input + output`
//! and adding a detail would double count.
//!
//! **An adapter is responsible for normalizing a provider that reports otherwise.** Anthropic
//! returns cache-read and cache-creation counts *outside* `input_tokens`; mapping those straight
//! through would leave them out of every total, and the token budget reads exactly that total.

use serde::{Deserialize, Serialize};

use crate::compat::{SchemaVersion, Unknown};

/// Current [`RequestUsage`] schema version.
pub const REQUEST_USAGE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Current [`Usage`] schema version.
pub const USAGE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Token counters reported by **one** provider request.
///
/// This is the raw fact everything else is derived from. A ledger keeps these entries so a host can
/// price each request under its own contract, chart the cache hit rate call by call, and tell a run
/// that made one enormous request from one that made twenty small ones — none of which survives
/// summation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUsage {
    #[serde(default = "request_usage_schema_version")]
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

const fn request_usage_schema_version() -> SchemaVersion {
    REQUEST_USAGE_SCHEMA_VERSION
}

impl RequestUsage {
    /// Creates the counters one request reported.
    #[must_use]
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            schema_version: REQUEST_USAGE_SCHEMA_VERSION,
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

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

impl Default for RequestUsage {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

/// Token accounting over a sequence of requests.
///
/// A model response carries one of these rather than a bare [`RequestUsage`] because a single
/// logical call is not always a single request: a fallback that escalates after a refusal, and any
/// adapter that internally splits one call, both spend more than once for one response.
///
/// # Why the totals are stored and not derived
///
/// [`Self::request_usage_entries`] is the detail a ledger *still holds*, not a guarantee of
/// completeness: entries are dropped when a total is carried across a boundary that must not grow
/// with the run's length — a session checkpoint being the concrete case — and a record written by
/// a build that predates per-request accounting has none to begin with. Deriving the totals from
/// the entries would therefore report less than was actually spent, exactly when the record is
/// oldest or the run is longest. The totals are the number a budget meters against; the entries
/// explain them while they are there.
///
/// This leaves reconciliation to the one place that can perform it: the summed per-request records
/// in a rollout log, which are never pruned, must equal the totals a ledger claims.
///
/// # Minting is not possible
///
/// There is no constructor that takes bare aggregates. A ledger starts empty and grows through
/// [`Self::from_request`] and [`Self::accumulate`], so every token in it arrived attached to a
/// request that reported it, and [`Self::requests`] cannot disagree with what was added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default = "usage_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    requests: u64,
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
    #[serde(default, skip_serializing_if = "is_zero")]
    carried_total_tokens: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    request_usage_entries: Vec<RequestUsage>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

const fn usage_schema_version() -> SchemaVersion {
    USAGE_SCHEMA_VERSION
}

/// The reference is Serde's, not a choice: `skip_serializing_if` always calls with `&T`.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if signature"
)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Usage {
    /// Creates a ledger holding exactly one request.
    ///
    /// Counters this build does not recognize are **lifted onto the ledger as well as kept on the
    /// entry**. A totals-only projection drops the entries, and a provider counter that lived only
    /// there would disappear with them — which is the opposite of what retaining unknown fields is
    /// for. Lifting keeps the newest observation reachable at the level that survives.
    #[must_use]
    pub fn from_request(request: RequestUsage) -> Self {
        let mut unknown = Unknown::new();
        unknown.extend_from(&request.unknown);
        Self {
            schema_version: USAGE_SCHEMA_VERSION,
            requests: 1,
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            cached_input_tokens: request.cached_input_tokens,
            cache_write_tokens: request.cache_write_tokens,
            reasoning_tokens: request.reasoning_tokens,
            carried_total_tokens: 0,
            request_usage_entries: vec![request],
            unknown,
        }
    }

    /// Creates a ledger describing spend inherited from a record that did not itemize it.
    ///
    /// The only caller is the migration that reads a checkpoint written when the budget counted
    /// tokens itself: that record states a total and nothing about how it split, so the total
    /// arrives here rather than being attributed to input or output tokens it was never known to
    /// be. See [`Self::carried_total_tokens`].
    #[must_use]
    pub(crate) fn from_carried_total(total_tokens: u64) -> Self {
        Self {
            carried_total_tokens: total_tokens,
            ..Self::default()
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Number of provider requests these totals cover.
    ///
    /// Counted even when a request reported no counters at all: a compatible endpoint that omits
    /// its usage block still charged for the call, and a ledger that dropped it would report a run
    /// as having made fewer requests than it did.
    #[must_use]
    pub const fn requests(&self) -> u64 {
        self.requests
    }

    /// Per-request detail still retained, oldest first.
    ///
    /// Shorter than [`Self::requests`] once entries have been dropped, and empty on a
    /// totals-only projection. It is never longer.
    #[must_use]
    pub fn request_usage_entries(&self) -> &[RequestUsage] {
        &self.request_usage_entries
    }

    /// Input tokens across every request.
    #[must_use]
    pub const fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    /// Output tokens across every request.
    #[must_use]
    pub const fn output_tokens(&self) -> u64 {
        self.output_tokens
    }

    /// Sum of input and output tokens, plus any spend carried in without a split.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.carried_total_tokens)
    }

    /// Input tokens served from cache.
    #[must_use]
    pub const fn cached_input_tokens(&self) -> u64 {
        self.cached_input_tokens
    }

    /// Input tokens written into the cache, which providers commonly price above a plain read.
    #[must_use]
    pub const fn cache_write_tokens(&self) -> u64 {
        self.cache_write_tokens
    }

    /// Reasoning tokens across every request.
    #[must_use]
    pub const fn reasoning_tokens(&self) -> u64 {
        self.reasoning_tokens
    }

    /// Spend inherited from a record that stated a total without saying how it split.
    ///
    /// It counts in [`Self::total_tokens`], which is what a budget meters, and deliberately not in
    /// [`Self::input_tokens`] or [`Self::output_tokens`]: attributing it to either would put a
    /// number nobody measured into the denominator of every cache-hit rate computed from this
    /// ledger. A run that never resumed from a pre-ledger checkpoint has zero here.
    #[must_use]
    pub const fn carried_total_tokens(&self) -> u64 {
        self.carried_total_tokens
    }

    /// Adds `delta` onto this ledger, saturating on overflow.
    ///
    /// The request count and the known counters are summed, and `delta`'s retained entries are
    /// appended in order. Unknown fields are **retained, not added**: they are opaque JSON values
    /// here, so there is nothing to sum them with, and `delta` wins on a key both sides carry — the
    /// later observation is the more specific one. Retaining them still matters, because a running
    /// total that dropped the counters this build does not recognize would describe less than the
    /// requests it was built from. Do not read a retained unknown counter as a total across
    /// `accumulate` calls; only the last one survives.
    ///
    /// Accumulation lives here rather than in whichever crate happens to keep the ledger so the
    /// unknown-field merge stays a `Usage` concern. Exposing a public setter for `unknown` instead
    /// would let any caller mint fields that never came from a payload, which is precisely what
    /// [`Unknown`]'s crate-internal merge is there to prevent.
    #[must_use]
    pub fn accumulate(&self, delta: &Self) -> Self {
        let mut merged = self.clone();

        merged.requests = self.requests.saturating_add(delta.requests);
        merged.input_tokens = self.input_tokens.saturating_add(delta.input_tokens);
        merged.output_tokens = self.output_tokens.saturating_add(delta.output_tokens);
        merged.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(delta.cached_input_tokens);
        merged.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(delta.cache_write_tokens);
        merged.reasoning_tokens = self.reasoning_tokens.saturating_add(delta.reasoning_tokens);
        merged.carried_total_tokens = self
            .carried_total_tokens
            .saturating_add(delta.carried_total_tokens);
        merged
            .request_usage_entries
            .extend(delta.request_usage_entries.iter().cloned());
        merged.unknown.extend_from(&delta.unknown);

        merged
    }

    /// The same totals with the per-request entries dropped.
    ///
    /// For a record whose size must not grow with the number of requests it summarizes. A session
    /// checkpoint is written repeatedly and each one restates the totals so far; carrying every
    /// entry into every checkpoint would make the log grow with the square of the session. The
    /// detail is not lost — it stays in the per-request records the checkpoint summarizes.
    #[must_use]
    pub fn without_entries(&self) -> Self {
        let mut projected = self.clone();
        projected.request_usage_entries.clear();
        projected
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            schema_version: USAGE_SCHEMA_VERSION,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            carried_total_tokens: 0,
            request_usage_entries: Vec::new(),
            unknown: Unknown::new(),
        }
    }
}

impl From<RequestUsage> for Usage {
    fn from(request: RequestUsage) -> Self {
        Self::from_request(request)
    }
}
