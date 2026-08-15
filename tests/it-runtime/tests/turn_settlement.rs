//! R3-4 contracts for the fixed order that settles one turn.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope},
    context::RunContext,
    error::{Error, GuardrailStage, Result, ToolErrorKind},
    finish::FinishReason,
    item::{
        CallId, ItemId, McpApprovalRequest, Message, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::ModelHandoffDefinition,
    state::{RunId, ToolFailureTracker, ToolUseTracker},
    step::NextStep,
    tool::{
        Tool, ToolApprovalPolicy, ToolCaller, ToolConcurrency, ToolContext, ToolFailureHandling,
        ToolOptions, ToolOrigin, ToolOutput, ToolSchema, ToolTimeoutBehavior,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    turn::{TurnSettlementRequest, prepare::TurnActionSurface, settle_turn},
};
use serde_json::{Value, json};

/// What a `ScriptedTool` does when it is finally invoked.
enum Behavior {
    Succeed(&'static str),
    Fail,
    Sleep(Duration),
}

struct ScriptedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    behavior: Behavior,
    calls: Arc<AtomicUsize>,
    failures_handled: Arc<AtomicUsize>,
}

impl ScriptedTool {
    fn new(name: &str, behavior: Behavior) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            options: ToolOptions::new(),
            behavior,
            calls: Arc::new(AtomicUsize::new(0)),
            failures_handled: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_options(mut self, options: ToolOptions) -> Self {
        self.options = options;
        self
    }
}

#[async_trait]
impl Tool for ScriptedTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.behavior {
            Behavior::Succeed(text) => Ok(ToolOutput::text(*text)),
            Behavior::Fail => Err(Error::tool(
                ToolErrorKind::ExecutionFailed,
                self.origin.qualified_name(),
                "工具内部失败：路径不存在",
            )),
            Behavior::Sleep(duration) => {
                tokio::time::sleep(*duration).await;
                Ok(ToolOutput::text("late"))
            }
        }
    }

    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        match self.options.approval() {
            ToolApprovalPolicy::Never => Ok(false),
            _ => Ok(true),
        }
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        _error: &Error,
    ) -> Result<Option<ToolOutput>> {
        self.failures_handled.fetch_add(1, Ordering::SeqCst);
        Ok(Some(ToolOutput::text("tool wrote its own explanation")))
    }
}

/// A tool that cannot finish until the test releases the whole batch.
///
/// It makes scheduler overlap observable without depending on elapsed wall-clock time, which is
/// noisy under CI.  If the second parallel call were not polled, its `entered` counter could never
/// reach one while the first is waiting for release.
struct GatedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    concurrency: ToolConcurrency,
    entered: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    completed: Arc<AtomicUsize>,
    output: &'static str,
}

/// Failure classes that must propagate out of a batch and therefore participate in the batch's
/// arbitration rather than becoming a model-visible observation.
#[derive(Clone, Copy)]
enum PropagatingFailure {
    Other,
    User,
    Guardrail,
    Timeout,
}

impl PropagatingFailure {
    fn error(self, tool: &str) -> Error {
        match self {
            Self::Other => Error::tool(
                ToolErrorKind::ExecutionFailed,
                tool,
                "ordinary tool failure",
            ),
            Self::User => Error::caller("the caller supplied an invalid run input"),
            Self::Guardrail => Error::guardrail(
                GuardrailStage::ToolOutput,
                "test",
                "guardrail rejected output",
            ),
            Self::Timeout => Error::tool(ToolErrorKind::Timeout, tool, "the tool timed out"),
        }
    }
}

/// Makes two failures happen at the same synchronization point. This catches the old collector's
/// accidental "first completion wins" behavior without relying on timing.
struct BarrierFailureTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    barrier: Arc<tokio::sync::Barrier>,
    entered: Arc<AtomicUsize>,
    failure: PropagatingFailure,
}

impl BarrierFailureTool {
    fn new(
        name: &str,
        barrier: Arc<tokio::sync::Barrier>,
        entered: Arc<AtomicUsize>,
        failure: PropagatingFailure,
    ) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            barrier,
            entered,
            failure,
        }
    }
}

#[async_trait]
impl Tool for BarrierFailureTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        // Both switches are required, and the timeout one is the easy thing to forget: dispatch
        // routes *any* `ToolErrorKind::Timeout` through `timeout_behavior`, whose default is
        // `ModelVisible`. Declaring only `failure_handling` would turn the timeout arm of this
        // tool into an observation, and the timeout tier of the arbitration table would silently
        // never be exercised by a test that claims to cover it.
        ToolOptions::new()
            .with_concurrency(ToolConcurrency::Parallel)
            .with_failure_handling(ToolFailureHandling::Propagate)
            .with_timeout_behavior(ToolTimeoutBehavior::Propagate)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.barrier.wait().await;
        Err(self.failure.error(self.origin.qualified_name()))
    }
}

/// Fails only once the test releases it, so a peer can be parked first and the teardown can be
/// started on demand.
struct GateThenFailTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    release: Arc<AtomicBool>,
    entered: Arc<AtomicUsize>,
    failure: PropagatingFailure,
}

impl GateThenFailTool {
    fn new(
        name: &str,
        release: Arc<AtomicBool>,
        entered: Arc<AtomicUsize>,
        failure: PropagatingFailure,
    ) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            release,
            entered,
            failure,
        }
    }
}

#[async_trait]
impl Tool for GateThenFailTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new()
            .with_concurrency(ToolConcurrency::Parallel)
            .with_failure_handling(ToolFailureHandling::Propagate)
            .with_timeout_behavior(ToolTimeoutBehavior::Propagate)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        Err(self.failure.error(self.origin.qualified_name()))
    }
}

/// Becomes ready from a flag and **never registers a waker**, so setting the flag schedules
/// nothing.
///
/// This is how the file pins down the one ordering in which a tool's own failure reaches the drain
/// loop. In production that ordering is a race — the call's result lands in the same wake-up as
/// the peer-failure cancellation — and `CancelScope::run` resolves it in the call's favour because
/// `futures::future::select` polls the call before it looks at the cancellation. Nothing here fakes
/// that resolution: the flag only controls *when* the call becomes ready, and the next poll it gets
/// is genuinely the one cancellation triggers.
struct ReadyWithoutWaking(Arc<AtomicBool>);

impl Future for ReadyWithoutWaking {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.0.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// A call whose own propagating failure can only be observed after the batch started tearing down.
struct LateFailureTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    ready: Arc<AtomicBool>,
    entered: Arc<AtomicUsize>,
    failure: PropagatingFailure,
}

impl LateFailureTool {
    fn new(
        name: &str,
        ready: Arc<AtomicBool>,
        entered: Arc<AtomicUsize>,
        failure: PropagatingFailure,
    ) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            ready,
            entered,
            failure,
        }
    }
}

#[async_trait]
impl Tool for LateFailureTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new()
            .with_concurrency(ToolConcurrency::Parallel)
            .with_failure_handling(ToolFailureHandling::Propagate)
            .with_timeout_behavior(ToolTimeoutBehavior::Propagate)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        ReadyWithoutWaking(Arc::clone(&self.ready)).await;
        Err(self.failure.error(self.origin.qualified_name()))
    }
}

/// A call whose supervised *task* dies while being torn down, rather than returning a failure.
///
/// This is the second of the two ways a failure can reach `drain_dispatches`, and it is the one
/// [`LateFailureTool`] cannot reach: it arrives as a `JoinError` instead of as an `Err` the tool
/// returned, so it is ranked by `RankedFailure::task_failure` rather than by the error's own class.
/// Failing from `Drop` is what a real cleanup path does when it cannot finish.
struct PanicOnTeardownTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    barrier: Arc<tokio::sync::Barrier>,
    entered: Arc<AtomicUsize>,
}

impl PanicOnTeardownTool {
    fn new(name: &str, barrier: Arc<tokio::sync::Barrier>, entered: Arc<AtomicUsize>) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            barrier,
            entered,
        }
    }
}

#[async_trait]
impl Tool for PanicOnTeardownTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_concurrency(ToolConcurrency::Parallel)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        struct FailOnDrop;

        impl Drop for FailOnDrop {
            fn drop(&mut self) {
                panic!("cleanup for this call could not complete");
            }
        }

        // Armed before the barrier, not after: the guard then fires even if this call is torn down
        // while still parked, so the test does not depend on getting one more poll first.
        self.entered.fetch_add(1, Ordering::SeqCst);
        let _fail_on_drop = FailOnDrop;
        self.barrier.wait().await;
        std::future::pending::<()>().await;
        unreachable!("a pending test tool can only leave through cancellation")
    }
}

/// Reports when a cancelled `Tool::call` future is actually dropped. Both calls wait forever, so
/// the only legal way this test can finish is for the batch collector to send cancellation and
/// supervise both task teardowns.
struct DropReportingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    entered: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl DropReportingTool {
    fn new(name: &str, entered: Arc<AtomicUsize>, dropped: Arc<AtomicUsize>) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            entered,
            dropped,
        }
    }
}

#[async_trait]
impl Tool for DropReportingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_concurrency(ToolConcurrency::Parallel)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        struct ReportDrop(Arc<AtomicUsize>);

        impl Drop for ReportDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        self.entered.fetch_add(1, Ordering::SeqCst);
        let _report = ReportDrop(Arc::clone(&self.dropped));
        std::future::pending::<()>().await;
        unreachable!("a pending test tool can only leave through cancellation")
    }
}

impl GatedTool {
    fn new(
        name: &str,
        concurrency: ToolConcurrency,
        entered: Arc<AtomicUsize>,
        release: Arc<AtomicBool>,
        completed: Arc<AtomicUsize>,
        output: &'static str,
    ) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            concurrency,
            entered,
            release,
            completed,
            output,
        }
    }
}

#[async_trait]
impl Tool for GatedTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_concurrency(self.concurrency)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text(self.output))
    }
}

/// The binding every settlement here runs under. Nothing prepared an execution instance, so the
/// two identities are the same object; the public/execution split has its own test file.
fn binding() -> AgentBinding {
    AgentBinding::direct(agent())
}

fn agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("main"))
        .name("Main")
        .build()
        .unwrap()
}

/// The live context of the run these settlements belong to.
fn run() -> Arc<RunContext> {
    Arc::new(RunContext::new(RunId::new("run-settlement"), agent()))
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn tool_call(id: &str, call_id: &str, name: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            name,
            json!({ "path": "a.txt" }),
        )),
    )
}

fn message(id: &str, text: &str) -> RunItem {
    item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn surface(tools: Vec<Arc<dyn Tool>>) -> TurnActionSurface {
    TurnActionSurface::new(tools, Vec::new()).unwrap()
}

/// Pulls the paired output payload for one call out of a settled turn.
fn output_for<'a>(items: &'a [RunItem], call_id: &str) -> &'a ra_core::item::ToolCallOutput {
    items
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == call_id => {
                Some(output)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("call `{call_id}` 没有配对输出"))
}

/// Reads a stored payload back as the typed result the tool returned.
///
/// Asserted through the type rather than by indexing the stored JSON: the payload's shape is
/// R2-3's to change, and a test that spelled it out by hand would fail on every change instead of
/// on the ones that alter what the tool actually said.
fn stored_text(output: &ra_core::item::ToolCallOutput) -> Option<String> {
    serde_json::from_value::<ToolOutput>(output.output().clone())
        .ok()?
        .as_text()
        .map(str::to_owned)
}

async fn wait_for_count(counter: &AtomicUsize, count: usize) {
    tokio::time::timeout(Duration::from_millis(250), async {
        while counter.load(Ordering::SeqCst) < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("counter did not reach {count}"));
}

#[tokio::test]
async fn test_turn_settlement_01() {
    let surface = surface(vec![Arc::new(ScriptedTool::new(
        "write_file",
        Behavior::Succeed("ok"),
    ))]);
    let response = ModelResponse::new(vec![message("msg-1", "完事了")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    // 「没要任何动作」是每个 provider 都一样表达的结构事实，不用去读文本判断意图。
    assert!(matches!(
        settled.next_step(),
        NextStep::FinalOutput {
            reason: FinishReason::Final
        }
    ));
    assert_eq!(settled.new_step_items().len(), 1);
    assert_eq!(settled.session_step_items().len(), 1);
}

#[tokio::test]
async fn test_turn_settlement_02() {
    let tool = Arc::new(ScriptedTool::new(
        "write_file",
        Behavior::Succeed("written"),
    ));
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![
        message("msg-1", "我来写一下"),
        tool_call("call-item-1", "call-1", "write_file"),
    ]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(settled.next_step(), NextStep::RunAgain));

    // 模型自己的记录在前，回答它的观察在后——这一轮生成的全部东西按发生顺序排列。
    let ids = settled
        .new_step_items()
        .iter()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["msg-1", "call-item-1", "call-1.output"]);

    let output = output_for(settled.new_step_items(), "call-1");
    assert!(!output.is_error());
    assert_eq!(stored_text(&output), Some("written".to_owned()));
}

#[tokio::test]
async fn test_turn_settlement_03() {
    let release = Arc::new(AtomicBool::new(false));
    let first_entered = Arc::new(AtomicUsize::new(0));
    let second_entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let first: Arc<dyn Tool> = Arc::new(GatedTool::new(
        "first_read",
        ToolConcurrency::Parallel,
        Arc::clone(&first_entered),
        Arc::clone(&release),
        Arc::clone(&completed),
        "first",
    ));
    let second: Arc<dyn Tool> = Arc::new(GatedTool::new(
        "second_read",
        ToolConcurrency::Parallel,
        Arc::clone(&second_entered),
        Arc::clone(&release),
        Arc::clone(&completed),
        "second",
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![first, second]);
        let response = ModelResponse::new(vec![
            tool_call("call-item-1", "call-1", "first_read"),
            tool_call("call-item-2", "call-2", "second_read"),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run(),
            &cancel,
            &mut tracker,
            &mut ToolFailureTracker::new(),
        ))
        .await
    });

    wait_for_count(&first_entered, 1).await;
    wait_for_count(&second_entered, 1).await;
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    release.store(true, Ordering::SeqCst);

    let settled = task.await.expect("settlement task joins").unwrap();
    assert_eq!(completed.load(Ordering::SeqCst), 2);
    let outputs = settled
        .new_step_items()
        .iter()
        .filter_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => Some(output.call_id().as_str().to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(outputs, ["call-1", "call-2"]);
    assert_eq!(
        stored_text(output_for(settled.new_step_items(), "call-1")),
        Some("first".to_owned())
    );
    assert_eq!(
        stored_text(output_for(settled.new_step_items(), "call-2")),
        Some("second".to_owned())
    );
}

#[tokio::test]
async fn test_turn_settlement_04() {
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(GatedTool::new(
        "read",
        ToolConcurrency::Parallel,
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
        "content",
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![tool]);
        let response = ModelResponse::new(vec![
            tool_call("call-item-1", "call-1", "read"),
            tool_call("call-item-2", "call-2", "read"),
            tool_call("call-item-3", "call-3", "read"),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        settle_turn(
            TurnSettlementRequest::new(
                &binding,
                &response,
                &surface,
                run(),
                &cancel,
                &mut tracker,
                &mut ToolFailureTracker::new(),
            )
            .with_max_function_tool_concurrency(1),
        )
        .await
    });

    wait_for_count(&entered, 1).await;
    // The other two futures are scheduled but still waiting for a semaphore permit.  They have
    // not reached `Tool::call`, so one model response cannot create unbounded active work.
    assert_eq!(entered.load(Ordering::SeqCst), 1);
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    release.store(true, Ordering::SeqCst);

    task.await
        .expect("settlement task joins")
        .expect("all calls settle after permits are released");
    assert_eq!(entered.load(Ordering::SeqCst), 3);
    assert_eq!(completed.load(Ordering::SeqCst), 3);
}

/// Has to stay on the single-threaded test runtime. Dispatch order is the order the batch spawns
/// its tasks, which only equals model order while one thread runs them FIFO; on a multi-threaded
/// runtime `patch` could take the write permit first, `read` would then never reach `Tool::call`,
/// and the release below would never fire. The batch promises the two permits are mutually
/// exclusive, **not** that the model's order decides who takes one first — resource-level
/// admission belongs to a later milestone, this one only owns the read/write permit.
#[tokio::test]
async fn test_turn_settlement_05() {
    let release = Arc::new(AtomicBool::new(false));
    let parallel_entered = Arc::new(AtomicUsize::new(0));
    let exclusive_entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let parallel: Arc<dyn Tool> = Arc::new(GatedTool::new(
        "read",
        ToolConcurrency::Parallel,
        Arc::clone(&parallel_entered),
        Arc::clone(&release),
        Arc::clone(&completed),
        "read",
    ));
    let exclusive: Arc<dyn Tool> = Arc::new(GatedTool::new(
        "patch",
        ToolConcurrency::Exclusive,
        Arc::clone(&exclusive_entered),
        Arc::clone(&release),
        Arc::clone(&completed),
        "patch",
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![parallel, exclusive]);
        let response = ModelResponse::new(vec![
            tool_call("call-item-1", "call-1", "read"),
            tool_call("call-item-2", "call-2", "patch"),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run(),
            &cancel,
            &mut tracker,
            &mut ToolFailureTracker::new(),
        ))
        .await
    });

    wait_for_count(&parallel_entered, 1).await;
    // The exclusive future has been started, but it cannot enter Tool::call while the read permit
    // is held by the still-blocked `read` call.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(exclusive_entered.load(Ordering::SeqCst), 0);
    release.store(true, Ordering::SeqCst);

    task.await
        .expect("settlement task joins")
        .expect("both actions settle");
    assert_eq!(completed.load(Ordering::SeqCst), 2);
    assert_eq!(exclusive_entered.load(Ordering::SeqCst), 1);
}

/// Settles one batch in which every call fails at the same barrier, and returns the batch-level
/// error. One shared barrier is what makes completion order unusable as an explanation for the
/// result: no call can finish before all of them have arrived.
async fn settle_simultaneous_failures(classes: &[PropagatingFailure]) -> Error {
    let expected = classes.len();
    let barrier = Arc::new(tokio::sync::Barrier::new(expected));
    let entered = Arc::new(AtomicUsize::new(0));
    let tools = classes
        .iter()
        .enumerate()
        .map(|(order, class)| {
            Arc::new(BarrierFailureTool::new(
                &format!("failure_{order}"),
                Arc::clone(&barrier),
                Arc::clone(&entered),
                *class,
            )) as Arc<dyn Tool>
        })
        .collect::<Vec<_>>();
    let calls = (0..expected)
        .map(|order| {
            tool_call(
                &format!("call-item-{order}"),
                &format!("call-{order}"),
                &format!("failure_{order}"),
            )
        })
        .collect::<Vec<_>>();

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(tools);
        let response = ModelResponse::new(calls);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run(),
            &cancel,
            &mut tracker,
            &mut ToolFailureTracker::new(),
        ))
        .await
    });

    wait_for_count(&entered, expected).await;
    task.await
        .expect("settlement task joins")
        .expect_err("a batch of propagating failures cannot settle")
}

#[tokio::test]
async fn test_turn_settlement_06() {
    use PropagatingFailure::{Guardrail, Other, Timeout, User};

    // Each row's expected winner is declared *last*, so both "whichever finished first" and
    // "whichever the model asked for first" give the wrong answer for every row but the last.
    let cases: [(&str, Vec<PropagatingFailure>, &str); 4] = [
        (
            "user_error_outranks_everything",
            vec![Other, Timeout, Guardrail, User],
            "caller",
        ),
        (
            "guardrail_outranks_timeout_and_other",
            vec![Other, Timeout, Guardrail],
            "guardrail.tool_output",
        ),
        (
            "timeout_outranks_other",
            vec![Other, Other, Timeout],
            "tool.timeout",
        ),
        // Nothing left to arbitrate: equal classes fall back to model order.
        (
            "equal_classes_fall_back_to_model_order",
            vec![Other, Other, Other],
            "tool.execution_failed",
        ),
    ];

    for (label, classes, expected_code) in cases {
        let error = settle_simultaneous_failures(&classes).await;
        assert_eq!(error.code(), expected_code, "{label} 选错了主失败：{error}");
    }
}

#[tokio::test]
async fn test_turn_settlement_07() {
    let error = settle_simultaneous_failures(&[PropagatingFailure::Other; 3]).await;

    // The class cannot separate these three, so the reported one has to be the call the model
    // asked for first — otherwise the batch's error is whichever task the scheduler polled first.
    match error {
        Error::Tool { tool, .. } => assert_eq!(tool, "failure_0"),
        other => panic!("应当报成工具失败，实际是：{other}"),
    }
}

#[tokio::test]
async fn test_turn_settlement_08() {
    // The late call is model order 1 and completion order last, so neither "the model asked for it
    // first" nor "it finished first" can explain a row where it wins.
    let cases: [(&str, PropagatingFailure, PropagatingFailure, &str); 2] = [
        // Wins on class alone. The drain loop has to merge what it joins rather than keep whatever
        // collection already selected, and it has to treat a non-cancellation result arriving
        // during teardown as a real outcome rather than teardown noise.
        (
            "late_user_error_outranks_the_peer_that_started_the_teardown",
            PropagatingFailure::Other,
            PropagatingFailure::User,
            "caller",
        ),
        // The reverse guard: arriving late is not itself a claim to the batch outcome.
        (
            "a_late_failure_does_not_outrank_a_higher_class",
            PropagatingFailure::Guardrail,
            PropagatingFailure::Other,
            "guardrail.tool_output",
        ),
    ];

    for (label, peer_failure, late_failure, expected_code) in cases {
        let entered = Arc::new(AtomicUsize::new(0));
        let release_peer = Arc::new(AtomicBool::new(false));
        let late_ready = Arc::new(AtomicBool::new(false));
        let peer: Arc<dyn Tool> = Arc::new(GateThenFailTool::new(
            "starts_the_teardown",
            Arc::clone(&release_peer),
            Arc::clone(&entered),
            peer_failure,
        ));
        let late: Arc<dyn Tool> = Arc::new(LateFailureTool::new(
            "fails_after_teardown_began",
            Arc::clone(&late_ready),
            Arc::clone(&entered),
            late_failure,
        ));

        let task = tokio::spawn(async move {
            let binding = binding();
            let surface = surface(vec![peer, late]);
            let response = ModelResponse::new(vec![
                tool_call("call-item-1", "call-1", "starts_the_teardown"),
                tool_call("call-item-2", "call-2", "fails_after_teardown_began"),
            ]);
            let cancel = CancelScope::root();
            let mut tracker = ToolUseTracker::new();
            settle_turn(TurnSettlementRequest::new(
                &binding,
                &response,
                &surface,
                run(),
                &cancel,
                &mut tracker,
                &mut ToolFailureTracker::new(),
            ))
            .await
        });

        // Both calls are inside `Tool::call`, and the late one is parked on a future that will
        // never wake it. Arming it therefore schedules nothing: the batch stays exactly as it is
        // until the peer is released, and the next poll the late call gets is the one that
        // `cancel_tool_scopes` triggers.
        wait_for_count(&entered, 2).await;
        late_ready.store(true, Ordering::SeqCst);
        release_peer.store(true, Ordering::SeqCst);

        let error = task
            .await
            .expect("settlement task joins")
            .expect_err("a propagating failure stops the batch");
        assert_eq!(error.code(), expected_code, "{label} 选错了主失败：{error}");
    }
}

#[tokio::test]
async fn test_turn_settlement_09() {
    // Distinct from the test above: this failure is not a tool result at all, it is the supervised
    // task dying during teardown, which reaches arbitration as a `JoinError` rather than as an
    // `Err` the tool returned. Model order 0 is the call that fails while being torn down and
    // model order 1 is the one that starts the teardown, so each row turns on exactly one rule.
    let cases: [(&str, PropagatingFailure, &str); 2] = [
        // Equal classes: the cleanup failure wins on model order.
        (
            "cleanup_failure_merges_and_wins_on_order",
            PropagatingFailure::Other,
            "caller",
        ),
        // A task that died during cleanup is a runtime defect, not the public `UserError` class of
        // the error container that carries it — so it must not outrank a guardrail tripwire that
        // lost on model order.
        (
            "guardrail_outranks_a_cleanup_failure",
            PropagatingFailure::Guardrail,
            "guardrail.tool_output",
        ),
    ];

    for (label, peer_failure, expected_code) in cases {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let entered = Arc::new(AtomicUsize::new(0));
        let late: Arc<dyn Tool> = Arc::new(PanicOnTeardownTool::new(
            "fails_while_tearing_down",
            Arc::clone(&barrier),
            Arc::clone(&entered),
        ));
        let peer: Arc<dyn Tool> = Arc::new(BarrierFailureTool::new(
            "starts_the_teardown",
            barrier,
            Arc::clone(&entered),
            peer_failure,
        ));

        let task = tokio::spawn(async move {
            let binding = binding();
            let surface = surface(vec![late, peer]);
            let response = ModelResponse::new(vec![
                tool_call("call-item-1", "call-1", "fails_while_tearing_down"),
                tool_call("call-item-2", "call-2", "starts_the_teardown"),
            ]);
            let cancel = CancelScope::root();
            let mut tracker = ToolUseTracker::new();
            settle_turn(TurnSettlementRequest::new(
                &binding,
                &response,
                &surface,
                run(),
                &cancel,
                &mut tracker,
                &mut ToolFailureTracker::new(),
            ))
            .await
        });

        wait_for_count(&entered, 2).await;
        let error = task
            .await
            .expect("settlement task joins")
            .expect_err("a propagating failure stops the batch");
        assert_eq!(error.code(), expected_code, "{label} 选错了主失败：{error}");
    }
}

#[tokio::test]
async fn test_turn_settlement_10() {
    let entered = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let first: Arc<dyn Tool> = Arc::new(DropReportingTool::new(
        "first_waiter",
        Arc::clone(&entered),
        Arc::clone(&dropped),
    ));
    let second: Arc<dyn Tool> = Arc::new(DropReportingTool::new(
        "second_waiter",
        Arc::clone(&entered),
        Arc::clone(&dropped),
    ));
    let cancel = CancelScope::root();
    let scope = cancel.clone();

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![first, second]);
        let response = ModelResponse::new(vec![
            tool_call("call-item-1", "call-1", "first_waiter"),
            tool_call("call-item-2", "call-2", "second_waiter"),
        ]);
        let mut tracker = ToolUseTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run(),
            &cancel,
            &mut tracker,
            &mut ToolFailureTracker::new(),
        ))
        .await
    });

    wait_for_count(&entered, 2).await;
    scope.cancel(CancelReason::UserInterrupt);

    let error = task
        .await
        .expect("settlement task joins")
        .expect_err("the parent cancellation leaves the turn cancelled");
    assert!(matches!(error, Error::Cancelled { .. }));
    // Observing the error is not enough: both task-local cleanup guards must already have run.
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_turn_settlement_11() {
    let surface = surface(vec![Arc::new(ScriptedTool::new(
        "write_file",
        Behavior::Succeed("ok"),
    ))]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "wrte_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    // 悬空的 tool call 会让下一次请求非法，所以它必须被回答；而模型还没看过这条失败，
    // 所以这一轮不能收尾。这正是 `has_tools_or_approvals_to_run()` 把 not-found 算进去的兑现。
    let output = output_for(settled.new_step_items(), "call-1");
    assert!(output.is_error());
    assert_eq!(output.output()["error"]["code"], json!("tool.not_found"));
    assert!(matches!(settled.next_step(), NextStep::RunAgain));
}

#[tokio::test]
async fn test_turn_settlement_12() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Succeed("ok"))
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    // 审批在任何东西跑起来之前发生。
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let NextStep::Interruption { items } = settled.next_step() else {
        panic!("应当停下来问人");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id().as_str(), "call-1.approval");
    assert!(items[0].kind().is_interruption());

    // 恢复读的是会话，所以待审批项必须存进去；`SingleStepResult` 的校验也认这一条。
    assert!(
        settled
            .session_step_items()
            .iter()
            .any(|item| item.id().as_str() == "call-1.approval")
    );
}

#[tokio::test]
async fn test_turn_settlement_13() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Succeed("ok"))
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Dynamic)),
    );
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![
        item(
            "approval-1",
            RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                "req-1",
                "docs",
                "search",
                json!({ "query": "x" }),
            )),
        ),
        tool_call("call-item-1", "call-1", "write_file"),
    ]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    let NextStep::Interruption { items } = settled.next_step() else {
        panic!("应当停下来问人");
    };
    let asked = items
        .iter()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    // 停下来却只问一部分，剩下那条要等一个永远不会来的轮次。
    assert_eq!(asked, ["approval-1", "call-1.approval"]);
}

#[tokio::test]
async fn test_turn_settlement_14() {
    let tool = Arc::new(ScriptedTool::new("write_file", Behavior::Fail));
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    let output = output_for(settled.new_step_items(), "call-1");
    assert!(output.is_error());
    assert_eq!(
        output.output()["error"]["code"],
        json!("tool.execution_failed")
    );
    assert_eq!(output.output()["error"]["tool"], json!("write_file"));

    // 框架的错误文案是写给日志和人看的，而且是中文的；它一旦进模型上下文，
    // 下一轮就会依赖它。载荷里只有 code 与工具名，没有任何散文——
    // 既没有工具自己的 message，也没有 `Error` 的 Display 模板。
    let rendered = output.output().to_string();
    let leaked = Error::tool(
        ToolErrorKind::ExecutionFailed,
        "write_file",
        "工具内部失败：路径不存在",
    )
    .to_string();
    assert!(!rendered.contains("路径不存在"));
    assert!(!rendered.contains("失败"));
    assert!(!rendered.contains(&leaked));
    assert_eq!(output.output()["error"].as_object().unwrap().len(), 2);
}

#[tokio::test]
async fn test_turn_settlement_15() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Fail)
            .with_options(ToolOptions::new().with_failure_handling(ToolFailureHandling::Propagate)),
    );
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let error = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        Error::Tool {
            kind: ToolErrorKind::ExecutionFailed,
            ..
        }
    ));
}

#[tokio::test]
async fn test_turn_settlement_16() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Fail)
            .with_options(ToolOptions::new().with_failure_handling(ToolFailureHandling::Custom)),
    );
    let handled = Arc::clone(&tool.failures_handled);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    assert_eq!(handled.load(Ordering::SeqCst), 1);
    let output = output_for(settled.new_step_items(), "call-1");
    // 这是唯一一条把散文送进模型上下文的路径，而且写它的是工具自己。
    assert_eq!(
        stored_text(&output),
        Some("tool wrote its own explanation".to_owned())
    );
}

#[tokio::test]
async fn test_turn_settlement_17() {
    let visible = Arc::new(
        ScriptedTool::new("slow", Behavior::Sleep(Duration::from_millis(80)))
            .with_options(ToolOptions::new().with_timeout(Duration::from_millis(5))),
    );
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "slow")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface(vec![visible]),
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();
    let output = output_for(settled.new_step_items(), "call-1");
    assert!(output.is_error());
    assert_eq!(output.output()["error"]["code"], json!("tool.timeout"));

    let propagating = Arc::new(
        ScriptedTool::new("slow", Behavior::Sleep(Duration::from_millis(80))).with_options(
            ToolOptions::new()
                .with_timeout(Duration::from_millis(5))
                .with_timeout_behavior(ToolTimeoutBehavior::Propagate),
        ),
    );
    let error = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface(vec![propagating]),
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        Error::Tool {
            kind: ToolErrorKind::Timeout,
            ..
        }
    ));
}

#[tokio::test]
async fn test_turn_settlement_18() {
    let tool = Arc::new(ScriptedTool::new(
        "slow",
        Behavior::Sleep(Duration::from_secs(30)),
    ));
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "slow")]);
    let cancel = CancelScope::root();

    let scope = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        scope.cancel(CancelReason::UserInterrupt);
    });

    let error = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap_err();

    // 把取消报成一条工具失败，会让 loop 越过那个叫它停下来的东西继续花钱。
    assert!(matches!(error, Error::Cancelled { .. }));
}

#[tokio::test]
async fn test_turn_settlement_19() {
    let tool = Arc::new(ScriptedTool::new("write_file", Behavior::Succeed("ok")));
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();
    cancel.cancel(CancelReason::Shutdown);

    let error = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Cancelled { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_turn_settlement_20() {
    // 上一条测试只覆盖了「解析得到工具」那一格。真正的契约是：一轮是否报成取消，
    // 不能取决于模型这次恰好点了什么名字——而剩下三格各自会settle成一个不同的谎。
    let cases: [(&str, Vec<RunItem>); 3] = [
        // 只有未解析调用：会生成 `tool.not_found` 观察并settle成 `RunAgain`。
        (
            "not_found",
            vec![tool_call("call-item-1", "call-1", "vanished")],
        ),
        // 只有 MCP 审批：会settle成 `Interruption`，向宿主要一个此刻没人该回答的决定。
        (
            "mcp_approval",
            vec![item(
                "approval-1",
                RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                    "req-1",
                    "docs",
                    "search",
                    json!({ "query": "x" }),
                )),
            )],
        ),
        // 纯消息、无动作：最糟的一格——会settle成 `FinalOutput{Final}`，而 `is_complete()`
        // 为真意味着 R15 认为不欠收尾、R17-3 走成功边。取消被报成了「agent 自己做完了」。
        ("no_action", vec![message("msg-1", "完事了")]),
    ];

    for (label, output) in cases {
        let response = ModelResponse::new(output);
        let cancel = CancelScope::root();
        cancel.cancel(CancelReason::UserInterrupt);

        let settled = settle_turn(TurnSettlementRequest::new(
            &binding(),
            &response,
            &surface(vec![Arc::new(ScriptedTool::new(
                "write_file",
                Behavior::Succeed("ok"),
            ))]),
            run(),
            &cancel,
            &mut ToolUseTracker::new(),
            &mut ToolFailureTracker::new(),
        ))
        .await;

        let error = settled
            .map(|settled| format!("{:?}", settled.next_step()))
            .unwrap_err();
        assert!(
            matches!(error, Error::Cancelled { .. }),
            "{label} 应当报成取消，实际是：{error}"
        );
    }
}

#[tokio::test]
async fn test_turn_settlement_21() {
    let tool = Arc::new(
        ScriptedTool::new("write_file", Behavior::Succeed("ok"))
            .with_options(ToolOptions::new().with_allowed_callers([ToolCaller::Programmatic])),
    );
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let output = output_for(settled.new_step_items(), "call-1");
    assert!(output.is_error());
    assert_eq!(output.output()["error"]["code"], json!("tool.not_found"));
}

#[tokio::test]
async fn test_turn_settlement_22() {
    let handoff = ModelHandoffDefinition::new(
        AgentId::new("reviewer"),
        "transfer_to_reviewer",
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    );
    let surface = TurnActionSurface::new(Vec::new(), vec![handoff]).unwrap();
    let response = ModelResponse::new(vec![tool_call(
        "call-item-1",
        "call-1",
        "transfer_to_reviewer",
    )]);
    let cancel = CancelScope::root();

    let error = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap_err();

    assert!(error.to_string().contains("reviewer"));
    assert!(error.to_string().contains("R17"));
}

#[tokio::test]
async fn test_turn_settlement_23() {
    let ok = Arc::new(ScriptedTool::new("read_file", Behavior::Succeed("content")));
    let failing = Arc::new(ScriptedTool::new("write_file", Behavior::Fail));
    let surface = surface(vec![ok, failing]);
    let response = ModelResponse::new(vec![
        tool_call("call-item-1", "call-1", "read_file"),
        tool_call("call-item-2", "call-2", "write_file"),
        tool_call("call-item-3", "call-3", "vanished"),
    ]);
    let cancel = CancelScope::root();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    // 三个调用、三份配对输出。少一份下一次请求就是非法的。
    assert!(!output_for(settled.new_step_items(), "call-1").is_error());
    assert!(output_for(settled.new_step_items(), "call-2").is_error());
    assert!(output_for(settled.new_step_items(), "call-3").is_error());
    assert!(matches!(settled.next_step(), NextStep::RunAgain));
}

#[tokio::test]
async fn test_turn_settlement_24() {
    let tool = Arc::new(ScriptedTool::new("write_file", Behavior::Succeed("ok")));
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "write_file")]);
    let cancel = CancelScope::root();
    let original: Vec<ra_core::item::ModelInputItem> =
        vec![ra_core::item::ModelInputItem::Message(Message::user(
            "开始",
        ))];

    let settled = settle_turn(
        TurnSettlementRequest::new(
            &binding(),
            &response,
            &surface,
            run(),
            &cancel,
            &mut ToolUseTracker::new(),
            &mut ToolFailureTracker::new(),
        )
        .with_original_input(original.clone())
        .with_pre_step_items(vec![message("old-1", "上一轮")]),
    )
    .await
    .unwrap();

    assert_eq!(settled.original_input(), original.as_slice());
    assert_eq!(settled.pre_step_items().len(), 1);
    // `generated_items()` 是下一轮输入的来源：前序在前，本轮在后。
    let ids = settled
        .generated_items()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["old-1", "call-item-1", "call-1.output"]);
    // 分类结果必须是这条响应的分类，会话必须存下模型说过的每一条——都由 builder 校验过了。
    assert_eq!(
        settled.processed_response().new_items().len(),
        response.output().len()
    );
}

/// 上一条测试的前提：`Error` 的 Display 确实是面向人的中文模板，所以它绝不能进模型上下文。
#[test]
fn test_turn_settlement_25() {
    let error = Error::tool(ToolErrorKind::ExecutionFailed, "write_file", "boom");
    let rendered: String = error.to_string();
    assert!(rendered.contains("失败"));
    assert!(rendered.contains("boom"));
    // 送回模型的那份只保留机器可读的两项。
    let observation: Value = json!({ "error": { "code": error.code(), "tool": "write_file" } });
    assert_eq!(observation["error"]["code"], json!("tool.execution_failed"));
    assert!(!observation.to_string().contains("boom"));
}

/// A settled turn is shareable, like everything else the loop carries across an `await`.
#[tokio::test]
async fn test_turn_settlement_26() {
    let surface = surface(Vec::new());
    let response = ModelResponse::new(vec![message("msg-1", "完事了")]);
    let cancel = CancelScope::root();
    let settled = Arc::new(
        settle_turn(TurnSettlementRequest::new(
            &binding(),
            &response,
            &surface,
            run(),
            &cancel,
            &mut ToolUseTracker::new(),
            &mut ToolFailureTracker::new(),
        ))
        .await
        .unwrap(),
    );
    let cloned = Arc::clone(&settled);
    let joined = tokio::spawn(async move { cloned.new_step_items().len() })
        .await
        .unwrap();
    assert_eq!(joined, 1);
}

/// The lock is only here to prove a run context stays `Send + Sync`: it is shared by every tool a
/// batch dispatches, and those run on separate tasks.
#[allow(dead_code)]
fn context_is_thread_safe(_context: &RunContext) {
    let _guard: Mutex<()> = Mutex::new(());
}
