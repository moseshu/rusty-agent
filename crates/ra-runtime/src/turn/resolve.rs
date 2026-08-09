//! Decides which of the four [`NextStep`] states applies. The single point where control flow
//! converges (R3-4).
//!
//! The order of the branches below **is** the priority rule, and it is written once here so no
//! call site re-derives it. Reading downwards: a pending decision outranks everything, because a
//! run that walks past one waits forever for an answer nobody was asked for; a transfer of control
//! outranks finishing, because the next agent has not spoken yet; a tool result promoted to the
//! final answer outranks another turn, because that is the round trip the promotion exists to save;
//! and anything still owed an answer outranks concluding.

use ra_core::{
    error::Result,
    finish::FinishReason,
    item::RunItem,
    step::{NextStep, ProcessedResponse},
};

use super::batch::TurnExecution;

/// Settles one turn into exactly one of the four states.
pub fn resolve_next_step(
    processed: &ProcessedResponse,
    execution: &TurnExecution,
) -> Result<NextStep> {
    if execution.has_interruptions() {
        return NextStep::interruption(execution.interruptions().to_vec());
    }

    // Handoffs never reach here today: `execute_actions` refuses them before this point, because
    // resolving a target to a runnable agent is R17's contract. When R17 lands, this is where the
    // resolved declaration becomes `NextStep::Handoff`.

    if let Some(reason) = check_for_final_output_from_tools(processed, execution) {
        return Ok(NextStep::FinalOutput { reason });
    }

    // Something was owed an answer and now has one — including a call that resolved to nothing,
    // whose failure observation the model has not seen yet. Concluding here would end the run on
    // a result the model never got to read.
    if processed.has_tools_or_approvals_to_run() {
        return Ok(NextStep::RunAgain);
    }

    // The model asked for nothing further. This is deliberately *not* keyed on message content or
    // on an output phase: "no actions requested" is a structural fact every provider expresses the
    // same way, while reading intent out of the text is the control flow R7-10 forbids.
    Ok(NextStep::FinalOutput {
        reason: FinishReason::Final,
    })
}

/// R3-5's insertion point for `tool_use_behavior`.
///
/// The default behaviour is to run the model again, so this always declines. `StopOnFirstTool`,
/// `StopAtTools`, and the custom form all answer here, and they answer with a
/// [`FinishReason::ToolStop`] rather than a boolean so the run result says *why* it stopped without
/// the host having to reconstruct it.
const fn check_for_final_output_from_tools(
    _processed: &ProcessedResponse,
    _execution: &TurnExecution,
) -> Option<FinishReason> {
    None
}

/// Everything this turn generated, in the order it happened: what the model said, then what
/// answering it produced.
#[must_use]
pub fn step_items(processed: &ProcessedResponse, execution: &TurnExecution) -> Vec<RunItem> {
    let mut items = processed.new_items().to_vec();
    items.extend(execution.new_items().iter().cloned());
    items
}
