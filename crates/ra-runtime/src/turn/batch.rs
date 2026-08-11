//! One thought -> a batch of actions -> a batch of observations (R3-4).
//!
//! Every bound action the response produced is answered here, and **every one of them is answered**:
//! a call that runs, a call that needs a human, and a call that resolved to nothing all leave with
//! a record paired to their `call_id`. That is the property the next request depends on — a tool
//! call with no output makes the history malformed, and no provider accepts it.
//!
//! Parallel-declared calls share a read permit; an exclusive call takes the matching write permit.
//! The ordered collector starts every action concurrently while still recording outcomes in the
//! order the model supplied them, so a faster later read can never reorder call/output pairs.
//!
//! # What this is not yet
//!
//! This is R3-4b's gate and ordering, not the whole loop shape, and **R3-4c still owns how a batch
//! of concurrent calls is collected and cancelled**. Three of its properties are absent here, and
//! each is absent in a way that is currently invisible:
//!
//! * **Failure selection.** A propagating failure is the first one *in model order*, because that
//!   is what `FuturesOrdered` yields first. R3-4c selects by class instead, so the reason a turn
//!   stopped is the most informative one rather than the leftmost one.
//! * **Late failures.** A failure that arrives after another has already propagated is dropped
//!   with the rest of the batch. R3-4c merges it.
//! * **Cancellation drain.** Propagating out of the loop below drops the remaining futures, which
//!   cancels each at its next await point with no chance to finish teardown. Every tool here is a
//!   read, so today that costs nothing; a tool that owns a child process or a partially written
//!   file needs the drain, and R3-4c is where it goes.
//!
//! R3-4b's first property is also still outstanding and cannot be met at this seam: dispatching
//! each call as the response *streams* — which overlaps tool execution with token generation — has
//! to happen where the stream is read, and [`execute_actions`] is handed a response that is
//! already complete.

use std::sync::Arc;

use futures::{StreamExt as _, stream::FuturesOrdered};
use ra_core::{
    cancel::CancelScope,
    error::{Error, Result, ToolErrorKind},
    item::{AgentId, CallId, ItemId, RunItem, RunItemKind, ToolCallOutput},
    state::{ToolUseTracker, WorkStateHandle},
    step::ProcessedResponse,
    tool::{ToolConcurrency, ToolRuntimeContext},
};
use serde_json::json;
use tokio::sync::{RwLock, Semaphore};

use crate::tool::dispatch::{ToolDispatch, ToolDispatchRequest, dispatch_tool};

/// Default cap for all function-tool dispatch chains in one model response.
///
/// Eight keeps the default comfortably below typical process and connection limits while still
/// allowing independent reads, searches, and MCP calls to overlap. Hosts that know their resource
/// budget can set a different value through [`RunConfig`](crate::runner::RunConfig).
pub(crate) const DEFAULT_MAX_FUNCTION_TOOL_CONCURRENCY: usize = 8;

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
    work_state: Option<&'a Arc<dyn WorkStateHandle>>,
    max_function_tool_concurrency: usize,
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
            work_state: None,
            max_function_tool_concurrency: DEFAULT_MAX_FUNCTION_TOOL_CONCURRENCY,
        }
    }

    /// Sets the task state every tool in this batch is handed (R3-13).
    pub const fn with_work_state(mut self, work_state: &'a Arc<dyn WorkStateHandle>) -> Self {
        self.work_state = Some(work_state);
        self
    }

    /// Sets the cap for concurrently dispatched function tools in this response.
    pub const fn with_max_function_tool_concurrency(mut self, max: usize) -> Self {
        self.max_function_tool_concurrency = max;
        self
    }
}

/// Answers every action the response bound.
pub async fn execute_actions(request: TurnExecutionRequest<'_>) -> Result<TurnExecution> {
    let processed = request.processed;
    let mut execution = TurnExecution::default();

    if request.max_function_tool_concurrency == 0 {
        return Err(Error::config(
            "`max_function_tool_concurrency` must be at least 1; zero would permanently queue \
             every tool call",
        ));
    }

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

    let gate = Arc::new(RwLock::new(()));
    let slots = Arc::new(Semaphore::new(request.max_function_tool_concurrency));
    let mut dispatches = FuturesOrdered::new();
    for action in processed.functions() {
        request.cancel.ensure_not_cancelled()?;
        // Asked of the action, never rebuilt from its parts: settlement recorded this turn under
        // `identity()` a moment ago, and a second derivation that drifted would look up something
        // nothing ever recorded and hand the breaker a permanent zero.
        let repeat_streak = request
            .tool_use
            .repeat_streak(request.agent_id, &action.identity());
        let mut dispatch_request = ToolDispatchRequest::new(
            action.tool(),
            action.call_id(),
            action.call().arguments(),
            request.context,
            request.cancel,
            repeat_streak,
        );
        if let Some(work_state) = request.work_state {
            dispatch_request = dispatch_request.with_work_state(work_state);
        }
        let gate = Arc::clone(&gate);
        let slots = Arc::clone(&slots);
        let concurrency = action.tool().options().concurrency();
        let call_id = action.call_id();
        let cancel = request.cancel;
        dispatches.push_back(async move {
            cancel.ensure_not_cancelled()?;
            // Take one total-dispatch permit before any per-tool gate.  The permit covers the
            // entire common chain (approval, guardrails, and tool call), which is exactly the
            // resource footprint a host needs to bound.  Waiting is cancellation-aware: dropping
            // a cancelled future returns its permit and cannot strand a queued batch.
            let _slot = cancel
                .run(slots.acquire_owned())
                .await?
                .map_err(|_| Error::caller("the function-tool concurrency semaphore closed"))?;
            // The guard must live across the complete common dispatch chain.  Taking it around
            // only `Tool::call` would let approval/guardrail code for a writer race a reader and
            // make the declaration mean something different for different execution stages.
            //
            // Only `Parallel` shares. `Exclusive` and every variant this executor cannot read yet
            // take the write permit: `ToolConcurrency` is `#[non_exhaustive]`, so some fallback is
            // mandatory, and this one matches the field's own default — a declaration nobody here
            // understands has not said the tool tolerates company. Refusing the call instead would
            // fail the whole batch, since one propagating error drops every other call in it, over
            // a field that does nothing but pick a permit.
            let dispatch = if matches!(concurrency, ToolConcurrency::Parallel) {
                let _permit = cancel.run(gate.read()).await?;
                cancel.ensure_not_cancelled()?;
                dispatch_tool(dispatch_request).await
            } else {
                let _permit = cancel.run(gate.write()).await?;
                cancel.ensure_not_cancelled()?;
                dispatch_tool(dispatch_request).await
            }?;
            Ok::<_, Error>((call_id, dispatch))
        });
    }

    // `FuturesOrdered` polls all submitted actions but yields them in response order.  Thus a
    // propagating failure is still the first failing action in model order, matching the old
    // sequential failure contract without throwing away useful parallelism before it.
    while let Some(result) = dispatches.next().await {
        let (call_id, dispatch) = result?;
        match dispatch {
            ToolDispatch::Observed(output) => {
                execution.new_items.push(output_item(call_id, output));
            }
            ToolDispatch::AwaitingApproval(approval) => {
                let item = RunItem::new(
                    approval_item_id(call_id),
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
