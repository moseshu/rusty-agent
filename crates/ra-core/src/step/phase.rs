//! R3-10's rule for assigning the two output channels, derived in exactly one place.
//!
//! [`item::phase`](crate::item::phase) owns the *value* an assistant message carries. This module
//! owns *who decides it*: the settled turn. Both the producer — `ra-runtime`'s turn settlement,
//! through [`resolve_output_phases`] — and the gate that every [`SingleStepResult`] passes read the
//! rule from here, so there is no second statement of it to drift from the first.
//!
//! # The rule
//!
//! **The turn's outcome decides, not the provider.** A model can request an action in the same
//! response it marks final, and it cannot know that a tool is about to fail, that a guard will stop
//! the run, or that a host will be asked for an approval — so a phase read straight off the wire
//! says "final" for turns that turned out to be the middle of the work.
//!
//! **Only the last assistant message of a settled turn is the delivery.** One response can carry
//! narration and then the answer — that is the shape this milestone is modelled on — and stamping
//! both `Final` costs twice: the UI renders two closing deliveries instead of the
//! intent → action → result chain R3-10 exists to produce, and the record goes back to the model as
//! input next turn, teaching it that pre-tool narration is what a final answer looks like.
//!
//! Nothing here reads message text. Which channel a message belongs to follows from how the turn
//! settled, and inferring it from what the model wrote is the text-driven control flow R7-10
//! forbids.
//!
//! [`SingleStepResult`]: crate::step::SingleStepResult

use super::NextStep;
use crate::item::{MessageRole, OutputPhase, RunItem, RunItemKind};

/// Puts every assistant message of one settled turn on its channel.
///
/// Records of other kinds, and messages from user or system roles, pass through untouched.
#[must_use]
pub fn resolve_output_phases(items: Vec<RunItem>, next_step: &NextStep) -> Vec<RunItem> {
    let delivery = delivery_index(&items, next_step);
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| item.with_output_phase(phase_at(index, delivery)))
        .collect()
}

/// Which record of a settled turn delivered the run, if any did.
///
/// `None` for every non-terminal outcome: a turn that hands off, asks for an approval, or owes the
/// model another call has not delivered anything, however the provider labelled it.
pub(crate) fn delivery_index(items: &[RunItem], next_step: &NextStep) -> Option<usize> {
    match next_step {
        NextStep::FinalOutput { .. } => items.iter().rposition(is_assistant_message),
        NextStep::RunAgain | NextStep::Handoff { .. } | NextStep::Interruption { .. } => None,
    }
}

/// The channel a record at `index` belongs to, given where the delivery is.
pub(crate) const fn phase_at(index: usize, delivery: Option<usize>) -> OutputPhase {
    match delivery {
        Some(delivery) if delivery == index => OutputPhase::Final,
        _ => OutputPhase::Commentary,
    }
}

/// Whether this record is something the model said in its own voice.
pub(crate) fn is_assistant_message(item: &RunItem) -> bool {
    matches!(item.kind(), RunItemKind::Message(message)
        if matches!(message.role(), MessageRole::Assistant))
}
