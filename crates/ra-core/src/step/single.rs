//! R3-3: the one product of settling a single turn.
//!
//! Everything a turn decided lives here — what went in, what the model said, what was generated,
//! what the session must store, and what happens next. The streaming and non-streaming paths
//! consume this same value; two result shapes would be two loops, and the second one always drifts.
//!
//! # The four item lists are not four names for one list
//!
//! - `original_input` is what started the run, kept so a retry or a nested run can rebuild input
//!   without replaying the loop;
//! - `pre_step_items` is what earlier turns generated;
//! - `new_step_items` is what this turn generated **and carries forward**;
//! - `session_step_items` is what this turn generated **and must be stored** — the complete,
//!   unfiltered set.
//!
//! The last two exist separately because context budgeting filters the model-facing view while the
//! session stays authoritative. Collapsing them means a filter silently deletes history.
//!
//! # What is deliberately absent
//!
//! **The four guardrail result lists.** R3-3 names input, output, tool-input, and tool-output
//! guardrail results as fields here, and they do belong here — but `InputGuardrailResult`,
//! `OutputGuardrailResult`, and the three-state tool guardrail result are R7-1 and R7-3's types and
//! do not exist yet. Standing in a `Vec<Value>` or a bare `bool` freezes the wrong shape before the
//! contract is written, which is the same call R3-1 made when it gave `FinalOutput` a
//! [`FinishReason`](crate::finish::FinishReason) instead of a placeholder output value. This struct
//! is `#[non_exhaustive]` with private fields and a builder precisely so R7 can add them without a
//! breaking change.

use std::collections::{BTreeMap, BTreeSet};

use super::{NextStep, ProcessedResponse};
use crate::{
    error::{Error, Result},
    item::{ItemId, ModelInputItem, ModelResponse, RunItem},
};

type ItemsById<'a> = BTreeMap<&'a ItemId, &'a RunItem>;

/// Everything one settled turn produced.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SingleStepResult {
    original_input: Vec<ModelInputItem>,
    model_response: ModelResponse,
    pre_step_items: Vec<RunItem>,
    new_step_items: Vec<RunItem>,
    session_step_items: Vec<RunItem>,
    nested_history_owned_items: Vec<ItemId>,
    processed_response: ProcessedResponse,
    next_step: NextStep,
}

impl SingleStepResult {
    /// Starts a builder.
    pub fn builder() -> SingleStepResultBuilder {
        SingleStepResultBuilder::new()
    }

    /// The input the run started from.
    #[must_use]
    pub fn original_input(&self) -> &[ModelInputItem] {
        &self.original_input
    }

    /// The raw terminal state of this turn's model call, kept for usage accounting, provider
    /// continuation IDs, and replay.
    #[must_use]
    pub const fn model_response(&self) -> &ModelResponse {
        &self.model_response
    }

    /// Items earlier turns generated.
    #[must_use]
    pub fn pre_step_items(&self) -> &[RunItem] {
        &self.pre_step_items
    }

    /// Items this turn generated and carries forward.
    #[must_use]
    pub fn new_step_items(&self) -> &[RunItem] {
        &self.new_step_items
    }

    /// Items this turn generated that the session must store, complete and unfiltered.
    #[must_use]
    pub fn session_step_items(&self) -> &[RunItem] {
        &self.session_step_items
    }

    /// Which stored items belong to a nested run rather than this one.
    ///
    /// Recorded as IDs, not copies: the record itself already lives in
    /// [`Self::session_step_items`], and a second copy is a second thing to keep in sync. R12-2
    /// attributes nested results with this, and R9-12 reconciles history with it, so neither has to
    /// re-derive ownership from list position — which stops being true the moment a handoff
    /// rewrites history.
    #[must_use]
    pub fn nested_history_owned_items(&self) -> &[ItemId] {
        &self.nested_history_owned_items
    }

    /// This turn's classification, kept because resuming an interruption needs the bound actions
    /// that produced it rather than a fresh guess at what the model meant.
    #[must_use]
    pub const fn processed_response(&self) -> &ProcessedResponse {
        &self.processed_response
    }

    /// What the loop does next.
    #[must_use]
    pub const fn next_step(&self) -> &NextStep {
        &self.next_step
    }

    /// Everything generated so far, earlier turns first. This is what the next turn's input is
    /// built from.
    pub fn generated_items(&self) -> impl Iterator<Item = &RunItem> {
        self.pre_step_items.iter().chain(&self.new_step_items)
    }
}

/// Builds a validated [`SingleStepResult`].
///
/// `model_response`, `processed_response`, `session_step_items`, and `next_step` are required.
/// `session_step_items` in particular has **no default**: defaulting it to `new_step_items` would
/// make a turn that filters its model-facing items silently persist the filtered set, and the loss
/// only shows up a session later.
#[must_use]
#[derive(Debug, Default)]
pub struct SingleStepResultBuilder {
    original_input: Vec<ModelInputItem>,
    model_response: Option<ModelResponse>,
    pre_step_items: Vec<RunItem>,
    new_step_items: Vec<RunItem>,
    session_step_items: Option<Vec<RunItem>>,
    nested_history_owned_items: Vec<ItemId>,
    processed_response: Option<ProcessedResponse>,
    next_step: Option<NextStep>,
}

impl SingleStepResultBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the input the run started from.
    pub fn original_input(mut self, input: Vec<ModelInputItem>) -> Self {
        self.original_input = input;
        self
    }

    /// Sets this turn's model response.
    pub fn model_response(mut self, response: ModelResponse) -> Self {
        self.model_response = Some(response);
        self
    }

    /// Sets the items earlier turns generated.
    pub fn pre_step_items(mut self, items: Vec<RunItem>) -> Self {
        self.pre_step_items = items;
        self
    }

    /// Sets the items this turn carries forward.
    pub fn new_step_items(mut self, items: Vec<RunItem>) -> Self {
        self.new_step_items = items;
        self
    }

    /// Sets the complete, unfiltered items this turn must store.
    pub fn session_step_items(mut self, items: Vec<RunItem>) -> Self {
        self.session_step_items = Some(items);
        self
    }

    /// Marks stored items as belonging to a nested run.
    pub fn nested_history_owned_items(mut self, item_ids: Vec<ItemId>) -> Self {
        self.nested_history_owned_items = item_ids;
        self
    }

    /// Sets this turn's classification.
    pub fn processed_response(mut self, processed: ProcessedResponse) -> Self {
        self.processed_response = Some(processed);
        self
    }

    /// Sets what the loop does next.
    pub fn next_step(mut self, next_step: NextStep) -> Self {
        self.next_step = Some(next_step);
        self
    }

    /// Validates the settlement and returns it.
    pub fn build(self) -> Result<SingleStepResult> {
        let model_response = self
            .model_response
            .ok_or_else(|| Error::caller("a settled turn requires its `model_response`"))?;
        let processed_response = self
            .processed_response
            .ok_or_else(|| Error::caller("a settled turn requires its `processed_response`"))?;
        let next_step = self
            .next_step
            .ok_or_else(|| Error::caller("a settled turn requires a `next_step`"))?;
        let session_step_items = self.session_step_items.ok_or_else(|| {
            Error::caller(
                "a settled turn requires `session_step_items`; it has no default because \
                 defaulting it to the model-facing list would silently drop filtered records",
            )
        })?;

        let session_items = index_unique_items(&session_step_items, "`session_step_items`")?;
        let pre_items = index_unique_items(&self.pre_step_items, "`pre_step_items`")?;
        let new_items = index_unique_items(&self.new_step_items, "`new_step_items`")?;

        check_classifies_this_response(&processed_response, &model_response)?;
        check_response_reaches_the_session(&model_response, &session_items)?;
        check_carried_items(&new_items, &pre_items, &session_items)?;
        check_session_items_are_new(&session_items, &pre_items)?;
        check_nested_ownership(&self.nested_history_owned_items, &session_items)?;
        check_next_step(&next_step, &processed_response, &session_items)?;

        Ok(SingleStepResult {
            original_input: self.original_input,
            model_response,
            pre_step_items: self.pre_step_items,
            new_step_items: self.new_step_items,
            session_step_items,
            nested_history_owned_items: self.nested_history_owned_items,
            processed_response,
            next_step,
        })
    }
}

/// The classification has to be *of this response*.
///
/// Nothing else in the struct can catch a mismatch: usage accounting would come from one call, the
/// bound actions from another, and resume would replay a third story.
///
/// The comparison is whole records, not just IDs. Classification copies the adapter's records
/// verbatim, so full equality is what actually holds — and matching IDs over different payloads is
/// exactly the case an ID check waves through while re-introducing the mismatch it was added to
/// stop. If classification ever stops being non-destructive, this is the line that says so.
fn check_classifies_this_response(
    processed_response: &ProcessedResponse,
    model_response: &ModelResponse,
) -> Result<()> {
    if processed_response.new_items() == model_response.output() {
        return Ok(());
    }
    let same_identities = processed_response
        .new_items()
        .iter()
        .map(RunItem::id)
        .eq(model_response.output().iter().map(RunItem::id));
    Err(Error::caller(if same_identities {
        "`processed_response` and `model_response` hold the same item ids but different content; \
         classification copies records unchanged, so one of them was rewritten"
    } else {
        "`processed_response` classifies a different response than `model_response`; the two must \
         be the same records in the same order"
    }))
}

/// Indexes one history list, rejecting an ID before it can ambiguously refer to two records.
fn index_unique_items<'a>(items: &'a [RunItem], list_name: &str) -> Result<ItemsById<'a>> {
    let mut indexed = BTreeMap::new();
    for item in items {
        if indexed.insert(item.id(), item).is_some() {
            return Err(Error::caller(format!(
                "{list_name} contains item id `{}` more than once; a settled turn needs one \
                 authoritative record per id",
                item.id()
            )));
        }
    }
    Ok(indexed)
}

/// The session may enrich a record's metadata, but may not replace its model-visible payload.
fn check_stored_payload(expected: &RunItem, stored: &RunItem, relation: &str) -> Result<()> {
    if expected.kind() == stored.kind() {
        return Ok(());
    }
    Err(Error::caller(format!(
        "item `{}` is {relation}, but its stored payload differs; session metadata may be \
         enriched but the record's payload must remain unchanged",
        expected.id()
    )))
}

/// Whatever the model produced has to reach the session.
///
/// Checking only [`SingleStepResult::new_step_items`] leaves the dangerous case open: that list is
/// allowed to be filtered, and once it is filtered to nothing its own subset check is vacuously
/// true while the turn quietly stores none of what the model said.
///
/// The session may add provenance, host-only session data, and raw provider payload, so full
/// record equality would be too strict. Its ID and model-visible payload still have to match: a
/// same-ID message cannot stand in for the tool call the model actually made.
fn check_response_reaches_the_session(
    model_response: &ModelResponse,
    session_items: &ItemsById<'_>,
) -> Result<()> {
    for item in model_response.output() {
        let Some(stored) = session_items.get(item.id()) else {
            return Err(Error::caller(format!(
                "the model produced item `{}` but the turn stores no record of it; the session is \
                 the authoritative history and cannot omit what the model said",
                item.id()
            )));
        };
        check_stored_payload(item, stored, "from the model response")?;
    }
    Ok(())
}

/// What the turn carries forward has to be stored, and cannot already belong to an earlier turn.
fn check_carried_items(
    new_step_items: &ItemsById<'_>,
    pre_step_items: &ItemsById<'_>,
    session_items: &ItemsById<'_>,
) -> Result<()> {
    for (item_id, item) in new_step_items {
        let Some(stored) = session_items.get(item_id) else {
            return Err(Error::caller(format!(
                "item `{item_id}` is carried forward but not stored; the session record is the \
                 authoritative one and cannot be a subset of the model-facing view"
            )));
        };
        check_stored_payload(item, stored, "carried forward")?;
        if pre_step_items.contains_key(item_id) {
            return Err(Error::caller(format!(
                "item `{item_id}` appears in both `pre_step_items` and `new_step_items`; history \
                 reconciliation would count it twice"
            )));
        }
    }
    Ok(())
}

/// The stored current-turn records cannot reuse an ID from the already stored history.
fn check_session_items_are_new(
    session_items: &ItemsById<'_>,
    pre_step_items: &ItemsById<'_>,
) -> Result<()> {
    for item_id in session_items.keys() {
        if pre_step_items.contains_key(item_id) {
            return Err(Error::caller(format!(
                "item `{item_id}` appears in both `pre_step_items` and `session_step_items`; \
                 storing it again would make one session ID refer to two turns"
            )));
        }
    }
    Ok(())
}

/// Nested ownership points at records that exist.
fn check_nested_ownership(
    nested_history_owned_items: &[ItemId],
    session_items: &ItemsById<'_>,
) -> Result<()> {
    for item_id in nested_history_owned_items {
        if !session_items.contains_key(item_id) {
            return Err(Error::caller(format!(
                "item `{item_id}` is marked as owned by a nested run but is not among the stored \
                 items"
            )));
        }
    }
    Ok(())
}

/// A pending decision the loop walks past is a decision nobody ever makes.
///
/// Ending the run is allowed — there is nothing left to answer — but continuing is not.
fn check_next_step(
    next_step: &NextStep,
    processed_response: &ProcessedResponse,
    session_items: &ItemsById<'_>,
) -> Result<()> {
    match next_step {
        NextStep::RunAgain | NextStep::Handoff { .. } if processed_response.has_interruptions() => {
            Err(Error::caller(
                "the response carries pending approvals, so the turn cannot settle to a state that \
                 continues the loop",
            ))
        }
        NextStep::Interruption { items } => {
            check_interruption(items, processed_response, session_items)
        }
        NextStep::RunAgain | NextStep::Handoff { .. } | NextStep::FinalOutput { .. } => Ok(()),
    }
}

fn check_interruption(
    items: &[RunItem],
    processed_response: &ProcessedResponse,
    session_items: &ItemsById<'_>,
) -> Result<()> {
    if items.is_empty() {
        return Err(Error::caller(
            "an interruption must carry at least one item for the host to answer",
        ));
    }
    let asked: BTreeSet<&ItemId> = items.iter().map(RunItem::id).collect();
    for item in items {
        // `NextStep::interruption()` checks this too, but the variant is directly constructible on
        // purpose (settlement lives inside the framework), so that constructor is a convention
        // rather than a gate. Here is where a non-approval item stops being a mistake and becomes a
        // run that waits forever for a decision nobody was asked to make, so the gate belongs here
        // as well.
        if !item.kind().is_interruption() {
            return Err(Error::caller(format!(
                "interruption item `{}` is of kind `{}`, which no host can approve; the run would \
                 wait for a decision nobody was asked to make",
                item.id(),
                item.kind().label()
            )));
        }
        let Some(stored) = session_items.get(item.id()) else {
            return Err(Error::caller(format!(
                "interruption item `{}` is not among the stored items; resume reads the session, \
                 so it could never be answered",
                item.id()
            )));
        };
        check_stored_payload(item, stored, "an interruption item")?;
    }

    // Every pending decision the response raised has to be among the questions asked. Stopping the
    // run and then asking about only some of them leaves the rest to be answered by a turn that
    // never comes.
    //
    // The reverse containment is **not** required, and requiring it would break the ordinary
    // approval flow: a local tool whose `needs_approval` fires produces a `ToolApproval` during
    // execution, which is a legitimate question the model's response never contained.
    for pending in processed_response.interruptions() {
        if !asked.contains(pending.id()) {
            return Err(Error::caller(format!(
                "the response raised pending decision `{}` but the interruption does not ask about \
                 it",
                pending.id()
            )));
        }
    }
    Ok(())
}
