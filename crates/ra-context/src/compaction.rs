//! Compaction triggers over a provider-neutral measurement of model input, and the projection
//! they lead to.
//!
//! This module decides whether an existing history should be compacted, and builds the compacted
//! model-input view once a summary exists. It does not issue the summary request or replace session
//! history: those operations need a model and a session owner respectively. Keeping both halves
//! pure makes them safe to reuse when a run is resumed, and prevents context policy from changing
//! runtime-owned control-plane state.

use std::collections::{BTreeMap, BTreeSet};

use ra_core::{
    error::{Error, Result},
    item::{CallId, Compaction, ItemId, ModelInputItem, RunItem, RunItemKind},
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
    /// Both sides count the same thing. `max_items` is measured against
    /// [`ContextUsage::item_count`], which counts model-input items, and
    /// [`project_compacted_model_input`] applies retention to the model-visible subsequence of a
    /// history rather than to its raw records. Were retention counted in source records instead,
    /// this comparison would silently mix units and pass a policy that cannot actually converge.
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

/// A trigger and the retention it compacts down to, checked against each other once.
///
/// The two halves are configured separately and neither constructor can see the other, so a host
/// that wired them up independently could run [`CompactionLimits::ensure_converges_with`] — or
/// forget to, and compact on every turn for the rest of the run. Pairing them in the type that
/// [`project_compacted_model_input`] requires makes the check unavoidable on the one path where
/// skipping it is not recoverable: by the time the divergence shows up, the run has already spent
/// a summary request per turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPolicy {
    limits: CompactionLimits,
    retention: AnchorRetention,
}

impl CompactionPolicy {
    /// Pairs a trigger with a retention policy that can actually clear it.
    pub fn new(limits: CompactionLimits, retention: AnchorRetention) -> Result<Self> {
        limits.ensure_converges_with(retention)?;
        Ok(Self { limits, retention })
    }

    /// Configured triggers.
    #[must_use]
    pub const fn limits(self) -> CompactionLimits {
        self.limits
    }

    /// Configured retention.
    #[must_use]
    pub const fn retention(self) -> AnchorRetention {
        self.retention
    }

    /// Assesses this policy's limits against one history measurement.
    #[must_use]
    pub fn assess(self, usage: ContextUsage) -> CompactionAssessment {
        self.limits.assess(usage)
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

/// The model-visible result of replacing part of an authoritative history with one summary.
///
/// This is deliberately an output-only projection. The source [`RunItem`] values remain the
/// session's authoritative history, and the running [`RunState`](ra_core::state::RunState) is not
/// an input to this operation. That boundary keeps compaction from resetting runtime-owned
/// failure streaks, resource admission, budget accounting, pending approvals, or any other
/// control-plane fact while the model sees a smaller history.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactedModelInput {
    items: Vec<ModelInputItem>,
    compacted_item_ids: Vec<ItemId>,
}

impl CompactedModelInput {
    /// Model input with the portable compaction summary in canonical projection order.
    #[must_use]
    pub fn items(&self) -> &[ModelInputItem] {
        &self.items
    }

    /// IDs of model-visible session records represented by the summary, in history order.
    ///
    /// Session-only control records are intentionally absent. They were never model input, so a
    /// summary cannot replace them; their authority remains with the session and run state.
    ///
    /// When a replaced record is itself a [`Compaction`], the IDs that summary already stood for
    /// are inherited here. Naming only the replaced summary item would leave every record behind
    /// it looking uncovered to a consumer that reads [`Compaction::compacted_items`] to decide
    /// what a summary now represents.
    #[must_use]
    pub fn compacted_item_ids(&self) -> &[ItemId] {
        &self.compacted_item_ids
    }
}

/// Builds a compacted model-input projection without changing the authoritative history.
///
/// Retention is applied to the **model-visible subsequence** of `history`, so `anchor_indices` are
/// positions in that projection — the same index space as the `&[ModelInputItem]` slice a host
/// measures with [`ContextUsage::estimate_model_input`] to decide compaction was needed at all.
/// Indexing raw records instead would shift the two spaces apart by every interleaved control
/// record, and an anchor landing on a local approval retains nothing while silently summarizing
/// away the item the host meant to keep. It also keeps a paused run's pending approvals from
/// spending the head and tail budget that was sized in conversation turns.
///
/// The head remains first, then one [`Compaction`] item stands in for every replaced record,
/// followed by the selected anchors and tail. A non-model record such as a local approval is
/// neither emitted nor named by the summary: it belongs to the control plane rather than the
/// conversation the provider receives. A hosted approval request is different: it is model input
/// as well as a pending control record, so it is retained verbatim even when no retention region
/// selected it.
///
/// **The projection is structurally self-sufficient.** Retention selects by position, which on its
/// own can split a call from its result or strand a reasoning item; the compacted set is therefore
/// widened until no such shape survives (see [`close_structural_dependencies`]). A caller may still
/// run [`InputItemNormalizer`](ra_core::item::InputItemNormalizer) afterwards, but does not have to
/// in order to get input a provider adapter will accept.
///
/// **One summary cannot preserve chronology around an anchor.** Replaced records both before and
/// after a retained anchor collapse into a single item placed ahead of it, so content that
/// chronologically followed the anchor is presented as preceding it. Callers that need the
/// ordering to hold should anchor only at the boundary of a replaced region, or compact each
/// region separately.
///
/// The function refuses an operation that would replace no model-visible item. Writing a summary
/// in that case would grow input and could be mistaken for a successful compaction even though no
/// context was removed.
pub fn project_compacted_model_input(
    history: &[RunItem],
    policy: CompactionPolicy,
    anchor_indices: impl IntoIterator<Item = usize>,
    summary: impl Into<String>,
) -> Result<CompactedModelInput> {
    // Checked before anything is selected or measured. A summary the caller cannot have meant is
    // the cheapest rejection available, so it should not wait behind the rest of the work.
    let summary = summary.into();
    if summary.trim().is_empty() {
        return Err(Error::caller(
            "a context compaction summary must not be blank",
        ));
    }

    let model_indices: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, item)| item.is_model_input())
        .map(|(index, _)| index)
        .collect();
    // Retention runs over the positions rather than over the records themselves. Cloning a
    // `RunItem` here would copy raw provider payloads and session data that the projection is
    // about to discard, on the one path that runs precisely because the history is already large.
    let preserved = policy
        .retention()
        .preserve(&model_indices, anchor_indices)?;
    let head: Vec<usize> = preserved
        .head()
        .iter()
        .map(|preserved| *preserved.item())
        .collect();
    let selected_tail: Vec<usize> = preserved
        .anchor()
        .iter()
        .chain(preserved.tail())
        .map(|preserved| *preserved.item())
        .collect();

    let head_indices: BTreeSet<usize> = head.iter().copied().collect();
    // An `McpApprovalRequest` is both model input and an interruption. Unlike a local approval, its
    // protocol item has to keep travelling to the provider; summarizing it away replaces the
    // control request with prose while the server is still waiting for a concrete answer.
    //
    // Only a request the run has not answered yet earns that treatment. `is_interruption` is a
    // property of the item kind rather than of the run, so retaining on that alone would also pin
    // every already-answered request forever — telling the server an approval is still open when it
    // was granted turns ago, and leaving an MCP-heavy history with a floor it can never compact
    // below. An answered request is an ordinary record that travels with its response.
    let answered = answered_approval_requests(history);
    let mut anchored_tail: BTreeSet<usize> = selected_tail.into_iter().collect();
    anchored_tail.extend(
        model_indices
            .iter()
            .filter(|index| {
                !head_indices.contains(index)
                    && history
                        .get(**index)
                        .is_some_and(|item| is_pending_approval_request(item, &answered))
            })
            .copied(),
    );
    let anchored_tail: Vec<usize> = anchored_tail.into_iter().collect();

    let mut retained: BTreeSet<usize> = head.iter().chain(&anchored_tail).copied().collect();
    close_structural_dependencies(history, &model_indices, &mut retained);

    let compacted_item_ids = replaced_item_ids(history, &model_indices, &retained);
    if compacted_item_ids.is_empty() {
        return Err(Error::caller(
            "context compaction would not replace any model-visible history item",
        ));
    }
    let compaction =
        ModelInputItem::Compaction(Compaction::new(summary, compacted_item_ids.clone()));

    let mut items = Vec::with_capacity(retained.len().saturating_add(1));
    items.extend(project_retained(history, &head, &retained));
    items.push(compaction);
    items.extend(project_retained(history, &anchored_tail, &retained));

    Ok(CompactedModelInput {
        items,
        compacted_item_ids,
    })
}

/// Projects the still-retained members of one selected region, in order.
fn project_retained<'a>(
    history: &'a [RunItem],
    region: &'a [usize],
    retained: &'a BTreeSet<usize>,
) -> impl Iterator<Item = ModelInputItem> + 'a {
    region
        .iter()
        .filter(|index| retained.contains(*index))
        .filter_map(|index| history.get(*index).and_then(RunItem::to_model_input))
}

/// Widens the replaced set until what remains is a shape a provider adapter will accept.
///
/// [`AnchorRetention`] selects by position, and a position boundary knows nothing about the
/// structures that span it. Two of those structures make a request unbuildable rather than merely
/// lossy, so the projection resolves them itself instead of leaving them to a downstream
/// normalizer:
///
/// * **A call and its result travel together.** Retaining one without the other yields a
///   `tool_result` with no preceding `tool_use` — which the Anthropic adapter refuses outright, and
///   which the default [`OrphanPolicy::DropCallsWithoutOutputs`](ra_core::item::OrphanPolicy)
///   deliberately will not repair, because an output-only item is legitimate when the call lives
///   behind a server continuation. That exception does not apply to a call this projection just
///   replaced locally. A pair is also broken up when a replaced record falls *between* its members,
///   since the summary would then land between a call and its result.
/// * **A reasoning item belongs to the item it precedes.** Responses-compatible endpoints reject
///   one that is not followed by that item. The normalizer's own guard cannot catch this case: the
///   summary this projection inserts reads as a valid non-reasoning follower, so a reasoning item
///   whose real follower was replaced would survive the check and then be rejected on the wire.
/// * **An answered hosted approval travels with its answer.** A request whose response was replaced
///   reads as still pending, so the server blocks on a decision the run already made. These pair on
///   `request_id` rather than on a call ID, which is why [`RunItem::call_id`] alone does not group
///   them. An *unanswered* request is a one-member group and is never disturbed here.
///
/// Replacing one record can break another structure, so the set is widened to a fixpoint. Each
/// pass that does not settle removes at least one retained record, so the loop is bounded by
/// [`AnchorRetention::max_retained_items`] — the configured retention capacity, not the length of
/// the history.
fn close_structural_dependencies(
    history: &[RunItem],
    model_indices: &[usize],
    retained: &mut BTreeSet<usize>,
) {
    let pairs = structural_pairs(history, model_indices);
    loop {
        let mut newly_replaced: Vec<usize> = Vec::new();

        for members in pairs.values() {
            let (Some(first), Some(last)) = (members.first(), members.last()) else {
                continue;
            };
            let intact = members.iter().all(|index| retained.contains(index));
            let interrupted = model_indices
                .iter()
                .any(|index| index > first && index < last && !retained.contains(index));
            if !intact || interrupted {
                newly_replaced.extend(members.iter().filter(|index| retained.contains(*index)));
            }
        }

        for (position, index) in model_indices.iter().enumerate() {
            if !retained.contains(index) || !is_reasoning(history, *index) {
                continue;
            }
            // Consecutive reasoning items all bind to the same follower, so each one asks about
            // the first non-reasoning record after it rather than about its immediate neighbour.
            let follower = model_indices
                .get(position.saturating_add(1)..)
                .unwrap_or_default()
                .iter()
                .copied()
                .find(|next| !is_reasoning(history, *next));
            if follower.is_none_or(|next| !retained.contains(&next)) {
                newly_replaced.push(*index);
            }
        }

        if newly_replaced.is_empty() {
            return;
        }
        for index in newly_replaced {
            retained.remove(&index);
        }
    }
}

/// What binds two model-input records into one structure that has to travel together.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PairKey<'a> {
    /// A tool or handoff call and its result.
    Call(&'a CallId),
    /// A hosted approval request and the response that answers it.
    McpApproval(&'a str),
}

/// Groups the model-visible members of each paired structure, in history order.
///
/// Tool and handoff pairs share one call-ID space here. A host that reused one ID across both would
/// only ever cause the two to be replaced together, which is the safe direction.
fn structural_pairs<'a>(
    history: &'a [RunItem],
    model_indices: &[usize],
) -> BTreeMap<PairKey<'a>, Vec<usize>> {
    let mut pairs: BTreeMap<PairKey<'a>, Vec<usize>> = BTreeMap::new();
    for index in model_indices {
        let Some(item) = history.get(*index) else {
            continue;
        };
        let key = match item.kind() {
            RunItemKind::McpApprovalRequest(request) => {
                Some(PairKey::McpApproval(request.request_id()))
            }
            RunItemKind::McpApprovalResponse(response) => {
                Some(PairKey::McpApproval(response.request_id()))
            }
            _ => item.call_id().map(PairKey::Call),
        };
        if let Some(key) = key {
            pairs.entry(key).or_default().push(*index);
        }
    }
    pairs
}

/// Request IDs of hosted approvals the run has already answered.
fn answered_approval_requests(history: &[RunItem]) -> BTreeSet<&str> {
    history
        .iter()
        .filter_map(|item| match item.kind() {
            RunItemKind::McpApprovalResponse(response) => Some(response.request_id()),
            _ => None,
        })
        .collect()
}

/// Whether this record is a hosted approval request still waiting on a decision.
fn is_pending_approval_request(item: &RunItem, answered: &BTreeSet<&str>) -> bool {
    matches!(
        item.kind(),
        RunItemKind::McpApprovalRequest(request) if !answered.contains(request.request_id())
    )
}

fn is_reasoning(history: &[RunItem], index: usize) -> bool {
    history
        .get(index)
        .is_some_and(|item| matches!(item.kind(), RunItemKind::Reasoning(_)))
}

/// Collects the IDs one summary stands for, expanding any summary it replaces.
fn replaced_item_ids(
    history: &[RunItem],
    model_indices: &[usize],
    retained: &BTreeSet<usize>,
) -> Vec<ItemId> {
    let mut ids: Vec<ItemId> = Vec::new();
    let mut seen = BTreeSet::new();
    for index in model_indices
        .iter()
        .filter(|index| !retained.contains(*index))
    {
        let Some(item) = history.get(*index) else {
            continue;
        };
        let inherited: &[ItemId] = match item.kind() {
            RunItemKind::Compaction(compaction) => compaction.compacted_items(),
            _ => &[],
        };
        for id in inherited.iter().chain(std::iter::once(item.id())) {
            if seen.insert(id.clone()) {
                ids.push(id.clone());
            }
        }
    }
    ids
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
