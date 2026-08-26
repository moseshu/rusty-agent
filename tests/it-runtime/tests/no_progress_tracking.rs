//! Contracts for feeding the failure tracker from turn settlement, and for the breaker it drives.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result, ToolErrorKind},
    item::{
        AgentId, CallId, ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    state::{RunId, ToolFailureTracker, ToolUse, ToolUseTracker},
    tool::{
        Tool, ToolConcurrency, ToolContext, ToolFailureHandling, ToolLookupKey, ToolOptions,
        ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    turn::{TurnSettlementRequest, prepare::TurnActionSurface, settle_turn},
};
use serde_json::{Value, json};

/// A tool whose failure text the test controls turn by turn, so "the same failure" and "a failure
/// that says something new" are two scripted cases rather than two accidents.
struct ScriptedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    script: Mutex<Vec<Option<&'static str>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedTool {
    fn new(name: &str, script: Vec<Option<&'static str>>) -> Self {
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
            script: Mutex::new(script),
            calls: Arc::new(AtomicUsize::new(0)),
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
        let mut script = self.script.lock().unwrap();
        let step = if script.is_empty() {
            None
        } else {
            script.remove(0)
        };
        match step {
            Some(detail) => Err(Error::tool(
                ToolErrorKind::ExecutionFailed,
                self.origin.qualified_name(),
                detail,
            )),
            None => Ok(ToolOutput::text("done")),
        }
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        // The tool writes its own model-facing sentence, so two different failures reach the model
        // as two different answers — the case the breaker must not fire on.
        Ok(Some(ToolOutput::text(error.to_string())))
    }
}

fn binding() -> AgentBinding {
    AgentBinding::direct(spec())
}

fn spec() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("main"))
        .name("main")
        .build()
        .unwrap()
}

/// The live context of the run these settlements belong to.
fn run() -> Arc<RunContext> {
    let spec = spec();
    Arc::new(RunContext::new(
        RunId::new("run-no-progress"),
        spec.as_ref(),
    ))
}

fn call(id: &str, call_id: &str, name: &str, arguments: Value) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, arguments)),
    )
}

fn surface(tools: Vec<Arc<dyn Tool>>) -> TurnActionSurface {
    TurnActionSurface::new(tools, Vec::new()).unwrap()
}

fn identity(name: &str) -> ToolUse {
    ToolUse::Tool(ToolLookupKey::bare(name).unwrap())
}

/// Settles one single-call turn and hands back what the model was answered with.
async fn settle_call(
    surface: &TurnActionSurface,
    tool_use: &mut ToolUseTracker,
    tool_failure: &mut ToolFailureTracker,
    call_id: &str,
    arguments: Value,
) -> Result<Vec<RunItem>> {
    settle_response(
        surface,
        tool_use,
        tool_failure,
        ModelResponse::new(vec![call(
            &format!("item-{call_id}"),
            call_id,
            "run_tests",
            arguments,
        )]),
    )
    .await
}

/// Settles one response and hands back what the model was answered with.
async fn settle_response(
    surface: &TurnActionSurface,
    tool_use: &mut ToolUseTracker,
    tool_failure: &mut ToolFailureTracker,
    response: ModelResponse,
) -> Result<Vec<RunItem>> {
    let cancel = CancelScope::root();
    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        surface,
        run(),
        &cancel,
        tool_use,
        tool_failure,
        Default::default(),
    ))
    .await?;
    Ok(settled.new_step_items().to_vec())
}

fn error_code(items: &[RunItem]) -> Option<String> {
    items.iter().find_map(|item| match item.kind() {
        RunItemKind::ToolCallOutput(output) => output.output()["error"]["code"]
            .as_str()
            .map(ToOwned::to_owned),
        _ => None,
    })
}

fn error_code_for_call(items: &[RunItem], call_id: &str) -> Option<String> {
    items.iter().find_map(|item| match item.kind() {
        RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == call_id => output
            .output()["error"]["code"]
            .as_str()
            .map(ToOwned::to_owned),
        _ => None,
    })
}

#[tokio::test]
async fn three_identical_failures_refuse_the_fourth_call() {
    let tool = Arc::new(ScriptedTool::new(
        "run_tests",
        vec![Some("same"), Some("same"), Some("same"), Some("same")],
    ));
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();

    for call_id in ["c-1", "c-2", "c-3"] {
        settle_call(
            &surface,
            &mut tool_use,
            &mut failures,
            call_id,
            json!({ "id": call_id }),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        failures.no_progress_streak(&AgentId::new("main"), &identity("run_tests")),
        3
    );

    // Arguments change every turn, so the repeat breaker would see nothing here even if it were on.
    let items = settle_call(
        &surface,
        &mut tool_use,
        &mut failures,
        "c-4",
        json!({ "id": "c-4" }),
    )
    .await
    .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 3, "the fourth call never ran");
    assert_eq!(error_code(&items).as_deref(), Some("tool.no_progress"));
}

#[tokio::test]
async fn a_response_is_admitted_as_a_whole_and_the_next_one_pays() {
    // The breaker acts between responses, never inside one: outcomes do not exist until their
    // calls have run. All three calls of this response are admitted on the streak the run had when
    // it arrived, and the refusal lands on the response after it.
    //
    // Admitting them together is what keeps `ToolConcurrency::Parallel` meaning what it says.
    // Ordering same-identity calls so each could see the previous one's outcome would withdraw
    // that declaration from every tool that did not exempt itself — and would do it to enforce a
    // limit those calls usually cannot reach, since this breaker fires on repeated evidence while
    // a chain would be built per identity.
    let tool = Arc::new(ScriptedTool::new(
        "run_tests",
        vec![Some("same"), Some("same"), Some("same"), Some("same")],
    ));
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();
    let agent = AgentId::new("main");

    let items = settle_response(
        &surface,
        &mut tool_use,
        &mut failures,
        ModelResponse::new(vec![
            call("item-c-1", "c-1", "run_tests", json!({ "id": "c-1" })),
            call("item-c-2", "c-2", "run_tests", json!({ "id": "c-2" })),
            call("item-c-3", "c-3", "run_tests", json!({ "id": "c-3" })),
        ]),
    )
    .await
    .unwrap();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "every call in the response ran"
    );
    for call_id in ["c-1", "c-2", "c-3"] {
        assert_eq!(
            error_code_for_call(&items, call_id).as_deref(),
            Some("tool.execution_failed"),
            "{call_id} was answered by the tool, not by the breaker"
        );
    }
    assert_eq!(
        failures.no_progress_streak(&agent, &identity("run_tests")),
        3
    );

    // The response after it is where the limit is enforced.
    let next = settle_call(&surface, &mut tool_use, &mut failures, "c-4", json!({}))
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 3, "the fourth call never ran");
    assert_eq!(error_code(&next).as_deref(), Some("tool.no_progress"));
}

#[tokio::test]
async fn a_failure_that_says_something_new_keeps_the_tool_available() {
    // The acceptance case the whole design exists for: three failures, but the third one tells the
    // run something the first two did not, so the fourth call is still allowed to run.
    //
    // The tool shapes its own failure text, which is what makes "something new" visible at all —
    // see the test below for what happens when the model is shown a bare code instead.
    let tool = Arc::new(
        ScriptedTool::new(
            "run_tests",
            vec![
                Some("two failing"),
                Some("two failing"),
                Some("one failing"),
            ],
        )
        .with_options(ToolOptions::new().with_failure_handling(ToolFailureHandling::Custom)),
    );
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();

    for call_id in ["c-1", "c-2", "c-3", "c-4"] {
        settle_call(&surface, &mut tool_use, &mut failures, call_id, json!({}))
            .await
            .unwrap();
    }

    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        failures.no_progress_streak(&AgentId::new("main"), &identity("run_tests")),
        0,
        "the fourth call succeeded, which clears the record outright"
    );
}

#[tokio::test]
async fn a_tool_that_shapes_its_own_failure_text_is_still_recorded_as_failing() {
    // `ToolFailureHandling::Custom` answers a failure with the tool's own sentence and no error
    // flag. A record that classified outcomes by reading the rendered result would be blind to
    // exactly the tools that explain themselves best.
    let tool = Arc::new(
        ScriptedTool::new("run_tests", vec![Some("same"), Some("same")])
            .with_options(ToolOptions::new().with_failure_handling(ToolFailureHandling::Custom)),
    );
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();

    for call_id in ["c-1", "c-2"] {
        let items = settle_call(&surface, &mut tool_use, &mut failures, call_id, json!({}))
            .await
            .unwrap();
        assert!(
            error_code(&items).is_none(),
            "the model reads the tool's own words, not an error code"
        );
    }

    assert_eq!(
        failures.no_progress_streak(&AgentId::new("main"), &identity("run_tests")),
        2
    );
}

#[tokio::test]
async fn a_refusal_re_arms_the_breaker_instead_of_retiring_the_tool() {
    // The difference from the repeat breaker, which counts attempts and therefore latches. A
    // refusal runs nothing, so it says nothing about the tool; leaving the streak standing would
    // refuse every later call too, since only a result can lower it and no result can now be
    // produced. The model is told once, and the next call is judged on its own.
    let tool = Arc::new(ScriptedTool::new(
        "run_tests",
        vec![Some("same"), Some("same"), Some("same"), Some("same")],
    ));
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();
    let agent = AgentId::new("main");

    for call_id in ["c-1", "c-2", "c-3"] {
        settle_call(&surface, &mut tool_use, &mut failures, call_id, json!({}))
            .await
            .unwrap();
    }
    let refused = settle_call(&surface, &mut tool_use, &mut failures, "c-4", json!({}))
        .await
        .unwrap();

    assert_eq!(error_code(&refused).as_deref(), Some("tool.no_progress"));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        failures.no_progress_streak(&agent, &identity("run_tests")),
        0
    );

    // Re-armed: the next call runs, and a run that really is stuck pays one refused call in every
    // four rather than losing the tool for the rest of the run.
    settle_call(&surface, &mut tool_use, &mut failures, "c-5", json!({}))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        failures.no_progress_streak(&agent, &identity("run_tests")),
        1
    );
}

#[tokio::test]
async fn a_bare_error_code_is_the_same_evidence_however_the_failure_differed() {
    // Under the default failure shaping the model is shown a code and a tool name — the same two
    // strings for two unrelated causes. That the streak advances anyway is the honest reading: the
    // party being asked to change its approach cannot tell those failures apart either.
    let tool = Arc::new(ScriptedTool::new(
        "run_tests",
        vec![Some("no such file: a"), Some("no such file: b")],
    ));
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();

    for call_id in ["c-1", "c-2"] {
        settle_call(&surface, &mut tool_use, &mut failures, call_id, json!({}))
            .await
            .unwrap();
    }

    assert_eq!(
        failures.no_progress_streak(&AgentId::new("main"), &identity("run_tests")),
        2
    );
}

#[tokio::test]
async fn a_tool_can_exempt_itself() {
    // The readiness probe that fails the same way until the thing it waits for exists.
    let tool = Arc::new(
        ScriptedTool::new(
            "run_tests",
            vec![Some("not ready"), Some("not ready"), Some("not ready")],
        )
        .with_options(ToolOptions::new().without_no_progress_limit()),
    );
    let calls = Arc::clone(&tool.calls);
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();

    for call_id in ["c-1", "c-2", "c-3", "c-4"] {
        settle_call(&surface, &mut tool_use, &mut failures, call_id, json!({}))
            .await
            .unwrap();
    }

    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn an_interrupted_turn_records_no_outcome_for_the_call_that_never_ran() {
    // Settling a response that stops for approval must not file an outcome: nothing ran, so
    // nothing was learned or failed to be learned. Resuming settles the same response again, and
    // the record has to read the same afterwards.
    let tool = Arc::new(
        ScriptedTool::new("run_tests", vec![Some("same"), Some("same")]).with_options(
            ToolOptions::new().with_approval(ra_core::tool::ToolApprovalPolicy::Always),
        ),
    );
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();

    let items = settle_call(&surface, &mut tool_use, &mut failures, "c-1", json!({}))
        .await
        .unwrap();

    assert!(
        items
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::ToolApproval(_)))
    );
    assert_eq!(
        failures.no_progress_streak(&AgentId::new("main"), &identity("run_tests")),
        0
    );
}

#[tokio::test]
async fn settling_the_same_failing_turn_twice_counts_once() {
    let tool = Arc::new(ScriptedTool::new(
        "run_tests",
        vec![Some("same"), Some("same")],
    ));
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();
    let agent = AgentId::new("main");

    settle_call(&surface, &mut tool_use, &mut failures, "c-1", json!({}))
        .await
        .unwrap();
    let after_first = failures.clone();
    settle_call(&surface, &mut tool_use, &mut failures, "c-1", json!({}))
        .await
        .unwrap();

    assert_eq!(failures, after_first);
    assert_eq!(
        failures.no_progress_streak(&agent, &identity("run_tests")),
        1
    );
}

#[tokio::test]
async fn a_message_only_turn_leaves_every_streak_alone() {
    let tool = Arc::new(ScriptedTool::new("run_tests", vec![Some("same")]));
    let surface = surface(vec![tool]);
    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();
    let agent = AgentId::new("main");

    settle_call(&surface, &mut tool_use, &mut failures, "c-1", json!({}))
        .await
        .unwrap();

    let response = ModelResponse::new(vec![RunItem::new(
        ItemId::new("msg-1"),
        RunItemKind::Message(Message::assistant("thinking", OutputPhase::Final)),
    )]);
    let cancel = CancelScope::root();
    settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        &surface,
        run(),
        &cancel,
        &mut tool_use,
        &mut failures,
        Default::default(),
    ))
    .await
    .unwrap();

    assert_eq!(
        failures.no_progress_streak(&agent, &identity("run_tests")),
        1
    );
}

/// A tool that cannot finish until the test releases it, so scheduler overlap is observable
/// without depending on elapsed wall-clock time.
struct GatedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    entered: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
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
        self.options.clone()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Ok(ToolOutput::text("done"))
    }
}

#[tokio::test]
async fn one_parallel_tool_still_overlaps_its_own_calls_under_default_options() {
    // Three reads of three different files, the shape the batch was measured on. The no-progress
    // breaker is on here — it is on by default — and it must not cost this tool the concurrency it
    // declared. Nothing else in the suite covers it: the other overlap test uses two *different*
    // tools, and the one that reuses a tool caps concurrency at one.
    let entered = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let tool: Arc<dyn Tool> = Arc::new(GatedTool {
        origin: ToolOrigin::new("read_file").unwrap(),
        schema: ToolSchema::new(
            "read_file",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .unwrap(),
        options: ToolOptions::new().with_concurrency(ToolConcurrency::Parallel),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    assert!(
        tool.options().max_no_progress_streak().is_some(),
        "the point of this test is that the breaker is on"
    );
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![
        call("item-1", "c-1", "read_file", json!({ "path": "a.rs" })),
        call("item-2", "c-2", "read_file", json!({ "path": "b.rs" })),
        call("item-3", "c-3", "read_file", json!({ "path": "c.rs" })),
    ]);

    let watcher = tokio::spawn({
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        async move {
            // All three have to be inside `Tool::call` at once. Serialized dispatch never reaches
            // three, and this future then releases them on the timeout and the assertion below is
            // what reports it.
            for _ in 0..2_000 {
                if entered.load(Ordering::SeqCst) >= 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let peak = entered.load(Ordering::SeqCst);
            release.store(true, Ordering::SeqCst);
            peak
        }
    });

    let mut tool_use = ToolUseTracker::new();
    let mut failures = ToolFailureTracker::new();
    settle_response(&surface, &mut tool_use, &mut failures, response)
        .await
        .unwrap();

    assert_eq!(
        watcher.await.unwrap(),
        3,
        "all three calls must be in flight at once; a breaker must not withdraw declared concurrency"
    );
}
