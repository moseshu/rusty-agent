//! R3-7 contracts for the agent loop and its two entry points.

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
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec, ToolUseBehavior, ToolUseBehaviorHandler, ToolUseResult},
    cancel::{CancelReason, CancelScope},
    error::{Error, Result, ToolErrorKind},
    finish::FinishReason,
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ProviderKey, ResolvedModel,
    },
    state::{ToolUse, WorkStateHandle},
    tool::{
        Tool, ToolApprovalPolicy, ToolCaller, ToolInvocation, ToolLookupKey, ToolNamespace,
        ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
    usage::Usage,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{
        ContinuationInput, RunConfig, RunOutcome, RunRequest, RunResult, RunStreamEvent, Runner,
    },
};
use serde_json::json;
use tokio::{sync::oneshot, time::timeout};

/// Replays a fixed script of responses, one per turn, and records the input it was handed.
struct ScriptedModel {
    script: Mutex<Vec<ModelResponse>>,
    inputs: Mutex<Vec<usize>>,
    input_items: Mutex<Vec<Vec<ModelInputItem>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            inputs: Mutex::new(Vec::new()),
            input_items: Mutex::new(Vec::new()),
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(request.input().len());
        self.input_items
            .lock()
            .unwrap()
            .push(request.input().to_vec());
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            // The loop asked for a turn the script did not plan for. Answering with a final
            // message would hide the mismatch behind a passing test.
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::empty().boxed()
    }
}

/// A model call that only ends by being cancelled. Its drop notification makes cancellation of an
/// in-flight runner observable without relying on timing or a provider implementation.
struct PendingModel {
    started: Mutex<Option<oneshot::Sender<()>>>,
    dropped: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl PendingModel {
    fn new() -> (Arc<Self>, oneshot::Receiver<()>, oneshot::Receiver<()>) {
        let (started_sender, started) = oneshot::channel();
        let (dropped_sender, dropped) = oneshot::channel();
        (
            Arc::new(Self {
                started: Mutex::new(Some(started_sender)),
                dropped: Arc::new(Mutex::new(Some(dropped_sender))),
            }),
            started,
            dropped,
        )
    }
}

struct NotifyWhenDropped(Arc<Mutex<Option<oneshot::Sender<()>>>>);

impl Drop for NotifyWhenDropped {
    fn drop(&mut self) {
        if let Some(sender) = self.0.lock().unwrap().take() {
            let _ = sender.send(());
        }
    }
}

#[async_trait]
impl Model for PendingModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        let _notify = NotifyWhenDropped(Arc::clone(&self.dropped));
        if let Some(sender) = self.started.lock().unwrap().take() {
            let _ = sender.send(());
        }
        futures::future::pending().await
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::empty().boxed()
    }
}

struct FixedResolver {
    model: Arc<ScriptedModel>,
    selectors: Arc<Mutex<Vec<Option<String>>>>,
}

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        self.selectors
            .lock()
            .unwrap()
            .push(model_name.map(str::to_owned));
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.model) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct SingleModelResolver {
    model: Arc<dyn Model>,
}

impl ModelResolver for SingleModelResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.model),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ScriptedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: Arc<AtomicUsize>,
    work_states: Arc<Mutex<Vec<Option<String>>>>,
}

impl ScriptedTool {
    fn new(name: &str) -> Self {
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
            calls: Arc::new(AtomicUsize::new(0)),
            work_states: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_options(mut self, options: ToolOptions) -> Self {
        self.options = options;
        self
    }

    fn namespaced(namespace: &str, name: &str) -> Self {
        let mut tool = Self::new(name);
        tool.origin = ToolOrigin::namespaced(ToolNamespace::new(namespace).unwrap(), name).unwrap();
        tool
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

    async fn call(&self, invocation: ToolInvocation<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.work_states.lock().unwrap().push(
            invocation
                .work_state()
                .and_then(|state| state.as_any().downcast_ref::<TaskState>())
                .map(|state| state.plan.to_owned()),
        );
        Ok(ToolOutput::text("done"))
    }

    async fn needs_approval(&self, _invocation: &ToolInvocation<'_>) -> Result<bool> {
        Ok(!matches!(
            self.options.approval(),
            ToolApprovalPolicy::Never
        ))
    }
}

struct Host;

/// Stand-in for the cross-run task state R17-1 will supply (R3-13).
struct TaskState {
    plan: &'static str,
}

impl WorkStateHandle for TaskState {
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn message(id: &str, text: &str) -> RunItem {
    message_with_phase(id, text, OutputPhase::Final)
}

fn message_with_phase(id: &str, text: &str, phase: OutputPhase) -> RunItem {
    item(id, RunItemKind::Message(Message::assistant(text, phase)))
}

/// The run's message channels in generation order (R3-10).
fn phases(result: &RunResult) -> Vec<OutputPhase> {
    result
        .new_items()
        .iter()
        .filter_map(|item| match item.kind() {
            RunItemKind::Message(message) => message.phase(),
            _ => None,
        })
        .collect()
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

fn agent(tools: Vec<Arc<dyn Tool>>) -> AgentBinding {
    agent_with_tool_use_behavior(tools, ToolUseBehavior::RunLlmAgain)
}

fn agent_with_tool_use_behavior(
    tools: Vec<Arc<dyn Tool>>,
    tool_use_behavior: ToolUseBehavior,
) -> AgentBinding {
    AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("coder"))
            .name("Coder")
            .instructions("do the thing")
            .tools(tools)
            .tool_use_behavior(tool_use_behavior)
            .build()
            .unwrap(),
    )
}

type Selectors = Arc<Mutex<Vec<Option<String>>>>;

fn request(
    tools: Vec<Arc<dyn Tool>>,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
) -> RunRequest {
    request_recording(tools, model, cancel, Arc::new(Mutex::new(Vec::new())))
}

fn request_with_tool_use_behavior(
    tools: Vec<Arc<dyn Tool>>,
    tool_use_behavior: ToolUseBehavior,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
) -> RunRequest {
    RunRequest::new(
        agent_with_tool_use_behavior(tools, tool_use_behavior),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
            selectors: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(Host),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    )
}

fn request_recording(
    tools: Vec<Arc<dyn Tool>>,
    model: &Arc<ScriptedModel>,
    cancel: &CancelScope,
    selectors: Selectors,
) -> RunRequest {
    RunRequest::new(
        agent(tools),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
            selectors,
        }),
        Arc::new(Host),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    )
}

fn pending_request(model: Arc<PendingModel>, cancel: &CancelScope) -> RunRequest {
    RunRequest::new(
        agent(Vec::new()),
        Arc::new(SingleModelResolver { model }),
        Arc::new(Host),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("帮我改一下文件"))],
    )
}

#[tokio::test]
async fn loops_between_tool_calls_and_final_answer_until_model_requests_nothing() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let tool_calls = Arc::clone(&tool.calls);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")])
            .with_usage(Usage::new(10, 4)),
        ModelResponse::new(vec![message("msg-1", "改完了")]).with_usage(Usage::new(20, 6)),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    assert_eq!(result.turns(), 2);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::Final
        }
    ));
    assert_eq!(
        result.final_message().unwrap().content()[0]
            .as_text()
            .unwrap(),
        "改完了"
    );

    // The second request must include the first turn's calls and outputs. A missing paired output
    // makes the provider reject the request.
    let inputs = model.inputs.lock().unwrap().clone();
    assert_eq!(inputs, [1, 3]);

    // Usage is projected by summing each call, not stored as an incrementally accumulated field.
    assert_eq!(result.usage().input_tokens(), 30);
    assert_eq!(result.usage().output_tokens(), 10);
    assert_eq!(result.model_responses().len(), 2);
}

#[tokio::test]
async fn stop_on_first_tool_executes_the_batch_without_calling_the_model_again() {
    let first = Arc::new(ScriptedTool::new("read_file"));
    let second = Arc::new(ScriptedTool::new("write_file"));
    let first_calls = Arc::clone(&first.calls);
    let second_calls = Arc::clone(&second.calls);
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        message("msg-1", "我先读再写"),
        tool_call("c-1", "call-1", "read_file"),
        tool_call("c-2", "call-2", "write_file"),
    ])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request_with_tool_use_behavior(
        vec![first, second],
        ToolUseBehavior::StopOnFirstTool,
        &model,
        &cancel,
    ))
    .await
    .unwrap();

    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::ToolStop
        }
    ));
    assert!(result.final_message().is_none());
    assert!(matches!(
        result.new_items()[3].kind(),
        RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == "call-1"
    ));
    assert!(matches!(
        result.new_items()[4].kind(),
        RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == "call-2"
    ));
}

#[tokio::test]
async fn stop_at_tools_stops_only_for_matching_names_and_supports_qualified_names() {
    let non_match = Arc::new(ScriptedTool::new("read_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "read_file")]),
        ModelResponse::new(vec![message("msg-1", "继续后的最终回答")]),
    ]);
    let cancel = CancelScope::root();
    let continued = Runner::run(request_with_tool_use_behavior(
        vec![non_match],
        ToolUseBehavior::stop_at_tools(["write_file"]),
        &model,
        &cancel,
    ))
    .await
    .unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        continued.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::Final
        }
    ));

    let matched = Arc::new(ScriptedTool::namespaced("workspace", "write_file"));
    let matching_model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-2",
        "call-2",
        "write_file",
    )])]);
    let matched_cancel = CancelScope::root();
    let stopped = Runner::run(request_with_tool_use_behavior(
        vec![matched],
        ToolUseBehavior::stop_at_tools(["workspace.write_file"]),
        &matching_model,
        &matched_cancel,
    ))
    .await
    .unwrap();
    assert_eq!(matching_model.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        stopped.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::ToolStop
        }
    ));
}

#[tokio::test]
async fn custom_tool_use_behavior_receives_complete_ordered_results_and_decides_to_stop() {
    let observed = Arc::new(Mutex::new(Vec::<Vec<(String, String, bool)>>::new()));
    let observed_by_handler = Arc::clone(&observed);
    let handler = Arc::new(move |results: &[ToolUseResult]| -> Result<bool> {
        observed_by_handler.lock().unwrap().push(
            results
                .iter()
                .map(|result| {
                    (
                        result.call_id().as_str().to_owned(),
                        result.tool().qualified_name().to_owned(),
                        result.output().is_error(),
                    )
                })
                .collect(),
        );
        Ok(results
            .iter()
            .any(|result| result.tool().name() == "write_file"))
    });
    let read = Arc::new(ScriptedTool::new("read_file"));
    let write = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        tool_call("c-1", "call-1", "read_file"),
        tool_call("c-2", "call-2", "write_file"),
    ])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request_with_tool_use_behavior(
        vec![read, write],
        ToolUseBehavior::custom(handler),
        &model,
        &cancel,
    ))
    .await
    .unwrap();

    assert_eq!(
        observed.lock().unwrap().as_slice(),
        [[
            ("call-1".to_owned(), "read_file".to_owned(), false),
            ("call-2".to_owned(), "write_file".to_owned(), false),
        ]]
    );
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::ToolStop
        }
    ));
}

#[tokio::test]
async fn custom_tool_use_behavior_can_continue_to_the_model() {
    let handler = Arc::new(|_results: &[ToolUseResult]| -> Result<bool> { Ok(false) });
    let tool = Arc::new(ScriptedTool::new("read_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "read_file")]),
        ModelResponse::new(vec![message("msg-1", "工具结果已处理")]),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request_with_tool_use_behavior(
        vec![tool],
        ToolUseBehavior::custom(handler),
        &model,
        &cancel,
    ))
    .await
    .unwrap();

    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::Final
        }
    ));
}

#[tokio::test]
async fn tool_stop_does_not_bypass_a_pending_approval_interruption() {
    let gated = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "write_file",
    )])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request_with_tool_use_behavior(
        vec![gated],
        ToolUseBehavior::StopOnFirstTool,
        &model,
        &cancel,
    ))
    .await
    .unwrap();

    assert!(matches!(result.outcome(), RunOutcome::Interrupted { .. }));
}

/// A tool that always fails, so its observation carries `is_error`.
struct FailingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl FailingTool {
    fn new(name: &str) -> Self {
        let scripted = ScriptedTool::new(name);
        Self {
            origin: scripted.origin,
            schema: scripted.schema,
        }
    }
}

#[async_trait]
impl Tool for FailingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new()
    }

    async fn call(&self, _invocation: ToolInvocation<'_>) -> Result<ToolOutput> {
        Err(Error::tool(
            ToolErrorKind::ExecutionFailed,
            self.origin.qualified_name(),
            "磁盘满了",
        ))
    }
}

#[tokio::test]
async fn not_every_observation_is_a_result_that_can_end_the_run() {
    // Both rows answer the model with a `tool.*` observation, and neither is a value any policy
    // could promote. Stopping on one would report `FinishReason::ToolStop`, whose `is_complete()`
    // is true — R15 sees no closeout owed and R17-3 takes the success edge — over a call that
    // failed, or one that never ran at all.
    let cases: [(&str, Arc<dyn Tool>); 2] =
        [
            // Caller admission refused it, so from the model's side the tool does not exist. The other
            // half of that same story — a name the turn never advertised — is already excluded.
            (
                "not_admitted",
                Arc::new(ScriptedTool::new("write_file").with_options(
                    ToolOptions::new().with_allowed_callers([ToolCaller::Programmatic]),
                )),
            ),
            // It ran and failed. The model has not read the failure yet, which is exactly why the turn
            // owes it another round.
            ("failed", Arc::new(FailingTool::new("write_file"))),
        ];

    for (label, tool) in cases {
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
            ModelResponse::new(vec![message("msg-1", "第二轮的最终回答")]),
        ]);
        let cancel = CancelScope::root();

        let result = Runner::run(request_with_tool_use_behavior(
            vec![tool],
            ToolUseBehavior::StopOnFirstTool,
            &model,
            &cancel,
        ))
        .await
        .unwrap();

        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            2,
            "{label} 应当逼出下一轮，而不是当场收尾"
        );
        assert!(
            matches!(
                result.outcome(),
                RunOutcome::Completed {
                    reason: FinishReason::Final
                }
            ),
            "{label} 的结局是：{:?}",
            result.outcome()
        );
        // Still answered: excluding it from the policy's view must not drop the record the next
        // request pairs to this call.
        assert!(
            result.new_items().iter().any(|item| matches!(
                item.kind(),
                RunItemKind::ToolCallOutput(output)
                    if output.call_id().as_str() == "call-1" && output.is_error()
            )),
            "{label} 丢了配对给 call-1 的观察"
        );
    }
}

/// What a `CancellationRacingHandler` does once the test lets it reach a decision.
#[derive(Clone, Copy)]
enum HandlerRace {
    /// Never becomes ready, so only cancellation can end the run.
    NeverReady,
    /// Becomes ready without waking anything, so the poll that observes it is provably the one
    /// cancellation triggered — and `CancelScope::run` looks at the handler first.
    ReadyInTheCancellingPoll,
}

/// Ready from a flag, and **never registers a waker**: arming it schedules nothing.
struct ReadyWithoutWaking(Arc<AtomicBool>);

impl Future for ReadyWithoutWaking {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.0.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

struct CancellationRacingHandler {
    entered: Arc<AtomicUsize>,
    ready: Arc<AtomicBool>,
    race: HandlerRace,
}

#[async_trait]
impl ToolUseBehaviorHandler for CancellationRacingHandler {
    async fn should_stop(&self, _tool_results: &[ToolUseResult]) -> Result<bool> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        match self.race {
            HandlerRace::NeverReady => {
                std::future::pending::<()>().await;
                unreachable!("a pending handler can only leave through cancellation")
            }
            HandlerRace::ReadyInTheCancellingPoll => {
                ReadyWithoutWaking(Arc::clone(&self.ready)).await;
                // The most dangerous answer it could give: stop the run and call it finished.
                Ok(true)
            }
        }
    }
}

#[tokio::test]
async fn cancellation_wins_over_custom_policy_whether_or_not_it_answers() {
    let cases: [(&str, HandlerRace); 2] = [
        // A bare `.await` would leave the run alive here with nothing running to blame, and no
        // tool left for the drain protocol to reach.
        ("handler_never_answers", HandlerRace::NeverReady),
        // The harder half. `CancelScope::run` polls the handler before the cancellation, so a
        // handler that is ready in that same wake-up returns normally and the scope is never
        // consulted. Nothing downstream would catch it — settlement does no further awaiting and
        // the runner breaks straight out of the loop on `FinalOutput` — so without an exit
        // checkpoint a stopped run settles as `FinishReason::ToolStop`: `is_complete()`, success
        // edge, no closeout owed.
        (
            "handler_answers_stop_in_the_cancelling_poll",
            HandlerRace::ReadyInTheCancellingPoll,
        ),
    ];

    for (label, race) in cases {
        let entered = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(CancellationRacingHandler {
            entered: Arc::clone(&entered),
            ready: Arc::clone(&ready),
            race,
        });
        let tool = Arc::new(ScriptedTool::new("read_file"));
        let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
            "c-1",
            "call-1",
            "read_file",
        )])]);
        let cancel = CancelScope::root();
        let scope = cancel.clone();

        let task = tokio::spawn(Runner::run(request_with_tool_use_behavior(
            vec![tool],
            ToolUseBehavior::custom(handler),
            &model,
            &cancel,
        )));

        timeout(Duration::from_secs(1), async {
            while entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{label}: 策略应当被问到"));

        // Arming before cancelling is what makes this deterministic rather than timed: it wakes
        // nothing, so the next poll the handler receives is the one `cancel` delivers.
        ready.store(true, Ordering::SeqCst);
        scope.cancel(CancelReason::UserInterrupt);

        let settled = timeout(Duration::from_secs(1), task)
            .await
            .unwrap_or_else(|_| panic!("{label}: 取消后 run 必须返回"))
            .expect("run 任务 join");
        let error = match settled {
            Ok(result) => panic!("{label}: 取消的 run 交付了结果 {:?}", result.outcome()),
            Err(error) => error,
        };
        assert!(
            matches!(error, Error::Cancelled { .. }),
            "{label} 报成了：{error}"
        );
    }
}

#[tokio::test]
async fn custom_tool_use_behavior_errors_propagate_unchanged() {
    // Not folded into stop or continue: a policy that could not decide has not decided, and
    // picking either outcome for it would be the framework inventing an answer.
    let handler = Arc::new(|_results: &[ToolUseResult]| -> Result<bool> {
        Err(Error::caller("这个策略拿不定主意"))
    });
    let tool = Arc::new(ScriptedTool::new("read_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c-1",
        "call-1",
        "read_file",
    )])]);
    let cancel = CancelScope::root();

    let error = Runner::run(request_with_tool_use_behavior(
        vec![tool],
        ToolUseBehavior::custom(handler),
        &model,
        &cancel,
    ))
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Caller { .. }));
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn settlement_outcome_decides_message_channels_and_carries_them_to_next_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        // Provider can label this Final, but its tool call means the runner must treat it as
        // progress.
        ModelResponse::new(vec![
            message_with_phase("msg-1", "我先写入文件", OutputPhase::Final),
            tool_call("c-1", "call-1", "write_file"),
        ]),
        // The inverse must also be corrected: a terminal turn is the final delivery.
        ModelResponse::new(vec![message_with_phase(
            "msg-2",
            "文件已写入",
            OutputPhase::Commentary,
        )]),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    assert_eq!(
        phases(&result),
        [OutputPhase::Commentary, OutputPhase::Final]
    );
    assert_eq!(result.final_message().unwrap().text_content(), "文件已写入");

    let inputs = model.input_items.lock().unwrap();
    let Some(ModelInputItem::Message(first_message)) = inputs[1].get(1) else {
        panic!("第二轮必须收到第一轮的 assistant message");
    };
    assert_eq!(first_message.phase(), Some(OutputPhase::Commentary));
}

#[tokio::test]
async fn only_last_message_is_delivery_when_commentary_and_delivery_coexist_in_turn() {
    // One response carrying narration and then the answer is the shape this milestone is modelled
    // on. Stamping the whole turn `Final` costs twice: the UI renders two closing deliveries, and
    // the record goes back to the model as input next turn, teaching it that pre-tool narration is
    // what a final answer looks like.
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        message_with_phase("msg-1", "我先说一下思路", OutputPhase::Commentary),
        message_with_phase("msg-2", "结论是这样", OutputPhase::Final),
    ])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(Vec::new(), &model, &cancel))
        .await
        .unwrap();

    assert_eq!(
        phases(&result),
        [OutputPhase::Commentary, OutputPhase::Final]
    );
    assert_eq!(result.final_message().unwrap().text_content(), "结论是这样");
}

#[tokio::test]
async fn run_hitting_turn_limit_has_no_delivery_message() {
    // The cap fires between turns, so the last thing the model said is still work in progress.
    // R3-8's error handler is what turns that into a delivered result; until then this has to be
    // `None` rather than promoting a progress update to the run's conclusion.
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(
        (0..8)
            .map(|turn| {
                ModelResponse::new(vec![
                    message_with_phase(
                        &format!("msg-{turn}"),
                        "我接着写下一个文件",
                        OutputPhase::Final,
                    ),
                    tool_call(&format!("c-{turn}"), &format!("call-{turn}"), "write_file"),
                ])
            })
            .collect(),
    );
    let cancel = CancelScope::root();

    let result = Runner::run(
        request(vec![tool], &model, &cancel).with_config(RunConfig::new().with_max_turns(2)),
    )
    .await
    .unwrap();

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::MaxTurns
        }
    ));
    assert_eq!(
        phases(&result),
        [OutputPhase::Commentary, OutputPhase::Commentary]
    );
    assert!(result.final_message().is_none());
}

#[tokio::test]
async fn final_message_does_not_treat_non_assistant_message_as_delivery() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![item(
        "user-1",
        RunItemKind::Message(Message::user("不是模型交付").with_phase(OutputPhase::Final)),
    )])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(Vec::new(), &model, &cancel))
        .await
        .unwrap();

    assert!(result.final_message().is_none());
}

#[tokio::test]
async fn reaching_turn_limit_ends_softly_instead_of_erroring() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    // The model requests a tool on every turn and never concludes.
    let model = ScriptedModel::new(
        (0..8)
            .map(|turn| {
                ModelResponse::new(vec![tool_call(
                    &format!("c-{turn}"),
                    &format!("call-{turn}"),
                    "write_file",
                )])
            })
            .collect(),
    );
    let cancel = CancelScope::root();

    let result = Runner::run(
        request(vec![tool], &model, &cancel).with_config(RunConfig::new().with_max_turns(3)),
    )
    .await
    .unwrap();

    // The limit is the loop's termination condition, not a failure; the host still receives the
    // items already produced.
    assert_eq!(result.turns(), 3);
    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::MaxTurns
        }
    ));
    assert_eq!(result.model_responses().len(), 3);
}

#[tokio::test]
async fn zero_turn_limit_is_rejected_instead_of_treated_as_unlimited() {
    let model = ScriptedModel::new(Vec::new());
    let cancel = CancelScope::root();

    let error = Runner::run(
        request(Vec::new(), &model, &cancel).with_config(RunConfig::new().with_max_turns(0)),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("max_turns"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn zero_tool_concurrency_limit_is_rejected_before_model_call() {
    let model = ScriptedModel::new(Vec::new());
    let cancel = CancelScope::root();

    let error = Runner::run(
        request(Vec::new(), &model, &cancel)
            .with_config(RunConfig::new().with_max_function_tool_concurrency(0)),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("max_function_tool_concurrency"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stopping_for_pending_approval_is_an_outcome_not_an_error() {
    let gated = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let tool_calls = Arc::clone(&gated.calls);
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        message_with_phase("msg-1", "我需要写入文件", OutputPhase::Final),
        tool_call("c-1", "call-1", "write_file"),
    ])]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![gated], &model, &cancel))
        .await
        .unwrap();

    // Reporting an error would give the host no way to answer it and resume.
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("应当停下来问人，实际是 {:?}", result.outcome());
    };
    assert_eq!(items.len(), 1);
    assert!(items[0].kind().is_interruption());
    assert_eq!(result.outcome().finish_reason(), None);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(result.turns(), 1);
    let RunItemKind::Message(message) = result.new_items()[0].kind() else {
        panic!("第一项必须是模型消息");
    };
    assert_eq!(message.phase(), Some(OutputPhase::Commentary));
    assert!(result.final_message().is_none());
}

#[tokio::test]
async fn cancellation_is_not_treated_as_normal_completion() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "不该跑到")])]);
    let cancel = CancelScope::root();
    cancel.cancel(CancelReason::UserInterrupt);

    let error = Runner::run(request(Vec::new(), &model, &cancel))
        .await
        .unwrap_err();

    // Reporting `FinalOutput { Final }` would tell R15 that no closeout is owed and send R17-3
    // down the success path.
    assert!(matches!(error, Error::Cancelled { .. }));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn two_resume_input_modes_produce_different_histories() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    let result = Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    // Display history, session history, and next-turn input are three distinct things. Resume
    // input is a projection rather than a fourth array, so it cannot tell a different story from
    // `new_items`.
    let preserve = result.continuation_input(ContinuationInput::PreserveAll);
    let normalized = result.continuation_input(ContinuationInput::Normalized);
    assert_eq!(preserve.len(), 1 + result.new_items().len());
    assert_eq!(normalized.len(), preserve.len());
    assert_eq!(result.original_input().len(), 1);

    // Original input comes first, followed by generated items in occurrence order.
    assert!(matches!(preserve[0], ModelInputItem::Message(_)));
    assert!(matches!(preserve[1], ModelInputItem::ToolCall(_)));
    assert!(matches!(preserve[2], ModelInputItem::ToolCallOutput(_)));
}

#[tokio::test]
async fn streaming_path_emits_each_turns_records_immediately_and_returns_terminal_state() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    let mut stream = Runner::run_streamed(request(vec![tool], &model, &cancel));
    let mut turns = Vec::new();
    let mut items = Vec::new();
    let mut finished = None;
    while let Some(event) = stream.next_event().await {
        match event {
            RunStreamEvent::TurnStarted { turn, agent } => turns.push((turn, agent)),
            RunStreamEvent::Item(item) => items.push(item.id().as_str().to_owned()),
            RunStreamEvent::Finished(outcome) => finished = Some(outcome),
            _ => panic!("未知事件"),
        }
    }

    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].0, 1);
    // Commentary uses the public identity, which is the agent configured by the user and shown by
    // the host.
    assert_eq!(turns[0].1.as_str(), "coder");
    assert_eq!(items, ["c-1", "call-1.output", "msg-1"]);
    assert!(matches!(
        finished,
        Some(RunOutcome::Completed {
            reason: FinishReason::Final
        })
    ));

    // Exhausting the events still leaves the terminal result available: they are two views of the
    // same run, not a one-use pipe.
    let result = stream.finish().await.unwrap();
    assert_eq!(result.turns(), 2);
    assert_eq!(result.new_items().len(), 3);
}

#[tokio::test]
async fn stream_events_include_settlement_normalized_message_channels() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![
            message_with_phase("msg-1", "我先写入文件", OutputPhase::Final),
            tool_call("c-1", "call-1", "write_file"),
        ]),
        ModelResponse::new(vec![message_with_phase(
            "msg-2",
            "文件已写入",
            OutputPhase::Commentary,
        )]),
    ]);
    let cancel = CancelScope::root();

    let mut stream = Runner::run_streamed(request(vec![tool], &model, &cancel));
    let mut phases = Vec::new();
    while let Some(event) = stream.next_event().await {
        if let RunStreamEvent::Item(item) = event
            && let RunItemKind::Message(message) = item.kind()
        {
            phases.push(message.phase());
        }
    }

    assert_eq!(
        phases,
        [Some(OutputPhase::Commentary), Some(OutputPhase::Final)]
    );
    assert_eq!(stream.finish().await.unwrap().turns(), 2);
}

#[tokio::test]
async fn terminal_result_does_not_require_reading_all_events_first() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-1", "完事了")])]);
    let cancel = CancelScope::root();

    let stream = Runner::run_streamed(request(Vec::new(), &model, &cancel));
    let result = stream.finish().await.unwrap();

    assert_eq!(result.turns(), 1);
    assert_eq!(
        result.final_message().unwrap().content()[0]
            .as_text()
            .unwrap(),
        "完事了"
    );
}

#[tokio::test]
async fn dropping_finish_midstream_still_cancels_background_run() {
    let (model, started, dropped) = PendingModel::new();
    let cancel = CancelScope::root();
    let stream = Runner::run_streamed(pending_request(model, &cancel));
    let finish = tokio::spawn(async move { stream.finish().await });

    started.await.unwrap();
    finish.abort();
    let _ = finish.await;

    timeout(Duration::from_millis(250), dropped)
        .await
        .expect("dropping finish must cancel the background run")
        .unwrap();
    assert!(!cancel.is_cancelled());
}

#[tokio::test]
async fn both_paths_produce_the_same_result() {
    let script = || {
        vec![
            ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
            ModelResponse::new(vec![message("msg-1", "改完了")]),
        ]
    };
    let cancel = CancelScope::root();

    let direct_model = ScriptedModel::new(script());
    let direct = Runner::run(request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &direct_model,
        &cancel,
    ))
    .await
    .unwrap();

    let streamed_model = ScriptedModel::new(script());
    let streamed = Runner::run_streamed(request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &streamed_model,
        &cancel,
    ))
    .finish()
    .await
    .unwrap();

    // The two entry points share one loop and settlement path. If they diverged, the fixed path
    // would hide the other's bug.
    assert_eq!(direct.turns(), streamed.turns());
    assert_eq!(
        direct
            .new_items()
            .iter()
            .map(|item| item.id().as_str().to_owned())
            .collect::<Vec<_>>(),
        streamed
            .new_items()
            .iter()
            .map(|item| item.id().as_str().to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        direct.outcome().finish_reason(),
        streamed.outcome().finish_reason()
    );
}

#[tokio::test]
async fn run_level_model_override_applies_to_every_turn() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();
    let selectors: Selectors = Arc::new(Mutex::new(Vec::new()));

    Runner::run(
        request_recording(vec![tool], &model, &cancel, Arc::clone(&selectors))
            .with_config(RunConfig::new().with_model("run/override")),
    )
    .await
    .unwrap();

    // It must be included on every turn, not only the first. A silently ignored run-level setting
    // manifests as inconsistent billing and latency rather than an error.
    assert_eq!(
        selectors.lock().unwrap().clone(),
        [
            Some("run/override".to_owned()),
            Some("run/override".to_owned())
        ]
    );
}

#[tokio::test]
async fn run_state_is_returned_with_result_so_next_segment_can_continue_counting() {
    // Each segment uses its own `call_id`: the provider mints one per call and the tracker's
    // replay criterion keys on it. Reusing one would make the second segment a replay of the first.
    let script = |segment: u32| {
        vec![
            ModelResponse::new(vec![tool_call(
                &format!("c-{segment}"),
                &format!("call-{segment}"),
                "write_file",
            )]),
            ModelResponse::new(vec![message(&format!("msg-{segment}"), "改完了")]),
        ]
    };
    let cancel = CancelScope::root();

    let first_model = ScriptedModel::new(script(1));
    let first = Runner::run(request(
        vec![Arc::new(ScriptedTool::new("write_file"))],
        &first_model,
        &cancel,
    ))
    .await
    .unwrap();
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    assert_eq!(
        first
            .tool_use()
            .repeat_streak(&AgentId::new("coder"), &identity),
        1
    );

    // Carry the complete run state into the second segment. Replacing it with an empty state would
    // reset consecutive segments, turning pause-and-resume into a way to bypass the R3-6 circuit
    // breaker. This resume entry point takes the complete `RunState`, so later runtime facts cannot
    // be omitted either.
    let second_model = ScriptedModel::new(script(2));
    let second = Runner::run(
        request(
            vec![Arc::new(ScriptedTool::new("write_file"))],
            &second_model,
            &cancel,
        )
        .with_state(first.state().clone()),
    )
    .await
    .unwrap();

    assert_eq!(
        second
            .tool_use()
            .repeat_streak(&AgentId::new("coder"), &identity),
        2
    );
}

#[tokio::test]
async fn work_state_handle_is_propagated_all_the_way_to_tools() {
    // This chain is what R3-13 buys: attached once on the run, it reaches the tool through
    // settlement, batch, and dispatch, so R17 adds channel operations to the handle rather than a
    // parameter to four layers and to every third-party `Tool::call`.
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let seen = Arc::clone(&tool.work_states);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();
    let task_state: Arc<dyn WorkStateHandle> = Arc::new(TaskState { plan: "第三步" });

    Runner::run(request(vec![tool], &model, &cancel).with_work_state(task_state))
        .await
        .unwrap();

    assert_eq!(seen.lock().unwrap().as_slice(), [Some("第三步".to_owned())]);
}

#[tokio::test]
async fn run_without_work_state_lets_tools_observe_none_not_empty_shell() {
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let seen = Arc::clone(&tool.work_states);
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c-1", "call-1", "write_file")]),
        ModelResponse::new(vec![message("msg-1", "改完了")]),
    ]);
    let cancel = CancelScope::root();

    Runner::run(request(vec![tool], &model, &cancel))
        .await
        .unwrap();

    assert_eq!(seen.lock().unwrap().as_slice(), [None]);
}

#[tokio::test]
async fn dropping_stream_does_not_cancel_callers_own_scope() {
    let (model, started, dropped) = PendingModel::new();
    let cancel = CancelScope::root();

    let stream = Runner::run_streamed(pending_request(model, &cancel));
    started.await.unwrap();
    drop(stream);

    // Dropping the stream cancels this run, preventing the provider from spending on an unobserved
    // result, but it cancels a child scope. Cancelling the caller's scope too would let one run's
    // lifetime determine the whole host's lifetime.
    timeout(Duration::from_millis(250), dropped)
        .await
        .expect("dropping the stream must cancel and reap its run")
        .unwrap();
    assert!(!cancel.is_cancelled());

    // The same caller scope can start another run.
    let again = ScriptedModel::new(vec![ModelResponse::new(vec![message("msg-2", "又完事了")])]);
    let result = Runner::run(request(Vec::new(), &again, &cancel))
        .await
        .unwrap();
    assert_eq!(result.turns(), 1);
}
