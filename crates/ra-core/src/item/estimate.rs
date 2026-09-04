//! The one character basis every provider-neutral estimate over model input prices with.
//!
//! Three callers share it and none of them owns it: `ra-context` reads it as a compaction trigger
//! against a configured limit and as a diagnostic a host shows a user, and
//! [`ContextFilterReport`](crate::filter::ContextFilterReport) reads it to say how much one filter
//! removed from a request. Housing the primitive with any one of them would make a change requested
//! by that caller silently move the others' numbers, and the filter report exists precisely so a
//! saving can be compared against the limit that motivated it.
//!
//! Provider adapters should still prefer their tokenizer when it is available: multimodal pricing
//! and request framing are provider-specific and this estimator prices neither.

use serde_json::Value;

use crate::{
    error::{Error, Result},
    item::ModelInputItem,
    prompt::CHARS_PER_TOKEN,
};

/// Counts the characters a model reads from one input item.
///
/// Every item is walked, so opaque input has a finite local cost rather than disappearing from the
/// estimate: a base64 body is a string value like any other.
///
/// **Content is priced, wire framing is not.** Field names, delimiters, and JSON escapes are
/// excluded, and a character is charged once in the form the model reads rather than twice in its
/// escaped form. Measuring the serialized text instead inflates a JSON-shaped tool result by
/// roughly a fifth, which has two consequences worth avoiding: a proportional compaction trigger
/// derived from a real model window would fire near 50% of that window rather than the configured
/// 60%, and an excerpt a per-result budget had just trimmed to its ceiling would be priced above
/// that same ceiling here — a single-item overflow no amount of compaction can clear.
///
/// This reasoning covers replay items, where the model-visible content sits in the JSON *values*
/// and the field names are the envelope. It does not carry over to a JSON Schema, which puts its
/// payload in its keys; a definition is priced on its rendered text by
/// [`ModelToolDefinition::advertised_chars`](crate::model::ModelToolDefinition::advertised_chars)
/// instead.
///
/// # Errors
///
/// Returns an error when the item cannot be rendered as JSON.
pub fn item_chars(item: &ModelInputItem) -> Result<usize> {
    let rendered = serde_json::to_value(item).map_err(|error| {
        Error::caller(format!(
            "failed to render a model-input item for a context estimate: {error}"
        ))
    })?;
    Ok(content_chars(&rendered))
}

/// Estimates what one model-input item costs in tokens.
///
/// Rounding is per item rather than over a summed total, because that is how a caller that
/// accumulates a running estimate item by item necessarily prices it. Summing characters first and
/// rounding once would produce a second, slightly smaller number for the same input.
///
/// # Errors
///
/// Returns an error when the item cannot be rendered as JSON.
pub fn item_tokens(item: &ModelInputItem) -> Result<usize> {
    Ok(chars_to_tokens(item_chars(item)?))
}

/// Converts a character count accumulated by a caller into the same token estimate.
///
/// It exists so a caller that already knows the characters a model reads — a rendered definition,
/// say — does not have to carry its own copy of the rounding rule.
#[must_use]
pub const fn chars_to_tokens(chars: usize) -> usize {
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Counts the characters a model reads from one serialized item.
///
/// A walk over the value rather than the length of its serialized text. [`ModelInputItem`] is
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
