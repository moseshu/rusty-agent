//! One thought -> a batch of actions -> a batch of observations (R3-4).
//!
//! Every bound action the response produced is answered here, and **every one of them is answered**:
//! a call that runs, a call that needs a human, and a call that resolved to nothing all leave with
//! a record paired to their `call_id`. That is the property the next request depends on — a tool
//! call with no output makes the history malformed, and no provider accepts it.
//!
//! Execution is sequential. R3-4b replaces this walk with the real loop shape — reads in parallel,
//! writes serialised under a lock — and R3-4c owns how a batch of concurrent calls is collected and
//! cancelled. Both replace the body of [`execute_actions`] without moving the stage.

use ra_core::{
    cancel::CancelScope,
    error::{Error, Result, ToolErrorKind},
    item::{AgentId, CallId, ItemId, RunItem, RunItemKind, ToolCallOutput},
    state::ToolUseTracker,
    step::ProcessedResponse,
    tool::ToolRuntimeContext,
};
use serde_json::json;

use crate::tool::dispatch::{ToolDispatch, ToolDispatchRequest, dispatch_tool};

/// What executing one turn's actions produced.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct TurnExecution {
    new_items: Vec<RunItem>,
    interruptions: Vec<RunItem>,
}

impl TurnExecution {
    /// Records generated while answering the response's actions, in action order.
    #[must_use]
    pub fn new_items(&self) -> &[RunItem] {
        &self.new_items
    }

    /// Decisions the host owes before the run continues, response-level ones first.
    #[must_use]
    pub fn interruptions(&self) -> &[RunItem] {
        &self.interruptions
    }

    /// Whether the run has to stop and ask.
    #[must_use]
    pub fn has_interruptions(&self) -> bool {
        !self.interruptions.is_empty()
    }
}

/// Inputs for answering one classified response.
#[must_use]
#[non_exhaustive]
pub struct TurnExecutionRequest<'a> {
    processed: &'a ProcessedResponse,
    agent_id: &'a AgentId,
    tool_use: &'a ToolUseTracker,
    context: &'a dyn ToolRuntimeContext,
    cancel: &'a CancelScope,
}

impl<'a> TurnExecutionRequest<'a> {
    /// Creates a request.
    pub fn new(
        processed: &'a ProcessedResponse,
        agent_id: &'a AgentId,
        tool_use: &'a ToolUseTracker,
        context: &'a dyn ToolRuntimeContext,
        cancel: &'a CancelScope,
    ) -> Self {
        Self {
            processed,
            agent_id,
            tool_use,
            context,
            cancel,
        }
    }
}

/// Answers every action the response bound.
pub async fn execute_actions(request: TurnExecutionRequest<'_>) -> Result<TurnExecution> {
    let processed = request.processed;
    let mut execution = TurnExecution::default();

    // The checkpoint belongs at the entry, not only in the loop below. A response that bound no
    // executable call still *settles* — into another turn, into a question for the host, or into
    // `FinishReason::Final` — and settling a cancelled run as `Final` is the worst of the three:
    // `is_complete()` would say the agent reached its own conclusion, so R15 sees no closeout owed
    // and R17-3 takes the success edge. Whether a cancelled turn reports as cancelled must not
    // depend on whether the model happened to name a tool that resolved.
    request.cancel.ensure_not_cancelled()?;

    // A transfer of control cannot be executed without the agent registry that resolves an
    // `AgentId` to a declaration, which R17 owns. The branch is unreachable today — preparation
    // advertises no handoffs, so classification can produce none — and it fails loudly rather than
    // letting the turn continue as if the model had asked for nothing.
    execute_handoffs(processed)?;

    // Decisions the model's own response raised (hosted approvals) are asked about first: they are
    // already stored records, and the host sees them in the order the model produced them.
    execution
        .interruptions
        .extend(processed.interruptions().cloned());

    for action in processed.functions() {
        request.cancel.ensure_not_cancelled()?;
        // Asked of the action, never rebuilt from its parts: settlement recorded this turn under
        // `identity()` a moment ago, and a second derivation that drifted would look up something
        // nothing ever recorded and hand the breaker a permanent zero.
        let repeat_streak = request
            .tool_use
            .repeat_streak(request.agent_id, &action.identity());
        let dispatch = dispatch_tool(ToolDispatchRequest::new(
            action.tool(),
            action.call_id(),
            action.call().arguments(),
            request.context,
            request.cancel,
            repeat_streak,
        ))
        .await?;

        match dispatch {
            ToolDispatch::Observed(output) => {
                execution
                    .new_items
                    .push(output_item(action.call_id(), output));
            }
            ToolDispatch::AwaitingApproval(approval) => {
                let item = RunItem::new(
                    approval_item_id(action.call_id()),
                    RunItemKind::ToolApproval(approval),
                );
                execution.interruptions.push(item.clone());
                execution.new_items.push(item);
            }
        }
    }

    // Not redundant with the entry check: the loop above awaits, and a cancellation that arrives
    // while the last dispatch is completing can lose that race and leave the scope cancelled here.
    request.cancel.ensure_not_cancelled()?;

    // A name the turn never advertised still owes an output. Answering it in the same structured
    // shape a failing tool uses means the model reads one error format, not two.
    for missing in processed.tools_not_found() {
        let error = Error::tool(
            ToolErrorKind::NotFound,
            missing.name(),
            "the turn did not advertise this tool",
        );
        execution.new_items.push(output_item(
            missing.call_id(),
            ToolCallOutput::new(
                missing.call_id().clone(),
                json!({ "error": { "code": error.code(), "tool": missing.name() } }),
            )
            .with_error(true),
        ));
    }

    Ok(execution)
}

/// R17's insertion point for executing a transfer of control.
fn execute_handoffs(processed: &ProcessedResponse) -> Result<()> {
    match processed.handoffs().first() {
        None => Ok(()),
        Some(handoff) => Err(Error::caller(format!(
            "the response hands off to agent `{}`, but resolving a handoff target to a runnable \
             agent is R17's contract and does not exist yet",
            handoff.target_agent()
        ))),
    }
}

fn output_item(call_id: &CallId, output: ToolCallOutput) -> RunItem {
    RunItem::new(output_item_id(call_id), RunItemKind::ToolCallOutput(output))
}

/// Identity of the record that answers one call.
///
/// Derived from the call rather than minted randomly: the answer's identity *is* "the output of
/// this call", which keeps it stable across a replay and lets R9-12 append idempotently without a
/// side table. This function and [`approval_item_id`] are the one place to change if R9-12 decides
/// the session should own item identity instead.
fn output_item_id(call_id: &CallId) -> ItemId {
    ItemId::new(format!("{call_id}.output"))
}

/// Identity of the record that asks about one call.
fn approval_item_id(call_id: &CallId) -> ItemId {
    ItemId::new(format!("{call_id}.approval"))
}
