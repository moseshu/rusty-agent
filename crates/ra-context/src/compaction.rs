//! Compaction triggers over a provider-neutral measurement of model input.
//!
//! This module deliberately decides only whether an existing history should be compacted. It does
//! not issue the summary request or replace session history: those operations need a model and a
//! session owner respectively. Keeping the decision pure makes it safe to reuse when a run is
//! resumed, and prevents context policy from changing runtime-owned control-plane state.

use ra_core::{
    error::{Error, Result},
    item::ModelInputItem,
    prompt::CHARS_PER_TOKEN,
};
use serde_json::Value;

use crate::{compaction::anchor::AnchorRetention, window::ContextWindowConfig};

/// One reason that the current model-input history needs compaction.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CompactionReason {
    /// The number of model-input items reached its configured limit.
    ItemCount,
    /// One model-input item reached its configured token limit.
    ///
    /// Compaction clears this only when the oversized item is one it drops. An item held by
    /// [`AnchorRetention`]'s head, anchor, or tail survives unchanged, so a history whose retained
    /// region contains the offender stays over the limit however often it is compacted. Shrinking
    /// that one item is the job of an item-level projector such as
    /// [`ToolResultBudget`](crate::budget::ToolResultBudget).
    SingleItemTokens,
    /// The complete model-input history reached its configured token limit.
    TotalTokens,
}

impl CompactionReason {
    /// Stable machine-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ItemCount => "item_count",
            Self::SingleItemTokens => "single_item_tokens",
            Self::TotalTokens => "total_tokens",
        }
    }
}

/// Measured size of one model-visible history.
///
/// A provider with exact token accounting can construct this directly. The approximate
/// [`Self::estimate_model_input`] helper exists for provider-neutral local decisions, not as a
/// replacement for a provider tokenizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    item_count: usize,
    largest_item_tokens: usize,
    total_tokens: usize,
}

impl ContextUsage {
    /// Creates a coherent measurement supplied by a provider or host tokenizer.
    ///
    /// Both bounds a measurement has to satisfy are checked here rather than trusted: no single
    /// item costs more than the whole history, and the items cannot add up to less than the total
    /// they are said to sum to. A provider adapter reading `largest_item_tokens` from a response
    /// field its API stopped returning would otherwise report a coherent-looking zero, and
    /// [`CompactionLimits::max_single_item_tokens`] would silently never fire for that provider.
    pub fn new(item_count: usize, largest_item_tokens: usize, total_tokens: usize) -> Result<Self> {
        if largest_item_tokens > total_tokens {
            return Err(Error::caller(format!(
                "a context measurement cannot report a largest item of {largest_item_tokens} tokens above its {total_tokens}-token total"
            )));
        }
        // An overflowing product is a ceiling no total can exceed, so it cannot be a violation.
        if item_count
            .checked_mul(largest_item_tokens)
            .is_some_and(|ceiling| ceiling < total_tokens)
        {
            return Err(Error::caller(format!(
                "{item_count} items of at most {largest_item_tokens} tokens each cannot add up to a {total_tokens}-token total"
            )));
        }
        Ok(Self {
            item_count,
            largest_item_tokens,
            total_tokens,
        })
    }

    /// Estimates usage from the provider-neutral serialized item representation.
    ///
    /// Every item is walked, so opaque input has a finite local cost rather than disappearing from
    /// the trigger calculation: a base64 body is a string value like any other.
    ///
    /// **Content is priced, wire framing is not.** Field names, delimiters, and JSON escapes are
    /// excluded, and a character is charged once in the form the model reads rather than twice in
    /// its escaped form. Measuring the serialized text instead inflates a JSON-shaped tool result
    /// by roughly a fifth, which has two consequences worth avoiding: the proportional trigger
    /// [`CompactionLimits::for_model`] derives from a real model window would fire near 50% of that
    /// window rather than the configured 60%, and an excerpt
    /// [`ToolResultBudget`](crate::budget::ToolResultBudget) had just trimmed to its per-result
    /// ceiling would be priced above that same ceiling here — a
    /// [`CompactionReason::SingleItemTokens`] no amount of compaction can clear. `crate::budget`
    /// rejected the serialized-JSON basis for the same reason.
    ///
    /// Provider adapters should still prefer their tokenizer when it is available: multimodal
    /// pricing and request framing are provider-specific and this estimator prices neither.
    pub fn estimate_model_input(items: &[ModelInputItem]) -> Result<Self> {
        let mut largest_item_tokens = 0_usize;
        let mut total_tokens = 0_usize;

        for item in items {
            let rendered = serde_json::to_value(item).map_err(|error| {
                Error::caller(format!(
                    "failed to render a model-input item for compaction: {error}"
                ))
            })?;
            let item_tokens = content_chars(&rendered).div_ceil(CHARS_PER_TOKEN);
            largest_item_tokens = largest_item_tokens.max(item_tokens);
            total_tokens = total_tokens
                .checked_add(item_tokens)
                .ok_or_else(|| Error::caller("the estimated context token total exceeds usize"))?;
        }

        Self::new(items.len(), largest_item_tokens, total_tokens)
    }

    /// Number of model-input items in the history.
    #[must_use]
    pub const fn item_count(self) -> usize {
        self.item_count
    }

    /// Largest estimated or provider-reported item cost.
    #[must_use]
    pub const fn largest_item_tokens(self) -> usize {
        self.largest_item_tokens
    }

    /// Total estimated or provider-reported context cost.
    #[must_use]
    pub const fn total_tokens(self) -> usize {
        self.total_tokens
    }
}

/// Optional limits that independently trigger context compaction.
///
/// At least one limit is required. A host that only knows a model-window threshold can supply
/// `None` for the item limits and set only `max_total_tokens`.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionLimits {
    max_items: Option<usize>,
    max_single_item_tokens: Option<usize>,
    max_total_tokens: Option<usize>,
}

impl CompactionLimits {
    /// Creates validated trigger limits.
    pub fn new(
        max_items: Option<usize>,
        max_single_item_tokens: Option<usize>,
        max_total_tokens: Option<usize>,
    ) -> Result<Self> {
        for (name, limit) in [
            ("item", max_items),
            ("single-item token", max_single_item_tokens),
            ("total-token", max_total_tokens),
        ] {
            if limit == Some(0) {
                return Err(Error::config(format!(
                    "a compaction {name} limit must be at least one"
                )));
            }
        }
        if max_items.is_none() && max_single_item_tokens.is_none() && max_total_tokens.is_none() {
            return Err(Error::config(
                "compaction limits require at least one configured trigger",
            ));
        }
        // One item never costs more than the whole history, so a single-item limit at or above the
        // total limit is a trigger that can only ever fire alongside the one it was meant to
        // anticipate. Refused rather than accepted as a guard that does nothing.
        if let (Some(single), Some(total)) = (max_single_item_tokens, max_total_tokens)
            && single >= total
        {
            return Err(Error::config(format!(
                "a compaction single-item limit of {single} tokens never fires before the {total}-token total limit and would guard nothing"
            )));
        }
        Ok(Self {
            max_items,
            max_single_item_tokens,
            max_total_tokens,
        })
    }

    /// Resolves a total-token trigger from a model's configured context window.
    ///
    /// An unknown model contributes no invented total-token capacity. Explicit item limits still
    /// form a usable policy in that case; only a call with no known window and no explicit limit
    /// returns `Ok(None)`. The host can add a window override before calling this method. The
    /// optional item limits remain useful alongside the proportional total threshold because one
    /// pathological item should not wait for the entire history to become large.
    pub fn for_model(
        context_windows: &ContextWindowConfig,
        model: &str,
        max_items: Option<usize>,
        max_single_item_tokens: Option<usize>,
    ) -> Result<Option<Self>> {
        let Some(total_limit) = context_windows.compaction_threshold(model) else {
            if max_items.is_none() && max_single_item_tokens.is_none() {
                return Ok(None);
            }
            return Self::new(max_items, max_single_item_tokens, None).map(Some);
        };
        // Reported against the ratio and the model that produced it. Falling through to
        // `Self::new`'s generic "must be at least one" would name neither, and the host would be
        // looking for a zero it never wrote: the ratio is non-zero and the window is a whole
        // number of tokens, but their integer product rounds down to nothing.
        if total_limit == 0 {
            return Err(Error::config(format!(
                "a compaction threshold of {} rounds model `{model}`'s context window down to zero tokens, which would require compaction before every request",
                context_windows.compaction_threshold_ratio()
            )));
        }
        let total_limit = usize::try_from(total_limit).map_err(|_| {
            Error::config(format!(
                "the compaction threshold for model `{model}` does not fit this platform's usize"
            ))
        })?;
        Self::new(max_items, max_single_item_tokens, Some(total_limit)).map(Some)
    }

    /// Refuses a retention policy that cannot bring a history back under the item-count trigger.
    ///
    /// Compaction replaces the dropped middle with one summary item, so a compacted history is
    /// that summary plus everything [`AnchorRetention`] keeps. When the two together still reach
    /// `max_items`, [`Self::assess`] reports [`CompactionReason::ItemCount`] again on the very next
    /// turn, and a host that compacts whenever an assessment requires it issues a summary request
    /// every turn for the rest of the run. The two policies are configured separately and neither
    /// constructor can see the other, so the check belongs on the pair.
    ///
    /// [`CompactionReason::SingleItemTokens`] has no equivalent static answer — whether it clears
    /// depends on where the oversized item sits, which is a property of the history rather than of
    /// the policy. That variant documents what does clear it.
    pub fn ensure_converges_with(self, retention: AnchorRetention) -> Result<()> {
        let Some(max_items) = self.max_items else {
            return Ok(());
        };
        let retained = retention.max_retained_items();
        if retained.saturating_add(1) >= max_items {
            return Err(Error::config(format!(
                "a retention policy keeping up to {retained} items plus one summary item cannot bring a history below the {max_items}-item compaction trigger"
            )));
        }
        Ok(())
    }

    /// Maximum number of input items, if this trigger is enabled.
    #[must_use]
    pub const fn max_items(self) -> Option<usize> {
        self.max_items
    }

    /// Maximum cost of one input item, if this trigger is enabled.
    #[must_use]
    pub const fn max_single_item_tokens(self) -> Option<usize> {
        self.max_single_item_tokens
    }

    /// Maximum total history cost, if this trigger is enabled.
    #[must_use]
    pub const fn max_total_tokens(self) -> Option<usize> {
        self.max_total_tokens
    }

    /// Assesses all enabled limits, retaining every reason that fired.
    #[must_use]
    pub fn assess(self, usage: ContextUsage) -> CompactionAssessment {
        // Deliberately not `with_capacity`: an assessment runs every turn and returns nothing on
        // almost all of them, and `CompactionAssessment` is `Clone`, so a pre-sized empty vector
        // would be allocated and then copied along the path that has no reasons to report.
        let mut reasons = Vec::new();
        if self
            .max_items
            .is_some_and(|limit| usage.item_count >= limit)
        {
            reasons.push(CompactionReason::ItemCount);
        }
        if self
            .max_single_item_tokens
            .is_some_and(|limit| usage.largest_item_tokens >= limit)
        {
            reasons.push(CompactionReason::SingleItemTokens);
        }
        if self
            .max_total_tokens
            .is_some_and(|limit| usage.total_tokens >= limit)
        {
            reasons.push(CompactionReason::TotalTokens);
        }
        CompactionAssessment { usage, reasons }
    }
}

/// The deterministic result of applying [`CompactionLimits`] to one history measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionAssessment {
    usage: ContextUsage,
    reasons: Vec<CompactionReason>,
}

impl CompactionAssessment {
    /// Usage against which the assessment was made.
    #[must_use]
    pub const fn usage(&self) -> ContextUsage {
        self.usage
    }

    /// Every limit that was reached, in stable item/single-item/total order.
    #[must_use]
    pub fn reasons(&self) -> &[CompactionReason] {
        &self.reasons
    }

    /// Whether any enabled trigger requires compaction.
    #[must_use]
    pub fn is_required(&self) -> bool {
        !self.reasons.is_empty()
    }
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

pub mod anchor;
pub mod summary;
