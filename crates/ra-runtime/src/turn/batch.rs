//! One thought -> a batch of actions -> a batch of observations (R3-4).
//!
//! Every bound action the response produced is answered here, and **every one of them is answered**:
//! a call that runs, a call that needs a human, and a call that resolved to nothing all leave with
//! a record paired to their `call_id`. That is the property the next request depends on — a tool
//! call with no output makes the history malformed, and no provider accepts it.
//!
//! Parallel-declared calls share a read permit; an exclusive call takes the matching write permit.
//! A [`JoinSet`](tokio::task::JoinSet) supervises every dispatched action until it has reached a
//! terminal state. Collection is deliberately separate from settlement: an error or cancellation
//! can never leave half a batch written into [`TurnExecution`].
//!
//! A propagating failure cancels every sibling's child [`CancelScope`]. The collector then drains
//! the tasks for [`DRAIN_GRACE`], merging failures that win the race with cancellation. A task
//! that does not cooperate with cancellation is aborted only after that grace period, and a panic
//! or unexpected cleanup error is recorded through `tracing` rather than being silently lost.
//!
//! # What the drain costs the caller
//!
//! Two consequences of [`DRAIN_GRACE`] belong to whoever calls [`execute_actions`], because both
//! are visible from outside:
//!
//! 1. **The drain needs a time driver.** It measures the grace period with
//!    [`tokio::time::timeout`], which panics on a runtime built without `enable_time`. Unlike
//!    R3-7's stream reaper — a detached task, where losing the timer only loses the abort backstop
//!    — this runs on the settlement path, so the panic reaches the caller. Every cancelled turn
//!    takes this path, not just exotic ones.
//! 2. **The final join after `abort_all` has no deadline.** A task that never reaches an await
//!    point is never aborted, and this function waits for it. That is the deliberate half of the
//!    trade: the alternative is dropping the [`JoinSet`], which detaches the task and leaves its
//!    child processes running — exactly what [`DRAIN_GRACE`] exists to prevent.
//!
//! R3-4b's first property is also still outstanding and cannot be met at this seam: dispatching
//! each call as the response *streams* — which overlaps tool execution with token generation — has
//! to happen where the stream is read, and [`execute_actions`] is handed a response that is
//! already complete.

use std::{collections::HashMap, sync::Arc};

use ra_core::{
    cancel::{CancelReason, CancelScope, DRAIN_GRACE, ScopeKind},
    error::{Error, Result, ToolErrorKind},
    item::{AgentId, CallId, ItemId, RunItem, RunItemKind, ToolCallOutput},
    state::{ToolUseTracker, WorkStateHandle},
    step::ProcessedResponse,
    tool::{ToolConcurrency, ToolRuntimeContext},
};
use serde_json::json;
use tokio::{
    sync::{RwLock, Semaphore},
    task::{Id, JoinError, JoinSet},
};
use tracing::{error, warn};

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

/// One completed task, retained until the whole batch is known to be safe to settle.
struct CompletedDispatch {
    order: usize,
    call_id: CallId,
    dispatch: ToolDispatch,
}

/// Result produced by the supervised task for one call.
struct DispatchTaskResult {
    order: usize,
    call_id: CallId,
    result: Result<ToolDispatch>,
}

/// The error classes R3-4c uses to choose one batch-level outcome.
///
/// `Cancelled` is above the four failure classes because it is a terminal control-flow outcome,
/// not a failure observation that a competing tool result may overwrite.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FailurePriority {
    Other,
    ToolTimeout,
    GuardrailTripwire,
    UserError,
    Cancelled,
}

/// A failure together with the model order used to break equal-priority ties.
struct RankedFailure {
    error: Error,
    order: usize,
    priority: FailurePriority,
}

/// The output of collection, before any model-visible record is committed.
#[derive(Default)]
struct CollectedDispatches {
    completed: Vec<CompletedDispatch>,
    failure: Option<RankedFailure>,
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
    context: Arc<dyn ToolRuntimeContext>,
    cancel: &'a CancelScope,
    work_state: Option<Arc<dyn WorkStateHandle>>,
    max_function_tool_concurrency: usize,
}

impl<'a> TurnExecutionRequest<'a> {
    /// Creates a request.
    pub fn new(
        processed: &'a ProcessedResponse,
        agent_id: &'a AgentId,
        tool_use: &'a ToolUseTracker,
        context: Arc<dyn ToolRuntimeContext>,
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
    pub fn with_work_state(mut self, work_state: Arc<dyn WorkStateHandle>) -> Self {
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
    let mut dispatches = JoinSet::new();
    let mut task_orders = HashMap::new();
    let mut tool_scopes = Vec::new();
    for (order, action) in processed.functions().iter().enumerate() {
        // Asked of the action, never rebuilt from its parts: settlement recorded this turn under
        // `identity()` a moment ago, and a second derivation that drifted would look up something
        // nothing ever recorded and hand the breaker a permanent zero.
        let repeat_streak = request
            .tool_use
            .repeat_streak(request.agent_id, &action.identity());
        let tool_scope = request.cancel.child(ScopeKind::Tool);
        let mut dispatch_request = ToolDispatchRequest::new(
            Arc::clone(action.tool()),
            action.call_id().clone(),
            action.call().arguments().clone(),
            Arc::clone(&request.context),
            tool_scope.clone(),
            repeat_streak,
        );
        if let Some(work_state) = &request.work_state {
            dispatch_request = dispatch_request.with_work_state(Arc::clone(work_state));
        }
        let gate = Arc::clone(&gate);
        let slots = Arc::clone(&slots);
        let concurrency = action.tool().options().concurrency();
        let call_id = action.call_id().clone();
        let cancel = tool_scope;
        tool_scopes.push(cancel.clone());
        let handle = dispatches.spawn(async move {
            let result = async {
                cancel.ensure_not_cancelled()?;
                // Take one total-dispatch permit before any per-tool gate.  The permit covers the
                // entire common chain (approval, guardrails, and tool call), which is exactly the
                // resource footprint a host needs to bound. Waiting is cancellation-aware: dropping
                // a cancelled future returns its permit and cannot strand a queued batch.
                let _slot = cancel
                    .run(slots.acquire_owned())
                    .await?
                    .map_err(|_| Error::caller("the function-tool concurrency semaphore closed"))?;
                // The guard must live across the complete common dispatch chain. Taking it around
                // only `Tool::call` would let approval/guardrail code for a writer race a reader and
                // make the declaration mean something different for different execution stages.
                //
                // Only `Parallel` shares. `Exclusive` and every variant this executor cannot read yet
                // take the write permit: `ToolConcurrency` is `#[non_exhaustive]`, so some fallback is
                // mandatory, and this one matches the field's own default — a declaration nobody here
                // understands has not said the tool tolerates company. Refusing the call instead would
                // fail the whole batch, since one propagating error cancels every other call in it, over
                // a field that does nothing but pick a permit.
                if matches!(concurrency, ToolConcurrency::Parallel) {
                    let _permit = cancel.run(gate.read()).await?;
                    cancel.ensure_not_cancelled()?;
                    dispatch_tool(dispatch_request).await
                } else {
                    let _permit = cancel.run(gate.write()).await?;
                    cancel.ensure_not_cancelled()?;
                    dispatch_tool(dispatch_request).await
                }
            }
            .await;
            DispatchTaskResult {
                order,
                call_id,
                result,
            }
        });
        task_orders.insert(handle.id(), order);
    }

    let collected = collect_dispatches(
        &mut dispatches,
        &mut task_orders,
        &tool_scopes,
        request.cancel,
    )
    .await;

    // Not redundant with the entry check: collection awaits, and a cancellation that arrives while
    // the last dispatch is completing can lose that race and leave the scope cancelled here.
    //
    // Its position is the contract, not a formality. Ahead of `settle_dispatches` it gives parent
    // cancellation priority over an action result — or a tool failure — that happened to complete
    // in the same scheduling turn, which is what stops a cancelled turn from reporting the last
    // thing that went wrong instead of reporting that it was cancelled. And `collect_dispatches`
    // has already driven every spawned task to a terminal state before this line, so returning
    // here can neither detach a tool task nor leave half a batch written.
    request.cancel.ensure_not_cancelled()?;
    settle_dispatches(collected, &mut execution)?;

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

/// Collect every task result before mutating the turn's records.
///
/// The first terminal dispatch result stops further useful work, but it does not stop
/// supervision. All sibling scopes receive `PeerFailure`, then every task is driven to a terminal
/// state before this function returns. That distinction is what prevents a later failure from
/// being silently dropped and prevents a cancelled task from escaping its owning turn.
async fn collect_dispatches(
    dispatches: &mut JoinSet<DispatchTaskResult>,
    task_orders: &mut HashMap<Id, usize>,
    tool_scopes: &[CancelScope],
    parent: &CancelScope,
) -> CollectedDispatches {
    let mut collected = CollectedDispatches::default();

    while !dispatches.is_empty() && !parent.is_cancelled() {
        let Some(joined) = (tokio::select! {
            () = parent.cancelled() => None,
            joined = dispatches.join_next_with_id() => joined,
        }) else {
            break;
        };
        record_task_result(joined, task_orders, &mut collected, false, "direct");

        // `JoinSet` is completion-ordered. Consume everything already ready before signalling
        // siblings so failures that raced in the same scheduler turn all get the documented
        // priority arbitration rather than whichever task happened to be polled first.
        while let Some(joined) = dispatches.try_join_next_with_id() {
            record_task_result(joined, task_orders, &mut collected, false, "direct");
        }

        if collected.failure.is_some() {
            cancel_tool_scopes(tool_scopes);
            break;
        }
    }

    if parent.is_cancelled() || collected.failure.is_some() {
        drain_dispatches(dispatches, task_orders, &mut collected).await;
    }

    collected
}

/// Cancels all tool children without changing the parent turn's reason.
fn cancel_tool_scopes(tool_scopes: &[CancelScope]) {
    for scope in tool_scopes {
        scope.cancel(CancelReason::PeerFailure);
    }
}

/// Gives cancelled tools their contractual grace period, then explicitly aborts only the tasks
/// that did not cooperate. `JoinSet` remains owned until the final join, so dropping this function
/// never detaches a background tool task.
///
/// # Panics
///
/// Panics when the host runtime was built without a time driver; see the module documentation.
async fn drain_dispatches(
    dispatches: &mut JoinSet<DispatchTaskResult>,
    task_orders: &mut HashMap<Id, usize>,
    collected: &mut CollectedDispatches,
) {
    let drained = tokio::time::timeout(DRAIN_GRACE, async {
        while let Some(joined) = dispatches.join_next_with_id().await {
            // Once cancellation has been sent, cancellation results are acknowledgements of the
            // teardown protocol, not competing turn outcomes. Non-cancellation errors still
            // participate in arbitration as late failures.
            //
            // That second sentence is not hypothetical, and the branch it describes is not dead
            // code someone should tidy away. Two things still arrive here after cancellation was
            // sent: a task that died while cleaning up, which lands as a `JoinError`; and a call
            // whose own result became available in the same wake-up that delivered the
            // cancellation — `CancelScope::run` polls the call before it looks at the cancellation,
            // so a ready call wins and its failure is a real outcome, not teardown noise.
            record_task_result(joined, task_orders, collected, true, "cancelled_teardown");
        }
    })
    .await;

    if drained.is_ok() {
        return;
    }

    warn!(
        remaining_tasks = dispatches.len(),
        grace_ms = DRAIN_GRACE.as_millis(),
        "function-tool cancellation drain exceeded its grace period; aborting remaining tasks"
    );
    dispatches.abort_all();
    // Deliberately without a second deadline: an abort only lands at an await point, so a task
    // that never reaches one is waited for rather than abandoned. Giving up here would mean
    // dropping the `JoinSet`, which detaches the task and leaks whatever it spawned.
    while let Some(joined) = dispatches.join_next_with_id().await {
        // These are expected `JoinError::is_cancelled()` results from the explicit abort. A panic
        // is still reported and merged, because it is cleanup work failing rather than the abort
        // itself doing what it was told to do.
        record_task_result(joined, task_orders, collected, true, "forced_teardown");
    }
}

/// Records one joined task. It is shared by normal collection and drain so the latter cannot lose
/// a failure merely because a different tool caused the batch to begin shutting down.
fn record_task_result(
    joined: std::result::Result<(Id, DispatchTaskResult), JoinError>,
    task_orders: &mut HashMap<Id, usize>,
    collected: &mut CollectedDispatches,
    ignore_cancellation: bool,
    source: &'static str,
) {
    match joined {
        Ok((id, task)) => {
            let recorded_order = task_orders.remove(&id);
            debug_assert_eq!(recorded_order, Some(task.order));
            match task.result {
                Ok(dispatch) => collected.completed.push(CompletedDispatch {
                    order: task.order,
                    call_id: task.call_id,
                    dispatch,
                }),
                Err(error) if ignore_cancellation && error.is_cancelled() => {}
                Err(error) => merge_failure(
                    &mut collected.failure,
                    RankedFailure::new(error, task.order),
                    source,
                ),
            }
        }
        Err(join_error) => {
            let id = join_error.id();
            let order = task_orders.remove(&id).unwrap_or(usize::MAX);
            if join_error.is_cancelled() && ignore_cancellation {
                return;
            }

            if join_error.is_panic() {
                error!(
                    task_id = %id,
                    model_order = order,
                    cleanup_stage = source,
                    error = %join_error,
                    "function-tool task panicked during batch collection"
                );
            } else {
                warn!(
                    task_id = %id,
                    model_order = order,
                    cleanup_stage = source,
                    error = %join_error,
                    "function-tool task ended without a dispatch result"
                );
            }
            merge_failure(
                &mut collected.failure,
                RankedFailure::task_failure(join_error, order),
                source,
            );
        }
    }
}

impl RankedFailure {
    fn new(error: Error, order: usize) -> Self {
        Self {
            priority: failure_priority(&error),
            error,
            order,
        }
    }

    fn task_failure(error: JoinError, order: usize) -> Self {
        // A panic or an externally-aborted task is a runtime defect, not the public `UserError`
        // class represented by `Error::Caller`; it must not mask a guardrail or timeout merely
        // because the framework used a caller-facing error container to carry its source.
        Self {
            error: Error::caller("a supervised function-tool task ended before producing a result")
                .with_source(error),
            order,
            priority: FailurePriority::Other,
        }
    }
}

/// The R3-4c failure arbitration table. Higher priority wins; equal classes retain model order.
fn failure_priority(error: &Error) -> FailurePriority {
    if error.is_cancelled() {
        return FailurePriority::Cancelled;
    }
    match error {
        Error::Caller { .. } => FailurePriority::UserError,
        Error::Guardrail { .. } => FailurePriority::GuardrailTripwire,
        Error::Tool {
            kind: ToolErrorKind::Timeout,
            ..
        } => FailurePriority::ToolTimeout,
        _ => FailurePriority::Other,
    }
}

/// Merges a result observed after the batch began closing down. The returned error type has one
/// primary source, so the selected outcome is the highest-priority failure; every losing late
/// failure is nevertheless recorded to tracing instead of disappearing with the cleanup task.
fn merge_failure(
    current: &mut Option<RankedFailure>,
    incoming: RankedFailure,
    source: &'static str,
) {
    let Some(existing) = current.take() else {
        *current = Some(incoming);
        return;
    };

    let incoming_wins = incoming.priority > existing.priority
        || (incoming.priority == existing.priority && incoming.order < existing.order);
    let (winner, loser) = if incoming_wins {
        (incoming, existing)
    } else {
        (existing, incoming)
    };
    warn!(
        selected_code = winner.error.code(),
        selected_order = winner.order,
        ignored_code = loser.error.code(),
        ignored_order = loser.order,
        cleanup_stage = source,
        "merged a concurrent function-tool failure into the batch outcome"
    );
    *current = Some(winner);
}

/// Converts a fully collected batch into records in model order. This is intentionally the only
/// place that mutates `TurnExecution` from tool completions.
fn settle_dispatches(
    mut collected: CollectedDispatches,
    execution: &mut TurnExecution,
) -> Result<()> {
    if let Some(failure) = collected.failure {
        return Err(failure.error);
    }

    collected.completed.sort_by_key(|completed| completed.order);
    for completed in collected.completed {
        match completed.dispatch {
            ToolDispatch::Observed(output) => {
                execution
                    .new_items
                    .push(output_item(&completed.call_id, output));
            }
            ToolDispatch::AwaitingApproval(approval) => {
                let item = RunItem::new(
                    approval_item_id(&completed.call_id),
                    RunItemKind::ToolApproval(approval),
                );
                execution.interruptions.push(item.clone());
                execution.new_items.push(item);
            }
        }
    }
    Ok(())
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
