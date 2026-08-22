//! Cache hit rate calculations and evaluation metrics.

use ra_core::usage::{RequestUsage, Usage};

/// Calculates the cache hit rate ratio from cached and total input tokens.
///
/// `cached_tokens` is expected to be the part of `total_input_tokens` served from cache, which is
/// the normalization [`Usage`] already guarantees — providers that report cache reads outside the
/// input count are converted before they reach here.
///
/// Returns a value between `0.0` and `1.0`. If total input tokens is zero, returns `0.0`.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "token counts stay far below f64's 2^53 exact-integer range"
)]
pub fn calculate_cache_hit_rate(cached_tokens: u64, total_input_tokens: u64) -> f64 {
    if total_input_tokens == 0 {
        return 0.0;
    }
    let cached = cached_tokens as f64;
    let total = total_input_tokens as f64;
    (cached / total).clamp(0.0, 1.0)
}

/// Evaluates cache hit rate directly from a [`Usage`] ledger.
#[must_use]
pub fn calculate_cache_hit_rate_from_usage(usage: &Usage) -> f64 {
    calculate_cache_hit_rate(usage.cached_input_tokens(), usage.input_tokens())
}

/// Evaluates cache hit rate for one request.
///
/// A ledger-wide rate answers whether a run was cheap; this one answers *which call* broke the
/// prefix. Averaged across a run, one cold first call and a stable prefix afterwards look much like
/// a prefix that is being invalidated every turn — and those are opposite problems.
#[must_use]
pub fn calculate_cache_hit_rate_from_request(request: &RequestUsage) -> f64 {
    calculate_cache_hit_rate(request.cached_input_tokens(), request.input_tokens())
}

/// Evaluates whether the cache hit rate meets or exceeds the target threshold (default 70%).
#[must_use]
pub fn meets_cache_hit_target(usage: &Usage, target_threshold: f64) -> bool {
    calculate_cache_hit_rate_from_usage(usage) >= target_threshold
}
