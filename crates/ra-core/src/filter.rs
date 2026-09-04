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
//! [`ModelInputData`] carries the request's input *and* its system instructions, but only the input
//! can be replaced. The instructions are the cached prefix: their bytes are what a provider's prompt
//! cache keys on, and a filter runs per turn, so a filter that rewrote them would move the cached
//! span on every call and cost more than everything trimming saves. This is the same rule
//! [`ResolvedPrompt`](crate::prompt::ResolvedPrompt) applies one stage earlier, where a per-run
//! generator is refused the prefix and given the volatile tail instead. The instructions are still
//! handed over because a filter that budgets a whole request has to be able to measure them.
//!
//! The chain enforces this rather than trusting it: a filter that returns different instructions is
//! refused by name, not quietly ignored.
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
    error::{Error, Result},
    item::{ModelInputItem, estimate},
    state::{RunId, ToolOutputReferenceTracker},
};

/// The model-facing request one filter may project.
///
/// Named after the upstream `ModelInputData` it mirrors. The deviation is that
/// [`Self::instructions`] is readable but not replaceable; see the module documentation for why the
/// cached prefix is not a per-turn filter's to rewrite.
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

    /// Stable system instructions this request carries, for measurement only.
    #[must_use]
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// Replaces the input, keeping the instructions this data arrived with.
    ///
    /// The only mutator a filter has, and the reason it takes `self` by value: the returned data is
    /// the filter's whole answer, so there is no partially rewritten intermediate for a later stage
    /// to observe.
    #[must_use]
    pub fn with_input(mut self, input: Vec<ModelInputItem>) -> Self {
        self.input = input;
        self
    }

    /// Takes the projected input out, for a caller rebuilding the request from it.
    #[must_use]
    pub fn into_input(self) -> Vec<ModelInputItem> {
        self.input
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
/// Only the input is measured. The instructions cannot change, so charging them to both sides would
/// add the same constant to each and leave every delta exactly where it is.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFilterReport {
    filter: String,
    changed: bool,
    chars_saved: i64,
    token_estimate_delta: i64,
}

impl ContextFilterReport {
    /// Reports a filter that returned its input unchanged.
    fn unchanged(filter: &str) -> Self {
        Self {
            filter: filter.to_owned(),
            changed: false,
            chars_saved: 0,
            token_estimate_delta: 0,
        }
    }

    /// Reports the difference one filter made, measured on both sides of its own step.
    fn measured(filter: &str, before: &InputMeasure, after: &InputMeasure) -> Self {
        Self {
            filter: filter.to_owned(),
            changed: true,
            chars_saved: saved(before.chars, after.chars),
            token_estimate_delta: saved(before.tokens, after.tokens),
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
    /// [`estimate`](crate::item::estimate) basis, and signed like [`Self::chars_saved`].
    ///
    /// Estimated rather than counted: a provider's tokenizer is the only thing that can answer this
    /// exactly, and it is not reachable from a provider-neutral projection. The value is comparable
    /// with the context limits derived on that same basis, which is what a saving needs to be
    /// compared against.
    #[must_use]
    pub const fn token_estimate_delta(&self) -> i64 {
        self.token_estimate_delta
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
    /// Returns whatever error a filter produced, or a caller error when a filter returned different
    /// system instructions — which it may not do, for the reason in the module documentation.
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

            if filtered.instructions() != before_instructions.as_deref() {
                return Err(Error::caller(format!(
                    "context filter `{}` changed the system instructions; a filter projects the \
                     request input only, because the instructions are the cached prefix and \
                     rewriting them per turn moves the span the cache keys on",
                    filter.name()
                )));
            }

            let report = if filtered.input() == before_input.as_slice() {
                ContextFilterReport::unchanged(filter.name())
            } else {
                let before = match measure.take() {
                    Some(measure) => measure,
                    None => InputMeasure::of(&before_input)?,
                };
                let after = InputMeasure::of(filtered.input())?;
                let report = ContextFilterReport::measured(filter.name(), &before, &after);
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

/// What one input measured, on both bases at once.
///
/// Both numbers come from the same walk. Deriving the token estimate from the character total
/// afterwards would round once over the sum instead of once per item, which is not the basis the
/// context limits it gets compared against were built on.
struct InputMeasure {
    chars: usize,
    tokens: usize,
}

impl InputMeasure {
    fn of(input: &[ModelInputItem]) -> Result<Self> {
        let mut chars = 0usize;
        let mut tokens = 0usize;
        for item in input {
            let item_chars = estimate::item_chars(item)?;
            chars = chars.saturating_add(item_chars);
            tokens = tokens.saturating_add(estimate::chars_to_tokens(item_chars));
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
