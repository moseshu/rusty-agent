//! The pure projections applied to one model request just before it is sent.
//!
//! A [`ContextFilter`] rewrites the input a model is about to receive and nothing else. Tool-output
//! trimming, a recent-window policy, and redaction are all this shape: they decide what the model
//! *sees*, while the session keeps the complete records for resume, storage, and replay. That
//! separation is the whole contract — a filter that could write back to the session would make the
//! authoritative history a function of a display policy.
//!
//! # Why this is not [`ContextProcessor`](crate::capability::ContextProcessor)
//!
//! The two look adjacent and are deliberately different contracts. A processor is asynchronous, may
//! spend money asking the runtime for a model-produced summary, and emits authoritative records; it
//! replaces whole regions of history with something that was not there before. A filter is
//! synchronous, spends nothing, emits nothing, and may only project items that already exist.
//!
//! That difference decides their order, too: **a processor runs first and a filter runs on what it
//! produced.** A processor rebuilds the history region out of authoritative records, so anything a
//! filter had trimmed there would come back at full size. Running the cheap, narrow pass last is
//! also what keeps it from paying to trim items a summary is about to replace outright.
//!
//! # What a filter may change
//!
//! [`ModelInputData`] carries the request's input *and* its system instructions, and **both can be
//! replaced**, matching the upstream contract this type is named after.
//!
//! An earlier version refused a filter that returned different instructions, on the grounds that
//! the instructions are the cached prefix and rewriting them per turn moves the span a provider's
//! prompt cache keys on. That cost is real, and it is still real — but it is the host's to weigh,
//! not this chain's to forbid. A host that redacts credentials out of its own prefix, or that
//! trims a prompt that turned out too long for a smaller model, is doing something legitimate that
//! a cache argument was being used to prevent.
//!
//! So the cost is **measured instead of refused**: [`ContextFilterReport::instructions_changed`]
//! says whether a filter moved the prefix this turn, and the savings it reports cover the
//! instructions as well as the input. A host that cares about cache residency reads that flag; one
//! that has a reason to pay reads it and proceeds.
//!
//! Writing the change back is the caller's job and it is not optional: a projection whose new
//! instructions never reached the provider would report a saving the request did not get. Anything
//! derived from the prefix — a [`CachePlan`](crate::prompt::CachePlan)'s hash, most of all — is
//! rebuilt from the text actually sent. The cache *scope* is not: it identifies a conversation
//! rather than a body of text, so it stays what the host set it to.
//!
//! # Reports are measured, not declared
//!
//! [`ContextFilterReport`] is produced by [`ContextFilterChain::apply`] from the input a filter was
//! given and the input it returned — a filter is never asked what it did. A self-reported saving is
//! a second copy of a fact the chain already holds, and the first time the two disagree the replay
//! assertion is checking the filter's opinion of itself rather than the request that was sent.

use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    error::Result,
    item::{ModelInputItem, estimate},
    state::{RunId, ToolOutputReferenceTracker},
};

/// The model-facing request one filter may project.
///
/// Named after the upstream `ModelInputData` it mirrors, and matching it: both halves are readable
/// and both are replaceable. See the module documentation for what replacing the instructions costs
/// and who is told about it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInputData {
    input: Vec<ModelInputItem>,
    instructions: Option<String>,
}

impl ModelInputData {
    /// Creates the data one chain pass starts from.
    #[must_use]
    pub fn new(input: Vec<ModelInputItem>, instructions: Option<String>) -> Self {
        Self {
            input,
            instructions,
        }
    }

    /// Items the model will receive, in request order.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// Stable system instructions this request carries.
    #[must_use]
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// Replaces the input, keeping the instructions this data arrived with.
    ///
    /// Takes `self` by value like its sibling below: the returned data is the filter's whole
    /// answer, so there is no partially rewritten intermediate for a later stage to observe.
    #[must_use]
    pub fn with_input(mut self, input: Vec<ModelInputItem>) -> Self {
        self.input = input;
        self
    }

    /// Replaces the system instructions, keeping the input this data arrived with.
    ///
    /// `None` sends the request with no stable prefix at all, which is a projection rather than an
    /// oversight and is passed through as one.
    ///
    /// What this costs is in the module documentation: the prefix is what a provider's prompt cache
    /// keys on, so a filter that rewrites it every turn moves that span every turn. The chain
    /// reports the move rather than preventing it.
    #[must_use]
    pub fn with_instructions(mut self, instructions: Option<String>) -> Self {
        self.instructions = instructions;
        self
    }

    /// Takes the projected input out, for a caller rebuilding the request from it.
    #[must_use]
    pub fn into_input(self) -> Vec<ModelInputItem> {
        self.input
    }

    /// Takes both halves out, for a caller rebuilding the whole request.
    ///
    /// The caller that writes a projection back needs both or neither: writing the input while
    /// dropping changed instructions would send a request the reports do not describe.
    #[must_use]
    pub fn into_parts(self) -> (Vec<ModelInputItem>, Option<String>) {
        (self.input, self.instructions)
    }
}

/// Facts about the turn a filter is projecting.
///
/// The reference ledger is here because retention is a structural fact, not a textual one: a filter
/// that keeps a result alive because a later turn still points at it reads that from the ledger
/// rather than searching model narration for the result's name.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct ContextFilterRequest<'a> {
    run_id: &'a RunId,
    current_turn: u64,
    tool_output_references: &'a ToolOutputReferenceTracker,
}

impl<'a> ContextFilterRequest<'a> {
    /// Creates the per-turn facts handed to every filter in one pass.
    #[must_use]
    pub const fn new(
        run_id: &'a RunId,
        current_turn: u64,
        tool_output_references: &'a ToolOutputReferenceTracker,
    ) -> Self {
        Self {
            run_id,
            current_turn,
            tool_output_references,
        }
    }

    /// Identity of the run this request belongs to.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        self.run_id
    }

    /// Whole-run ordinal of the pending turn.
    ///
    /// It spans the run rather than the current segment: the ledger below was restored with the
    /// checkpoint, so a result last referenced before a resume has to stay comparable with the turn
    /// now being prepared.
    #[must_use]
    pub const fn current_turn(&self) -> u64 {
        self.current_turn
    }

    /// Persisted record of which tool outputs later turns still referenced.
    #[must_use]
    pub const fn tool_output_references(&self) -> &ToolOutputReferenceTracker {
        self.tool_output_references
    }
}

/// One pure projection over the model input of a single request.
///
/// Synchronous on purpose. Everything a filter is allowed to do is arithmetic over items it already
/// holds; an implementation that needs to await something needs a model call, a store, or a lock,
/// and all three belong to [`ContextProcessor`](crate::capability::ContextProcessor), which is
/// asynchronous and may record what it spent.
pub trait ContextFilter: Send + Sync + 'static {
    /// Stable identity of this filter, used to attribute its report.
    ///
    /// Two filters of the same kind installed with different settings share a name; a report is
    /// located by its position in the chain, and the name says which kind produced it.
    fn name(&self) -> &str;

    /// Returns the projected model input for this request.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be projected at all — an unreadable stored result,
    /// say. The chain propagates it; a request is not sent on a projection that failed halfway.
    fn filter_model_input(
        &self,
        request: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<ModelInputData>;
}

/// What one filter did to one request.
///
/// Every number is measured by [`ContextFilterChain::apply`] across that filter's own step, so a
/// report describes the difference this filter made rather than the state of the request when it
/// ran. Both savings are signed the same way: **positive means the filter made the request
/// smaller**, and a redaction that replaces a short secret with a longer marker reports negative
/// savings rather than wrapping around to a large one.
///
/// The instructions are measured alongside the input, because a filter may replace them: a pass
/// that trimmed only the prefix would otherwise report a saving of zero on a request it made
/// smaller.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFilterReport {
    filter: String,
    changed: bool,
    chars_saved: i64,
    token_estimate_delta: i64,
    /// Defaulted so a report persisted before instructions became replaceable still loads, reading
    /// as the `false` that was the only possibility when it was written.
    #[serde(default)]
    instructions_changed: bool,
}

impl ContextFilterReport {
    /// Reports a filter that returned its input unchanged.
    fn unchanged(filter: &str) -> Self {
        Self {
            filter: filter.to_owned(),
            changed: false,
            chars_saved: 0,
            token_estimate_delta: 0,
            instructions_changed: false,
        }
    }

    /// Reports the difference one filter made, measured on both sides of its own step.
    fn measured(
        filter: &str,
        before: &InputMeasure,
        after: &InputMeasure,
        instructions_changed: bool,
    ) -> Self {
        Self {
            filter: filter.to_owned(),
            changed: true,
            chars_saved: saved(before.chars, after.chars),
            token_estimate_delta: saved(before.tokens, after.tokens),
            instructions_changed,
        }
    }

    /// Which filter this report describes, as it named itself.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Whether the filter returned an input different from the one it was given.
    ///
    /// A filter whose policy did not fire this turn reports `false` with zero savings, which is what
    /// makes "the trimmer ran but had nothing to trim" distinguishable from "the trimmer was never
    /// installed".
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.changed
    }

    /// Characters this filter removed from the input, negative when it added more than it removed.
    #[must_use]
    pub const fn chars_saved(&self) -> i64 {
        self.chars_saved
    }

    /// The same change in estimated tokens, on the shared
    /// [`estimate`] basis, and signed like [`Self::chars_saved`].
    ///
    /// Estimated rather than counted: a provider's tokenizer is the only thing that can answer this
    /// exactly, and it is not reachable from a provider-neutral projection. The value is comparable
    /// with the context limits derived on that same basis, which is what a saving needs to be
    /// compared against.
    #[must_use]
    pub const fn token_estimate_delta(&self) -> i64 {
        self.token_estimate_delta
    }

    /// Whether this filter moved the stable prefix.
    ///
    /// Reported on its own rather than folded into [`Self::changed`] because the two have different
    /// consequences. Trimming the input costs the tokens it removed and nothing else; replacing the
    /// instructions also moves the span a provider's prompt cache keys on, so the turn after it
    /// starts from a cold prefix. A host weighing residency reads this; the savings above do not
    /// say it, because a small prefix edit and a large input trim can report the same number.
    #[must_use]
    pub const fn instructions_changed(&self) -> bool {
        self.instructions_changed
    }
}

/// The projected request and the per-filter record of how it got that way.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct FilteredModelInput {
    data: ModelInputData,
    reports: Vec<ContextFilterReport>,
}

impl FilteredModelInput {
    /// The data as the last filter left it.
    #[must_use]
    pub const fn data(&self) -> &ModelInputData {
        &self.data
    }

    /// One report per installed filter, in the order they ran.
    #[must_use]
    pub fn reports(&self) -> &[ContextFilterReport] {
        &self.reports
    }

    /// Takes the projection and its reports apart.
    #[must_use]
    pub fn into_parts(self) -> (ModelInputData, Vec<ContextFilterReport>) {
        (self.data, self.reports)
    }
}

/// The ordered filters one run applies before every model call.
///
/// Order is install order and it is load bearing: each filter sees what the one before it produced,
/// so a window policy installed after a trimmer counts the trimmed sizes, and installed before it
/// counts the originals.
#[derive(Clone, Default)]
pub struct ContextFilterChain {
    filters: Vec<Arc<dyn ContextFilter>>,
}

impl ContextFilterChain {
    /// Creates an empty chain, which projects nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            filters: Vec::new(),
        }
    }

    /// Appends a filter after the ones already installed.
    pub fn push(&mut self, filter: Arc<dyn ContextFilter>) {
        self.filters.push(filter);
    }

    /// Whether no filter is installed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// How many filters are installed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.filters.len()
    }

    /// The installed filters, in the order they run.
    #[must_use]
    pub fn filters(&self) -> &[Arc<dyn ContextFilter>] {
        &self.filters
    }

    /// Runs every filter in order and measures what each one did.
    ///
    /// Measurement is lazy. A chain whose filters all pass their input through untouched never walks
    /// the history at all, and that walk — a full render of every item — is the expensive half of
    /// this pass. Once a filter changes something, the measurement it produced becomes the next
    /// filter's starting point, so a chain of `n` filters costs at most `n + 1` walks and usually
    /// far fewer.
    ///
    /// # Errors
    ///
    /// Returns whatever error a filter produced, and whatever measuring an item produced.
    pub fn apply(
        &self,
        request: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<FilteredModelInput> {
        let mut data = data;
        let mut reports = Vec::with_capacity(self.filters.len());
        let mut measure: Option<InputMeasure> = None;

        for filter in &self.filters {
            let before_input = data.input().to_vec();
            let before_instructions = data.instructions().map(str::to_owned);
            let filtered = filter.filter_model_input(request, data)?;

            let instructions_changed = filtered.instructions() != before_instructions.as_deref();
            let report = if filtered.input() == before_input.as_slice() && !instructions_changed {
                ContextFilterReport::unchanged(filter.name())
            } else {
                // The cached measure is only reusable when the previous step left the prefix alone;
                // otherwise it was taken against instructions this step no longer starts from.
                let before = match measure.take() {
                    Some(measure) => measure,
                    None => InputMeasure::of(&before_input, before_instructions.as_deref())?,
                };
                let after = InputMeasure::of(filtered.input(), filtered.instructions())?;
                let report = ContextFilterReport::measured(
                    filter.name(),
                    &before,
                    &after,
                    instructions_changed,
                );
                measure = Some(after);
                report
            };
            reports.push(report);
            data = filtered;
        }

        Ok(FilteredModelInput { data, reports })
    }
}

impl fmt::Debug for ContextFilterChain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.filters.iter().map(|filter| filter.name()))
            .finish()
    }
}

/// What one request measured, on both bases at once.
///
/// Both numbers come from the same walk. Deriving the token estimate from the character total
/// afterwards would round once over the sum instead of once per item, which is not the basis the
/// context limits it gets compared against were built on — and for the same reason the
/// instructions are rounded as their own item rather than added to the character total first.
struct InputMeasure {
    chars: usize,
    tokens: usize,
}

impl InputMeasure {
    fn of(input: &[ModelInputItem], instructions: Option<&str>) -> Result<Self> {
        let mut chars = 0usize;
        let mut tokens = 0usize;
        for item in input {
            let item_chars = estimate::item_chars(item)?;
            chars = chars.saturating_add(item_chars);
            tokens = tokens.saturating_add(estimate::chars_to_tokens(item_chars));
        }
        if let Some(instructions) = instructions {
            let instruction_chars = instructions.chars().count();
            chars = chars.saturating_add(instruction_chars);
            tokens = tokens.saturating_add(estimate::chars_to_tokens(instruction_chars));
        }
        Ok(Self { chars, tokens })
    }
}

/// The signed reduction from `before` to `after`, saturating rather than wrapping.
fn saved(before: usize, after: usize) -> i64 {
    let before = i64::try_from(before).unwrap_or(i64::MAX);
    let after = i64::try_from(after).unwrap_or(i64::MAX);
    before.saturating_sub(after)
}
