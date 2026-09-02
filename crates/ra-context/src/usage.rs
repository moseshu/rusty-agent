//! A category-level estimate of the context a model request occupies.
//!
//! A provider's response usage is a billing observation about a request that already happened.
//! This module instead measures the request that is about to be sent, so a host can explain the
//! current context window to a user and diagnose which model-visible material occupies it. The
//! two must not be conflated: provider framing, tokenizers, server-managed history, and cache
//! accounting are all provider-specific facts.
//!
//! # Relationship to [`ContextUsage`]
//!
//! [`ContextUsage`] measures the *history* — the items compaction can shrink. This module
//! measures the *request*, which additionally carries system instructions and the tool, handoff,
//! and output-schema definitions that no amount of compaction removes. The two totals therefore
//! differ, often by tens of thousands of tokens on a large tool surface, and reading one as the
//! other is how a host ends up reporting a window as nearly full while the compaction trigger it
//! configured has not fired.
//!
//! They are not computed twice. [`ContextUsageBreakdown::estimate_model_request`] walks the items
//! once and reports the history measurement it derived on the way through
//! [`ContextUsageBreakdown::model_input`], so a host that needs both pays for one walk and cannot
//! be handed two numbers that disagree.

use ra_core::{
    error::{Error, Result},
    item::ModelInputItem,
    model::{ModelHandoffDefinition, ModelOutputSchema, ModelRequest, ModelToolDefinition},
    prompt::estimate_tokens,
};

use crate::{
    compaction::ContextUsage,
    estimate::{chars_to_tokens, item_tokens},
    window::ContextWindowConfig,
};

/// A model-visible category in a context-usage breakdown.
///
/// The categories deliberately describe the semantic source of the context instead of provider
/// wire envelopes. A provider may expose a more precise tokenizer, but its adapter must not make
/// its own wire-format categories part of this provider-neutral API.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ContextUsageCategory {
    /// Stable system instructions.
    System,
    /// Everything that advertises a callable action: tool, handoff, and structured-output
    /// definitions, plus any MCP tool catalog replayed in the history.
    Tools,
    /// Ordinary replay items — messages, calls, compactions, and hosted approvals.
    Messages,
    /// Tool-call output observations.
    ToolResults,
    /// Reasoning replay material.
    Reasoning,
}

impl ContextUsageCategory {
    /// Number of categories, and the width of a breakdown's storage.
    pub const COUNT: usize = 5;

    /// Every category in the stable presentation order used by a UI.
    ///
    /// The `index` match below is what keeps this array honest: it is exhaustive, so a new variant
    /// cannot compile until it is given a position, and the const assertion under this `impl`
    /// rejects an array that does not hold every position exactly once. Growing the enum therefore
    /// means growing `COUNT` and this array together — the assertion fails otherwise.
    pub const ALL: [Self; Self::COUNT] = [
        Self::System,
        Self::Tools,
        Self::Messages,
        Self::ToolResults,
        Self::Reasoning,
    ];

    /// Stable machine-readable category name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Tools => "tools",
            Self::Messages => "messages",
            Self::ToolResults => "tool_results",
            Self::Reasoning => "reasoning",
        }
    }

    /// Position of this category in [`Self::ALL`] and in a breakdown's storage.
    const fn index(self) -> usize {
        match self {
            Self::System => 0,
            Self::Tools => 1,
            Self::Messages => 2,
            Self::ToolResults => 3,
            Self::Reasoning => 4,
        }
    }
}

const _: () = {
    let mut position = 0;
    while position < ContextUsageCategory::COUNT {
        assert!(
            ContextUsageCategory::ALL[position].index() == position,
            "every category must appear in ALL exactly once, at the position `index` names"
        );
        position += 1;
    }
};

/// A provider-neutral estimate of the current model request, split by source category.
///
/// Replay items use the same content-only estimator as compaction: string values are charged as
/// the model reads them, object field names and JSON framing are excluded, and opaque JSON values
/// remain visible. Definitions are priced differently and deliberately — see
/// [`Self::estimate_model_request`]. The sum gives a coherent local estimate, not a provider
/// billing or tokenizer result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsageBreakdown {
    /// Indexed by `ContextUsageCategory::index`, so the categories a UI iterates over and the
    /// numbers that make up the total are the same list rather than two lists kept in step.
    tokens: [usize; ContextUsageCategory::COUNT],
    /// Validated once at construction, which is what lets [`Self::total_tokens`] add without
    /// checking. Removing it reintroduces an overflow panic in a reporting path.
    total_tokens: usize,
    model_input: ContextUsage,
}

impl ContextUsageBreakdown {
    /// Estimates the model-visible context in `request`.
    ///
    /// Handoffs and a structured-output schema join the tools category because they are all
    /// request-time model definitions, as does a replayed MCP tool catalog: it advertises the same
    /// name, description, and input schema, so filing it under messages would send a host trimming
    /// its conversation when the fix is to trim its tool surface. Tool calls themselves remain
    /// messages, while only their observations enter `tool_results`; that distinction makes an
    /// oversized tool result visible without hiding the assistant decision that produced it.
    ///
    /// **Definitions are priced on their rendered text, replay items on their values.** A JSON
    /// Schema carries its meaning in its keys — `properties`, and every parameter name — so the
    /// content-only walk that suits a replay item would charge a tool's parameter names nothing
    /// and undercount a real tool surface several-fold. Definitions are therefore measured with
    /// [`ModelToolDefinition::advertised_chars`], the same rendering the tool-surface byte budget
    /// counts, and the whole definition table is rounded to tokens once rather than once per part.
    ///
    /// # Errors
    ///
    /// Returns an error when an input item cannot be rendered as JSON, when a definition's schema
    /// cannot be rendered, or when the estimated total exceeds `usize`.
    pub fn estimate_model_request(request: &ModelRequest) -> Result<Self> {
        let system_tokens = request.system_instructions().map_or(0, estimate_tokens);
        let definition_chars = request
            .tools()
            .iter()
            .map(ModelToolDefinition::advertised_chars)
            .chain(
                request
                    .handoffs()
                    .iter()
                    .map(ModelHandoffDefinition::advertised_chars),
            )
            .chain(
                request
                    .output_schema()
                    .map(ModelOutputSchema::advertised_chars),
            )
            .try_fold(0_usize, |total, chars| checked_add(total, chars?))?;

        let mut tokens = [0_usize; ContextUsageCategory::COUNT];
        tokens[ContextUsageCategory::System.index()] = system_tokens;
        tokens[ContextUsageCategory::Tools.index()] = chars_to_tokens(definition_chars);

        let mut item_count = 0_usize;
        let mut largest_item_tokens = 0_usize;
        let mut input_tokens = 0_usize;
        for item in request.input() {
            let item_cost = item_tokens(item)?;
            let slot = category_of(item).index();
            tokens[slot] = checked_add(tokens[slot], item_cost)?;
            item_count += 1;
            largest_item_tokens = largest_item_tokens.max(item_cost);
            input_tokens = checked_add(input_tokens, item_cost)?;
        }

        Ok(Self {
            tokens,
            total_tokens: tokens.into_iter().try_fold(0_usize, checked_add)?,
            model_input: ContextUsage::new(item_count, largest_item_tokens, input_tokens)?,
        })
    }

    /// Estimated tokens in `category`.
    #[must_use]
    pub const fn tokens(self, category: ContextUsageCategory) -> usize {
        self.tokens[category.index()]
    }

    /// Estimated tokens in stable system instructions.
    #[must_use]
    pub const fn system_tokens(self) -> usize {
        self.tokens(ContextUsageCategory::System)
    }

    /// Estimated tokens in advertised actions and request-time model definitions.
    #[must_use]
    pub const fn tool_tokens(self) -> usize {
        self.tokens(ContextUsageCategory::Tools)
    }

    /// Estimated tokens in ordinary replay items.
    #[must_use]
    pub const fn message_tokens(self) -> usize {
        self.tokens(ContextUsageCategory::Messages)
    }

    /// Estimated tokens in tool-call output observations.
    #[must_use]
    pub const fn tool_result_tokens(self) -> usize {
        self.tokens(ContextUsageCategory::ToolResults)
    }

    /// Estimated tokens in reasoning replay items.
    #[must_use]
    pub const fn reasoning_tokens(self) -> usize {
        self.tokens(ContextUsageCategory::Reasoning)
    }

    /// Total estimated context tokens across every category.
    #[must_use]
    pub const fn total_tokens(self) -> usize {
        self.total_tokens
    }

    /// The history measurement this estimate derived on its way through the request.
    ///
    /// This is exactly what [`ContextUsage::estimate_model_input`] would return for the same
    /// items, so a host can drive its compaction triggers from it without walking the history a
    /// second time. It excludes system instructions and definitions, which is why its total is
    /// below [`Self::total_tokens`].
    #[must_use]
    pub const fn model_input(self) -> ContextUsage {
        self.model_input
    }

    /// Resolves this estimate against a configured model context window.
    ///
    /// `None` means the model has no configured window. No capacity is invented from a model name.
    #[must_use]
    pub fn for_model(
        self,
        context_windows: &ContextWindowConfig,
        model: &str,
    ) -> Option<ContextWindowUsage> {
        context_windows
            .context_window(model)
            .map(|context_window| ContextWindowUsage {
                breakdown: self,
                context_window,
            })
    }
}

/// A context-usage breakdown paired with a known model context-window capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextWindowUsage {
    breakdown: ContextUsageBreakdown,
    context_window: u64,
}

impl ContextWindowUsage {
    /// Category-level estimate.
    #[must_use]
    pub const fn breakdown(self) -> ContextUsageBreakdown {
        self.breakdown
    }

    /// Configured raw context-window capacity.
    #[must_use]
    pub const fn context_window(self) -> u64 {
        self.context_window
    }

    /// Total estimated context tokens.
    #[must_use]
    pub const fn total_tokens(self) -> usize {
        self.breakdown.total_tokens()
    }

    /// Estimated window occupancy in basis points.
    ///
    /// This may exceed 10,000 when the request is already larger than the configured window. The
    /// whole and fractional parts are taken separately so a total near `u64::MAX` cannot overflow
    /// the scaling multiplication, which the naive `total * 10_000 / window` form does well before
    /// a request is implausible.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the remainder is smaller than the context window, which `ContextWindowTable` refuses to construct as zero, so this component is below 10,000"
    )]
    pub fn occupancy_basis_points(self) -> u64 {
        let total = self.total_tokens() as u64;
        let whole = total / self.context_window;
        let remainder = total % self.context_window;
        let fractional =
            ((u128::from(remainder) * 10_000) / u128::from(self.context_window)) as u64;
        whole.saturating_mul(10_000).saturating_add(fractional)
    }
}

/// Decides which category one replay item belongs to.
///
/// Every known variant is named rather than folded into the wildcard, so adding a model-input
/// shape is a decision made here instead of an unnoticed arrival in `messages`. The wildcard
/// remains because `ModelInputItem` is `#[non_exhaustive]`: a variant this crate has never seen is
/// ordinary replay material until someone says otherwise.
#[allow(
    clippy::match_same_arms,
    reason = "the named arms and the wildcard agree today, but folding them together is exactly what would let the next variant reach `messages` with nobody deciding that it should"
)]
fn category_of(item: &ModelInputItem) -> ContextUsageCategory {
    match item {
        ModelInputItem::Reasoning(_) => ContextUsageCategory::Reasoning,
        ModelInputItem::ToolCallOutput(_) => ContextUsageCategory::ToolResults,
        ModelInputItem::McpListTools(_) => ContextUsageCategory::Tools,
        ModelInputItem::Message(_)
        | ModelInputItem::ToolCall(_)
        | ModelInputItem::HandoffCall(_)
        | ModelInputItem::HandoffOutput(_)
        | ModelInputItem::McpApprovalRequest(_)
        | ModelInputItem::McpApprovalResponse(_)
        | ModelInputItem::Compaction(_) => ContextUsageCategory::Messages,
        _ => ContextUsageCategory::Messages,
    }
}

fn checked_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| Error::caller("the estimated context token total exceeds usize"))
}
