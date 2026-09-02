//! The one character basis every provider-neutral context estimate in this crate prices with.
//!
//! Two callers share it and neither owns it. [`compaction`](crate::compaction) reads the number as
//! a trigger against a configured limit, and [`usage`](crate::usage) reads it as a diagnostic a
//! host shows a user. Housing the primitive with either would make a change requested by one of
//! them silently move the other's number, so it lives here with both consumers named.
//!
//! Provider adapters should still prefer their tokenizer when it is available: multimodal pricing
//! and request framing are provider-specific and this estimator prices neither.

use ra_core::{
    error::{Error, Result},
    item::ModelInputItem,
    prompt::CHARS_PER_TOKEN,
};
use serde_json::Value;

/// Estimates what one model-input item costs.
///
/// Every item is walked, so opaque input has a finite local cost rather than disappearing from the
/// estimate: a base64 body is a string value like any other.
///
/// **Content is priced, wire framing is not.** Field names, delimiters, and JSON escapes are
/// excluded, and a character is charged once in the form the model reads rather than twice in its
/// escaped form. Measuring the serialized text instead inflates a JSON-shaped tool result by
/// roughly a fifth, which has two consequences worth avoiding: the proportional trigger
/// [`CompactionLimits::for_model`](crate::compaction::CompactionLimits::for_model) derives from a
/// real model window would fire near 50% of that window rather than the configured 60%, and an
/// excerpt [`ToolResultBudget`](crate::budget::ToolResultBudget) had just trimmed to its
/// per-result ceiling would be priced above that same ceiling here — a
/// [`CompactionReason::SingleItemTokens`](crate::compaction::CompactionReason::SingleItemTokens)
/// no amount of compaction can clear. `crate::budget` rejected the serialized-JSON basis for the
/// same reason.
///
/// This reasoning covers replay items, where the model-visible content sits in the JSON *values*
/// and the field names are the envelope. It does not carry over to a JSON Schema, which puts its
/// payload in its keys; a definition is priced on its rendered text by
/// [`ModelToolDefinition::advertised_chars`](ra_core::model::ModelToolDefinition::advertised_chars)
/// instead.
///
/// # Errors
///
/// Returns an error when the item cannot be rendered as JSON.
pub(crate) fn item_tokens(item: &ModelInputItem) -> Result<usize> {
    let rendered = serde_json::to_value(item).map_err(|error| {
        Error::caller(format!(
            "failed to render a model-input item for a context estimate: {error}"
        ))
    })?;
    Ok(content_chars(&rendered).div_ceil(CHARS_PER_TOKEN))
}

/// Converts a character count accumulated by a caller into the same token estimate.
///
/// It exists so a caller that already knows the characters a model reads — a rendered definition,
/// say — does not have to carry its own copy of the rounding rule.
pub(crate) const fn chars_to_tokens(chars: usize) -> usize {
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Counts the characters a model reads from one serialized item.
///
/// A walk over the value rather than the length of its serialized text. `ModelInputItem` is
/// `#[non_exhaustive]` and several of its variants carry provider-opaque maps, so a `match` on the
/// item would need a wildcard that prices future shapes at zero, while the serialized text charges
/// for field names, delimiters, and one extra character per escape.
fn content_chars(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Bool(true) => "true".len(),
        Value::Bool(false) => "false".len(),
        Value::Number(number) => number.to_string().chars().count(),
        Value::String(text) => text.chars().count(),
        Value::Array(values) => values
            .iter()
            .fold(0, |total, value| total.saturating_add(content_chars(value))),
        Value::Object(fields) => fields
            .values()
            .fold(0, |total, value| total.saturating_add(content_chars(value))),
    }
}
