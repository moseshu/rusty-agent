//! Decides which of the four [`NextStep`] states applies. The single point where control flow
//! converges (R3-4).
//!
//! The order of the branches below **is** the priority rule, and it is written once here so no
//! call site re-derives it. Reading downwards: a pending decision outranks everything, because a
//! run that walks past one waits forever for an answer nobody was asked for; a transfer of control
//! outranks finishing, because the next agent has not spoken yet; a tool result promoted to the
//! final answer outranks another turn, because that is the round trip the promotion exists to save;
//! and anything still owed an answer outranks concluding.

use std::collections::BTreeMap;

use ra_core::{
    agent::{AgentSpec, ToolUseBehavior},
    cancel::CancelScope,
    error::{Error, Result},
    finish::FinishReason,
    item::{ItemId, ItemProvenance, RunItem},
    step::{NextStep, ProcessedResponse, resolve_output_phases},
};

use super::batch::TurnExecution;

/// Settles one turn into exactly one of the four states.
pub async fn resolve_next_step(
    processed: &ProcessedResponse,
    execution: &TurnExecution,
    tool_use_behavior: &ToolUseBehavior,
    cancel: &CancelScope,
) -> Result<NextStep> {
    if execution.has_interruptions() {
        return NextStep::interruption(execution.interruptions().to_vec());
    }

    // Handoffs never reach here today: `execute_actions` refuses them before this point, because
    // resolving a target to a runnable agent is R17's contract. When R17 lands, this is where the
    // resolved declaration becomes `NextStep::Handoff`.

    if check_for_final_output_from_tools(execution, tool_use_behavior, cancel).await? {
        return Ok(NextStep::FinalOutput {
            reason: FinishReason::ToolStop,
        });
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

/// Applies `tool_use_behavior`: whether this response's tool results end the run.
///
/// Only the answer is returned. The [`FinishReason::ToolStop`] that goes with it is written at the
/// single call site above, because every policy here stops for the same reason — the caller would
/// only be choosing between one value and itself.
///
/// The decision reads [`TurnExecution::tool_results`], not `new_items`: what a policy is allowed to
/// promote is decided where the batch settles, not by re-inspecting records here.
///
/// Whether the policy is one this runtime understands is checked **before** the results are looked
/// at. [`ToolUseBehavior`] is `#[non_exhaustive]` and lives in another crate, so a catch-all is
/// mandatory rather than a choice — but a configuration error that surfaced only on the turns where
/// the model happened to call a tool would be the same defect the entry checkpoint in
/// [`execute_actions`](super::batch::execute_actions) exists to prevent. That arm is unreachable
/// from the test workspace for the same `#[non_exhaustive]` reason: no variant outside this list
/// exists to construct yet.
async fn check_for_final_output_from_tools(
    execution: &TurnExecution,
    tool_use_behavior: &ToolUseBehavior,
    cancel: &CancelScope,
) -> Result<bool> {
    let tool_results = execution.tool_results();
    match tool_use_behavior {
        ToolUseBehavior::RunLlmAgain => Ok(false),
        ToolUseBehavior::StopOnFirstTool => Ok(!tool_results.is_empty()),
        ToolUseBehavior::StopAtTools { names } => Ok(tool_results.iter().any(|result| {
            names.contains(result.tool().name()) || names.contains(result.tool().qualified_name())
        })),
        ToolUseBehavior::Custom(handler) => {
            if tool_results.is_empty() {
                return Ok(false);
            }
            // Third-party `async` code, so it is never awaited bare — the cancellation contract's
            // one rule. A handler that ignored a stop signal would otherwise keep the whole run
            // alive after the user asked it to stop, with no tool left running to blame. There is
            // no entry check here because `CancelScope::run` already refuses to start work on an
            // already-cancelled scope; a second one would only restate its contract.
            let stop = cancel.run(handler.should_stop(tool_results)).await??;

            // The exit check is not symmetry with that entry check, and it is not optional.
            // `CancelScope::run` polls the handler *before* it looks at the cancellation, so a
            // handler that becomes ready in the very wake-up that delivered the interrupt wins the
            // race and returns normally — the same `select` ordering the batch drain in
            // `super::batch` relies on, read from the other side. Nothing downstream would catch
            // it: settlement does no further awaiting and the runner breaks straight out of the
            // loop on `FinalOutput`. The run would report `FinishReason::ToolStop`, whose
            // `is_complete()` tells anything downstream that no closeout is owed and the run
            // reached its own success edge — claiming the agent finished on the turn the user
            // stopped it.
            cancel.ensure_not_cancelled()?;
            Ok(stop)
        }
        _ => Err(Error::config(
            "this runtime does not support the configured tool-use behavior",
        )),
    }
}

/// Everything this turn generated, in the order it happened: what the model said, then what
/// answering it produced.
///
/// This is also where the turn's records are attributed (R3-12) and where each assistant message
/// gets its output channel (R3-10), because it is the one place that sees all of them. `public` is
/// the user's agent even when a prepared instance executed the turn: a session read back later has
/// to say who produced a record in terms the user recognises, and an execution-time clone is not
/// something they ever configured.
#[must_use]
pub fn step_items(
    processed: &ProcessedResponse,
    execution: &TurnExecution,
    public: &AgentSpec,
    next_step: &NextStep,
) -> Vec<RunItem> {
    let mut items = processed.new_items().to_vec();
    items.extend(execution.new_items().iter().cloned());
    let items = items
        .into_iter()
        .map(|item| attribute(item, public))
        .collect();
    // R3-10's rule lives in `ra-core::step::phase`, where the `SingleStepResult` gate reads it
    // too. Restating it here would make the producer and the gate two statements of one rule, and
    // the day they disagreed the gate would be checking its own copy.
    resolve_output_phases(items, next_step)
}

/// Re-points a settled interruption at the records the turn actually stores.
///
/// [`resolve_next_step`] has to run before [`step_items`] — the output-phase rule reads the
/// decision, so the decision cannot read the records — which leaves the pending items it carries as
/// pre-settlement copies of records written below. Left that way, one settled turn holds two
/// unequal copies of every pending decision: the stored one names its producer and the one offered
/// to the host does not. Reconciling at the single point that produces the decision is what keeps
/// each consumer from having to look the stored copy up for itself, and keeps the ones that only
/// pass it along — a turn record, a checkpoint, a resume — from carrying the copy that lost.
///
/// **Ask order is preserved, not record order.** The two agree today, but what the host was
/// promised is the order it was asked in — hosted approvals as the model produced them, then the
/// calls that turned out to need one — and re-deriving that from record positions would silently
/// renumber the questions the day the two orders diverge.
///
/// The missing-record arm is unreachable by construction: every interruption is either one of
/// `processed`'s own items or an approval the batch also pushed onto `execution.new_items`, and
/// [`step_items`] is the union of those two. It answers rather than unwraps because that
/// containment is an invariant of another module, and a silent `expect` here would report the
/// breakage as a panic in the runner rather than as a settlement that named the item it lost.
pub(super) fn rebind_interruption(next_step: NextStep, items: &[RunItem]) -> Result<NextStep> {
    match next_step {
        NextStep::Interruption { items: pending } => {
            let stored: BTreeMap<&ItemId, &RunItem> =
                items.iter().map(|item| (item.id(), item)).collect();
            let pending = pending
                .iter()
                .map(|item| {
                    stored.get(item.id()).copied().cloned().ok_or_else(|| {
                        Error::caller(format!(
                            "pending decision `{}` is missing from the records this turn stores; \
                             the host would be asked about something the session never kept",
                            item.id()
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            NextStep::interruption(pending)
        }
        // No `_` arm, for the reason `NextStep` is exhaustive: a fifth state has to say whether it
        // carries a copy of this turn's records, and this is one of the sites that must stop
        // compiling until it does.
        step @ (NextStep::RunAgain | NextStep::FinalOutput { .. } | NextStep::Handoff { .. }) => {
            Ok(step)
        }
    }
}

/// Files one record under the public agent, leaving an existing attribution alone.
///
/// Only fills what is empty. A record that already names a producer got it from something that
/// knew better — R12's nested runs will attribute their own items to the sub-agent — and
/// overwriting that would re-label a sub-agent's work as the parent's.
fn attribute(item: RunItem, public: &AgentSpec) -> RunItem {
    if item.provenance().is_some() {
        return item;
    }
    item.with_provenance(ItemProvenance::new(public.id().clone()).with_agent_name(public.name()))
}
