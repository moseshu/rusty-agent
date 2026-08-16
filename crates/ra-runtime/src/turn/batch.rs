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
//!    [`tokio::time::timeout`], which panics on a runtime built without `enable_time`. Unlike the
//!    streaming runner's own reaper — a detached task, where losing the timer only loses the abort
//!    backstop — this runs on the settlement path, so the panic reaches the caller. Every cancelled
//!    turn takes this path, not just exotic ones.
//! 2. **The final join after `abort_all` has no deadline.** A task that never reaches an await
//!    point is never aborted, and this function waits for it. That is the deliberate half of the
//!    trade: the alternative is dropping the [`JoinSet`], which detaches the task and leaves its
//!    child processes running — exactly what [`DRAIN_GRACE`] exists to prevent.
//!
//! R3-4b's first property is also still outstanding and cannot be met at this seam: dispatching
//! each call as the response *streams* — which overlaps tool execution with token generation — has
//! to happen where the stream is read, and [`execute_actions`] is handed a response that is
//! already complete.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use ra_core::{
    agent::ToolUseResult,
    cancel::{CancelReason, CancelScope, DRAIN_GRACE, ScopeKind},
    context::RunContext,
    error::{Error, Result, ToolErrorKind},
    item::{AgentId, CallId, ItemId, RunItem, RunItemKind, ToolCallOutput},
    state::{ToolFailureTracker, ToolOutcome, ToolUse, ToolUseTracker},
    step::{ProcessedResponse, ToolRunFunction},
    tool::{ResourceClaim, ResourceId, ToolConcurrency, ToolOrigin, ToolServices},
    trace::SpanKind,
};
use serde_json::{Value, json};
use tokio::{
    sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, Semaphore},
    task::{Id, JoinError, JoinSet},
};
use tracing::{Instrument, error, info_span, warn};

use crate::tool::dispatch::{
    CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool_with_admission, duration_ms,
};

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
    tool_results: Vec<ToolUseResult>,
    outcomes: Vec<ToolOutcome>,
}

/// One completed task, retained until the whole batch is known to be safe to settle.
struct CompletedDispatch {
    order: usize,
    call_id: CallId,
    tool: ToolOrigin,
    dispatch: ToolDispatch,
}

/// Result produced by the supervised task for one call.
struct DispatchTaskResult {
    order: usize,
    call_id: CallId,
    tool: ToolOrigin,
    result: Result<ToolDispatch>,
}

/// The error classes used to choose one batch-level outcome when several calls fail together.
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

    /// Function-tool results, in the response's model order.
    ///
    /// The structured input a tool-use stop policy reads, and **narrower than `new_items`** on
    /// purpose: it holds only calls that ran and produced a value. See [`settle_dispatches`] for
    /// what that leaves out and why.
    #[must_use]
    pub fn tool_results(&self) -> &[ToolUseResult] {
        &self.tool_results
    }

    /// How each answered call turned out, in the response's model order.
    ///
    /// Three things distinguish this from `new_items`. A call still awaiting approval is absent —
    /// it has not turned out any way yet. A call the chain refused is present *as a refusal*, which
    /// says nothing about the tool and clears whatever streak was running, rather than as a
    /// failure. And a call whose tool shaped its own failure text is present **as a failure**,
    /// which is the classification the rendered result deliberately does not carry.
    #[must_use]
    pub fn outcomes(&self) -> &[ToolOutcome] {
        &self.outcomes
    }
}

/// Inputs for answering one classified response.
#[must_use]
#[non_exhaustive]
pub struct TurnExecutionRequest<'a> {
    processed: &'a ProcessedResponse,
    agent_id: &'a AgentId,
    tool_use: &'a ToolUseTracker,
    tool_failure: &'a ToolFailureTracker,
    run: Arc<RunContext>,
    cancel: &'a CancelScope,
    services: ToolServices,
    max_function_tool_concurrency: usize,
}

impl<'a> TurnExecutionRequest<'a> {
    /// Creates a request.
    pub fn new(
        processed: &'a ProcessedResponse,
        agent_id: &'a AgentId,
        tool_use: &'a ToolUseTracker,
        tool_failure: &'a ToolFailureTracker,
        run: Arc<RunContext>,
        cancel: &'a CancelScope,
    ) -> Self {
        Self {
            processed,
            agent_id,
            tool_use,
            tool_failure,
            run,
            cancel,
            services: ToolServices::new(),
            max_function_tool_concurrency: DEFAULT_MAX_FUNCTION_TOOL_CONCURRENCY,
        }
    }

    /// Sets the framework ports every tool in this batch is handed.
    pub fn with_services(mut self, services: ToolServices) -> Self {
        self.services = services;
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

    let (mut dispatches, mut task_orders, tool_scopes) = spawn_function_dispatches(&request);

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
    settle_dispatches(collected, processed, &mut execution)?;

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

/// Coordinates concurrency permits across global and per-resource constraints.
#[derive(Clone)]
pub(crate) struct ResourceAdmissionGate {
    global_gate: Arc<RwLock<()>>,
    resource_locks: Arc<Mutex<HashMap<ResourceId, Arc<RwLock<()>>>>>,
}

impl ResourceAdmissionGate {
    fn new() -> Self {
        Self {
            global_gate: Arc::new(RwLock::new(())),
            resource_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn get_or_create_lock(&self, resource: &ResourceId) -> Arc<RwLock<()>> {
        let mut locks = self
            .resource_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks
            .entry(resource.clone())
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }
}

/// An acquired read or write permit on an `RwLock`.
pub(crate) enum ResourcePermit {
    Read { _guard: OwnedRwLockReadGuard<()> },
    Write { _guard: OwnedRwLockWriteGuard<()> },
}

/// Holds all acquired permits for a tool execution so they release on drop.
///
/// In Rust, struct fields drop in declaration order.
pub(crate) struct AdmissionPermits {
    _global: ResourcePermit,
    _resources: Vec<ResourcePermit>,
}

impl ResourceAdmissionGate {
    /// Acquires global write or global read + fine-grained resource locks.
    pub(crate) async fn acquire_permits(
        &self,
        cancel: &CancelScope,
        concurrency: ToolConcurrency,
        claims: &[ResourceClaim],
    ) -> Result<AdmissionPermits> {
        if matches!(concurrency, ToolConcurrency::Exclusive) {
            // Exclusive tools require a global write lock to run alone.
            let global_guard = cancel.run(self.global_gate.clone().write_owned()).await?;
            Ok(AdmissionPermits {
                _global: ResourcePermit::Write {
                    _guard: global_guard,
                },
                _resources: Vec::new(),
            })
        } else {
            // Parallel tools acquire a global read lock and per-resource locks if claims exist.
            let global_guard = cancel.run(self.global_gate.clone().read_owned()).await?;
            if claims.is_empty() {
                Ok(AdmissionPermits {
                    _global: ResourcePermit::Read {
                        _guard: global_guard,
                    },
                    _resources: Vec::new(),
                })
            } else {
                // Sort claims deterministically by ResourceId to prevent AB-BA deadlocks.
                let deduplicated = ResourceClaim::deduplicate(claims.iter().cloned());
                let mut resource_permits = Vec::with_capacity(deduplicated.len());
                for claim in deduplicated {
                    let lock = self.get_or_create_lock(claim.resource());
                    if claim.is_exclusive() {
                        let guard = cancel.run(lock.write_owned()).await?;
                        resource_permits.push(ResourcePermit::Write { _guard: guard });
                    } else {
                        let guard = cancel.run(lock.read_owned()).await?;
                        resource_permits.push(ResourcePermit::Read { _guard: guard });
                    }
                }
                Ok(AdmissionPermits {
                    _global: ResourcePermit::Read {
                        _guard: global_guard,
                    },
                    _resources: resource_permits,
                })
            }
        }
    }
}

/// Spawns the response's function calls and returns their supervisor state.
fn spawn_function_dispatches(
    request: &TurnExecutionRequest<'_>,
) -> (
    JoinSet<DispatchTaskResult>,
    HashMap<Id, usize>,
    Vec<CancelScope>,
) {
    let gate = ResourceAdmissionGate::new();
    let slots = Arc::new(Semaphore::new(request.max_function_tool_concurrency));
    let mut dispatches = JoinSet::new();
    let mut task_orders = HashMap::new();
    let mut tool_scopes = Vec::new();
    for (order, action) in request.processed.functions().iter().enumerate() {
        // Asked of the action, never rebuilt from its parts: settlement recorded this turn under
        // `identity()` a moment ago, and a second derivation that drifted would look up something
        // nothing ever recorded and hand the breaker a permanent zero.
        let identity = action.identity();
        // Admission reads the run's records as they stood when this response arrived; see
        // `circuit::admit_progress` for what that means for several calls in one response.
        let history = CallHistory::new(
            request.tool_use.repeat_streak(request.agent_id, &identity),
            request
                .tool_failure
                .no_progress_streak(request.agent_id, &identity),
        );
        let tool_scope = request.cancel.child(ScopeKind::Tool);
        let dispatch_request = ToolDispatchRequest::new(
            Arc::clone(action.tool()),
            action.call_id().clone(),
            action.call().arguments().clone(),
            Arc::clone(&request.run),
            tool_scope.clone(),
            history,
        )
        .with_services(request.services.clone());
        let gate = gate.clone();
        let slots = Arc::clone(&slots);
        let call_id = action.call_id().clone();
        let tool = action.tool().origin().clone();
        let cancel = tool_scope;
        tool_scopes.push(cancel.clone());
        // Built here rather than inside the spawned task, and this is the whole reason the
        // helper exists: `tokio` does not carry the tracing context across a spawn, so a span
        // created in the task body finds an empty span stack and becomes a root. Every tool call
        // would then sit outside the run that made it, which is precisely the attribution this
        // span is for. Created on the caller's side, it takes the enclosing agent span as parent.
        let function_span = function_span(&tool, &call_id);
        let task_id = spawn_dispatch_task(
            &mut dispatches,
            order,
            call_id,
            tool,
            cancel,
            gate,
            slots,
            dispatch_request,
            function_span,
        );
        task_orders.insert(task_id, order);
    }
    (dispatches, task_orders, tool_scopes)
}

/// Creates one function span under whatever span the caller is running in.
fn function_span(tool: &ToolOrigin, call_id: &CallId) -> tracing::Span {
    info_span!(
        "function",
        span.kind = SpanKind::Function.label(),
        tool.name = %tool.qualified_name(),
        tool.call_id = %call_id,
        outcome = tracing::field::Empty,
        error.code = tracing::field::Empty,
        cancel.reason = tracing::field::Empty,
        cancel.scope = tracing::field::Empty,
        duration.ms = tracing::field::Empty,
        tool.admission_wait_ms = tracing::field::Empty,
        tool.execution_ms = tracing::field::Empty,
    )
}

/// Spawns one cancellation-aware dispatch chain and returns its supervisor identity.
#[allow(clippy::too_many_arguments)]
fn spawn_dispatch_task(
    dispatches: &mut JoinSet<DispatchTaskResult>,
    order: usize,
    call_id: CallId,
    tool: ToolOrigin,
    cancel: CancelScope,
    gate: ResourceAdmissionGate,
    slots: Arc<Semaphore>,
    dispatch_request: ToolDispatchRequest,
    function_span: tracing::Span,
) -> Id {
    let task_span = function_span.clone();
    dispatches
        .spawn(
            async move {
                let started = Instant::now();
                // 1. Acquire batch concurrency slot permit before entering third-party code.
                // This bounds peak concurrent needs_approval(), resource_claims(), and call()
                // invocations to `max_function_tool_concurrency`.
                let slot_result = cancel.run(slots.acquire_owned()).await.and_then(|res| {
                    res.map_err(|_| Error::caller("the function-tool concurrency semaphore closed"))
                });

                let result = match slot_result {
                    Ok(_slot_permit) => {
                        dispatch_tool_with_admission(dispatch_request, Some(&gate), started).await
                    }
                    Err(error) => Err(error),
                };

                function_span.record(
                    ra_core::trace::field::DURATION_MS,
                    duration_ms(started.elapsed()),
                );
                record_function_outcome(&function_span, &result, &cancel);
                DispatchTaskResult {
                    order,
                    call_id,
                    tool,
                    result,
                }
            }
            .instrument(task_span),
        )
        .id()
}

/// Records the terminal result of one function-tool dispatch without leaking its payload.
fn record_function_outcome(
    span: &tracing::Span,
    result: &Result<ToolDispatch>,
    cancel: &CancelScope,
) {
    match result {
        Ok(ToolDispatch::Observed(observation)) => match observation.failure_code() {
            Some(code) => {
                span.record(ra_core::trace::field::ERROR_CODE, code);
                ra_core::trace::record_outcome(span, ra_core::trace::SpanOutcome::Error);
            }
            None => ra_core::trace::record_outcome(span, ra_core::trace::SpanOutcome::Ok),
        },
        Ok(ToolDispatch::Refused(refusal)) => {
            span.record(ra_core::trace::field::ERROR_CODE, refusal.code());
            ra_core::trace::record_outcome(span, ra_core::trace::SpanOutcome::Error);
        }
        // Suspended, not finished: the handler never ran, so `tool.execution_ms` is absent and the
        // duration is the wait for a decision. Recorded as `ok` because nothing failed — a report
        // of tool latency has to filter on the execution field being present, not assume it.
        Ok(ToolDispatch::AwaitingApproval(_)) => {
            ra_core::trace::record_outcome(span, ra_core::trace::SpanOutcome::Ok);
        }
        // The tool's own scope, so a tool that timed itself out is attributed to `tool` and one
        // killed by the run's deadline to `run`, which is the distinction that says whether the
        // ceiling that fired was the tool's or the run's.
        Err(error) => match (
            error.is_cancelled(),
            cancel.reason(),
            cancel.cancelled_scope(),
        ) {
            (true, Some(reason), Some(kind)) => {
                span.record(ra_core::trace::field::ERROR_CODE, error.code());
                ra_core::trace::record_cancel(span, &reason, &kind);
            }
            _ => ra_core::trace::record_error(span, error),
        },
    }
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
                    tool: task.tool,
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

/// The failure arbitration table. Higher priority wins; equal classes retain model order.
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
///
/// # Why an observation is not automatically a result
///
/// Every observation is answered to the model; only some become a [`ToolUseResult`] that a
/// tool-use stop policy may promote to the run's outcome. The dividing line is `is_error`, and it is the
/// invariant those policies rest on — **a result is a value a tool successfully produced**.
/// `StopOnFirstTool` and `StopAtTools` cannot inspect what they stop on, so anything else would let
/// a run report [`FinishReason::ToolStop`](ra_core::finish::FinishReason::ToolStop), whose
/// `is_complete()` is true, over something that went wrong.
///
/// Two answers look like results and are not:
///
/// - **A call the chain refused before running it**, which is [`ToolDispatch::Refused`] and so is
///   excluded by the match rather than by inspecting what it rendered. `execute_actions` keeps the
///   other half of that same story, a name the turn never advertised, out of `tool_results` too.
///   Reporting a completed run over a tool that never executed is the sharper version of the
///   problem.
/// - **A call that ran and failed** under `ToolFailureHandling::ModelVisible`. The model has not
///   even read the failure yet, which is precisely why settlement owes it another turn.
///
/// A tool that handled its own failure through `ToolFailureHandling::Custom` *does* produce a
/// result: it returned a value it means the model to act on, and it wrote that value itself.
///
/// # Why the outcome record is built here and not from the items
///
/// The no-progress records need the identity, the arguments, the result, and whether the call
/// failed. The first two come from the action the response bound, the last two from the dispatch —
/// and only here are all four in scope at once. Rebuilding any of them later would mean reading a
/// classification back out of a rendered item, which is exactly the derivation
/// [`ToolObservation`](crate::tool::dispatch::ToolObservation) exists to make unnecessary.
fn settle_dispatches(
    mut collected: CollectedDispatches,
    processed: &ProcessedResponse,
    execution: &mut TurnExecution,
) -> Result<()> {
    if let Some(failure) = collected.failure {
        return Err(failure.error);
    }

    collected.completed.sort_by_key(|completed| completed.order);
    for completed in collected.completed {
        match completed.dispatch {
            ToolDispatch::Observed(observation) => {
                // `order` indexes the same list the dispatch was spawned from, so the action it
                // names is the one this observation answers.
                if let Some(action) = processed.functions().get(completed.order) {
                    let result = observation.output().output();
                    execution.outcomes.push(match observation.failure_code() {
                        None => outcome(action, ToolOutcome::succeeded, result),
                        Some(code) => ToolOutcome::failed(
                            action.identity(),
                            action.call_id().clone(),
                            action.call().arguments(),
                            result,
                            code,
                        ),
                    });
                }
                let output = observation.into_output();
                if !output.is_error() {
                    execution
                        .tool_results
                        .push(ToolUseResult::new(completed.tool, output.clone()));
                }
                execution
                    .new_items
                    .push(output_item(&completed.call_id, output));
            }
            ToolDispatch::Refused(refusal) => {
                if let Some(action) = processed.functions().get(completed.order) {
                    execution.outcomes.push(outcome(
                        action,
                        ToolOutcome::refused,
                        refusal.output().output(),
                    ));
                }
                // Never a `ToolUseResult`: a stop policy cannot inspect what it stops on, and a run
                // reporting `FinishReason::ToolStop` over a call that never ran would be reporting
                // that the agent reached its own conclusion.
                execution
                    .new_items
                    .push(output_item(&completed.call_id, refusal.into_output()));
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

/// Builds the record of how one call turned out.
///
/// The identity is asked of the action rather than rebuilt from its parts, for the reason the
/// attempt trail is: settlement recorded this turn under `identity()`, and a second derivation that
/// drifted would file the outcome under something nothing ever counted, leaving the breaker on a
/// permanent zero.
fn outcome(
    action: &ToolRunFunction,
    build: fn(ToolUse, CallId, &Value, &Value) -> ToolOutcome,
    result: &Value,
) -> ToolOutcome {
    build(
        action.identity(),
        action.call_id().clone(),
        action.call().arguments(),
        result,
    )
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
