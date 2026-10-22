//! The live thread and the runner's pending input, run end to end. Ported from Codex's
//! `CodexThread` and session turn input (`core/src/codex_thread.rs`,
//! `core/src/session/turn_input.rs`, `core/src/session/turn.rs`, `core/src/tasks/mod.rs`): a turn
//! starts on the thread's history, input given while it runs steers it at the next model-call
//! boundary, a turn about to finish follows up on input still pending, an interrupt aborts it, and
//! a turn waiting for approval stays active until it is answered.
//!
//! What a model call sees is checked through the input the scripted model records; that a turn
//! has reached a point is checked through gates the model or a hook waits on, never through time.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::{FutureExt, future::BoxFuture};
use ra_core::{
    agent::{AgentId, AgentSpec, HandoffSpec},
    cancel::CancelScope,
    capability::{Capability, CapabilityFamily, SamplingContext},
    error::{Error, Result},
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    item::{
        CallId, ContentBlock, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    lifecycle::{AgentEndInput, LifecycleHook, LifecycleScope},
    model::{
        ApiProtocol, Effort, Model, ModelOutputSchema, ModelRequest, ModelResolver, ModelSelector,
        ModelSettings, ProviderKey, ResolvedModel,
    },
    output::OutputSchema,
    prompt::{PromptSection, SectionPosition, SectionStability},
    session::{SessionId, interrupt::is_interrupted_turn_marker},
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    hook::UserHookRegistration,
    runner::{PendingInput, RunConfig, RunRequest, Runner},
    thread::{
        AgentThread, NotSubmittedReason, SteerSubmission, TURN_OUTPUT_SCHEMA_NAME, ThreadEvent,
        ThreadSettingsOverrides, TurnAbortReason, TurnInputRequest, TurnInputSubmission,
        TurnStartOptions, turn_input_item_id,
    },
};
use serde_json::{Value, json};
use tokio::{sync::Notify, time::timeout};

#[path = "agent_thread/wait_input.rs"]
mod wait_input;

/// How long a case waits for any one event.
const PATIENCE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// What the model does on one call.
enum Step {
    Respond(ModelResponse),
    /// Awaits a side effect — a gate — before answering.
    After(BoxFuture<'static, ()>, ModelResponse),
    /// Never answers; the call ends only by cancellation.
    Hang,
    /// Awaits a gate, then fails.
    FailAfter(BoxFuture<'static, ()>),
}

/// What one model call was asked.
#[derive(Debug, Clone)]
struct Call {
    model: Option<String>,
    effort: Option<Effort>,
    temperature: Option<f64>,
    instructions: Option<String>,
    tools: Vec<String>,
    input: Vec<ModelInputItem>,
    output_schema: Option<ModelOutputSchema>,
}

#[derive(Default)]
struct Script {
    steps: Mutex<VecDeque<Step>>,
    calls: Mutex<Vec<Call>>,
    /// The model each call resolved, in order.
    resolved: Mutex<Vec<Option<String>>>,
    /// Woken as each call starts.
    called: Notify,
}

impl Script {
    fn push(&self, step: Step) {
        self.steps.lock().unwrap().push_back(step);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// Waits until the model has been called `count` times.
    async fn wait_for_calls(&self, count: usize) {
        timeout(PATIENCE, async {
            loop {
                let called = self.called.notified();
                if self.calls.lock().unwrap().len() >= count {
                    return;
                }
                called.await;
            }
        })
        .await
        .expect("the model is called");
    }
}

struct Resolver(Arc<Script>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        self.0
            .resolved
            .lock()
            .unwrap()
            .push(model_name.map(str::to_owned));
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some(model_name.unwrap_or("canonical-model").to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(ScriptedModel {
                script: Arc::clone(&self.0),
                model: model_name.map(str::to_owned),
            }) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ScriptedModel {
    script: Arc<Script>,
    model: Option<String>,
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.script.calls.lock().unwrap().push(Call {
            model: self.model.clone(),
            effort: request.model_settings().effort(),
            temperature: request.model_settings().temperature(),
            instructions: request.system_instructions().map(str::to_owned),
            tools: request
                .tools()
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect(),
            input: request.input().to_vec(),
            output_schema: request.output_schema().cloned(),
        });
        self.script.called.notify_waiters();
        let step = self.script.steps.lock().unwrap().pop_front();
        match step {
            Some(Step::Respond(response)) => Ok(response),
            Some(Step::After(effect, response)) => {
                effect.await;
                Ok(response)
            }
            Some(Step::Hang) => std::future::pending().await,
            Some(Step::FailAfter(effect)) => {
                effect.await;
                Err(Error::caller("the model failed"))
            }
            None => Err(Error::caller("the model ran out of responses")),
        }
    }
}

fn final_message(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn tool_call(id: &str, call_id: &str, name: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, json!({}))),
    )])
}

/// A gate the model waits on, and the handle that opens it.
fn gate() -> (Arc<Notify>, BoxFuture<'static, ()>) {
    let gate = Arc::new(Notify::new());
    let waiter = Arc::clone(&gate);
    (gate, async move { waiter.notified().await }.boxed())
}

/// A tool that answers at once, optionally only once approved.
struct QuickTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    approval: ToolApprovalPolicy,
}

impl QuickTool {
    fn shared(name: &str, approval: ToolApprovalPolicy) -> Arc<dyn Tool> {
        Arc::new(Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            approval,
        })
    }
}

#[async_trait]
impl Tool for QuickTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(self.approval)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("looked"))
    }
}

/// A lifecycle hook that holds the end of a run until released, and says when it is reached.
struct HoldAtEnd {
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl LifecycleHook for HoldAtEnd {
    fn name(&self) -> &str {
        "hold-at-end"
    }

    async fn on_agent_end(&self, _scope: LifecycleScope, _input: &AgentEndInput<'_>) -> Result<()> {
        self.reached.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

/// A stop hook that counts the conclusions it is asked about and lets each stand.
#[derive(Default)]
struct CountingStop {
    calls: Mutex<usize>,
}

#[async_trait]
impl UserHook for CountingStop {
    fn name(&self) -> &str {
        "counting-stop"
    }

    async fn call(
        &self,
        _context: &UserHookContext<'_>,
        _event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        *self.calls.lock().unwrap() += 1;
        Ok(HookDecision::Continue)
    }
}

fn agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(ra_core::agent::AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(vec![
            QuickTool::shared("quick_look", ToolApprovalPolicy::Never),
            QuickTool::shared("guarded_look", ToolApprovalPolicy::Always),
        ])
        .build()
        .unwrap()
}

fn thread(script: &Arc<Script>) -> AgentThread {
    thread_with(script, RunConfig::new())
}

fn thread_with(script: &Arc<Script>, config: RunConfig) -> AgentThread {
    AgentThread::builder(
        SessionId::new("thread-1"),
        AgentBinding::direct(agent()),
        Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
    )
    .with_config(config)
    .with_step_model_switching(true)
    .build()
}

struct ThreadCapability {
    tool: Arc<dyn Tool>,
    sampling: Mutex<Vec<SamplingContext>>,
}

#[async_trait]
impl Capability for ThreadCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SHELL
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.tool.clone()]
    }

    async fn instructions(&self) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(
            self.kind().prompt_section_name(),
            "thread capability instructions",
            self.kind().prompt_source(),
            SectionStability::Stable,
            SectionPosition::Prefix,
            "thread capability instructions",
        )?))
    }

    fn sampling_params_for(
        &self,
        settings: ModelSettings,
        context: &SamplingContext,
    ) -> ModelSettings {
        self.sampling.lock().unwrap().push(context.clone());
        settings.with_temperature(if context.model() == Some("next-model") {
            0.8
        } else {
            0.2
        })
    }
}

fn transfer(target: &str) -> HandoffSpec {
    HandoffSpec::new(
        AgentId::new(target),
        ToolSchema::new(
            format!("transfer_to_{target}"),
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .unwrap(),
    )
}

fn handoff_thread(script: &Arc<Script>, prepared: bool) -> (AgentThread, Arc<ThreadCapability>) {
    let capability = Arc::new(ThreadCapability {
        tool: QuickTool::shared("capability_look", ToolApprovalPolicy::Never),
        sampling: Mutex::new(Vec::new()),
    });
    let mut lead = agent().to_builder().handoff(transfer("reviewer"));
    let config = if prepared {
        lead = lead.tools(capability.tools());
        RunConfig::new().with_prepared_capabilities(
            [capability.clone() as Arc<dyn Capability>],
            capability.tools(),
        )
    } else {
        RunConfig::new().with_capability(capability.clone())
    };
    let lead = lead.build().unwrap();
    let reviewer = AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .instructions("reviewer")
        .tool(QuickTool::shared(
            "guarded_look",
            ToolApprovalPolicy::Always,
        ))
        .handoff(transfer("lead"))
        .build()
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(lead.clone())
        .register(reviewer)
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new("handoff-thread"),
        AgentBinding::direct(lead),
        Arc::new(Resolver(script.clone())),
    )
    .with_config(config.with_agent_registry(registry))
    .with_step_model_switching(true)
    .build();
    (thread, capability)
}

fn assert_lead_capabilities(call: &Call, temperature: f64) {
    assert_eq!(
        call.instructions.as_deref(),
        Some("lead\n\nthread capability instructions")
    );
    assert!(call.tools.contains(&"capability_look".to_owned()));
    assert_eq!(call.temperature, Some(temperature));
}

fn assert_reviewer_without_capabilities(call: &Call) {
    assert_eq!(call.instructions.as_deref(), Some("reviewer"));
    assert_eq!(call.tools, ["guarded_look"]);
    assert_eq!(call.temperature, None);
}

fn text(text: &str) -> TurnInputRequest {
    TurnInputRequest::new(vec![ContentBlock::text(text)])
}

fn texts(input: &[ModelInputItem]) -> Vec<String> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) => Some(message.text_content()),
            ModelInputItem::ToolCallOutput(_) => Some("<tool output>".to_owned()),
            ModelInputItem::ToolCall(call) => Some(format!("<call {}>", call.name())),
            _ => None,
        })
        .collect()
}

async fn next_event(thread: &AgentThread) -> ThreadEvent {
    loop {
        let event = timeout(PATIENCE, thread.next_event())
            .await
            .expect("the thread reports an event")
            .expect("the thread keeps reporting");
        if !matches!(
            event,
            ThreadEvent::ThreadSettingsApplied | ThreadEvent::Item { .. }
        ) {
            return event;
        }
    }
}

/// Waits for the turn `turn_id`'s run to report that it stopped on approvals.
async fn awaits_approval(thread: &AgentThread, turn_id: &str) {
    match next_event(thread).await {
        ThreadEvent::TurnAwaitingApproval { turn_id: id } => assert_eq!(id, turn_id),
        other => panic!("expected the turn to wait for approval, got {other:?}"),
    }
}

async fn started(thread: &AgentThread, request: TurnInputRequest) -> String {
    match thread.start_or_steer_turn(request).await {
        TurnInputSubmission::Started { turn_id } => turn_id,
        other => panic!("expected a started turn, got {other:?}"),
    }
}

/// Waits for the turn `turn_id` to start, then to complete, and returns its final answer.
async fn completes(thread: &AgentThread, turn_id: &str) -> Option<String> {
    match next_event(thread).await {
        ThreadEvent::TurnStarted { turn_id: id, .. } => assert_eq!(id, turn_id),
        other => panic!("expected the turn to start, got {other:?}"),
    }
    match next_event(thread).await {
        ThreadEvent::TurnComplete {
            turn_id: id,
            last_agent_message,
            ..
        } => {
            assert_eq!(id, turn_id);
            last_agent_message
        }
        other => panic!("expected the turn to complete, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Pending input in the runner
// ---------------------------------------------------------------------------------------------

fn run_request(script: &Arc<Script>, pending: &PendingInput) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(agent()),
        Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
        RunId::new("run-1"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("do the work"))],
    )
    .with_pending_input(pending.clone())
}

#[tokio::test]
async fn pending_input_is_delivered_at_the_next_model_call() {
    let script = Arc::new(Script::default());
    let pending = PendingInput::new();
    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("m-1", "c-1", "quick_look")));
    script.push(Step::Respond(final_message("m-2", "done")));

    let run = tokio::spawn(Runner::run(run_request(&script, &pending)));
    script.wait_for_calls(1).await;
    pending
        .push(vec![Message::user("also check the tests")])
        .unwrap();
    release.notify_one();
    let result = run.await.unwrap().unwrap();

    let calls = script.calls();
    assert_eq!(texts(&calls[0].input), vec!["do the work"]);
    assert_eq!(
        texts(&calls[1].input),
        vec![
            "do the work",
            "<call quick_look>",
            "<tool output>",
            "also check the tests"
        ]
    );
    assert_eq!(result.final_text(), "done");
    assert!(pending.is_closed(), "a finished run closes its queue");
}

#[tokio::test]
async fn a_run_about_to_finish_follows_up_on_pending_input() {
    let script = Arc::new(Script::default());
    let pending = PendingInput::new();
    let (release, wait) = gate();
    script.push(Step::After(wait, final_message("m-1", "first answer")));
    script.push(Step::Respond(final_message("m-2", "second answer")));

    let run = tokio::spawn(Runner::run(run_request(&script, &pending)));
    script.wait_for_calls(1).await;
    pending.push(vec![Message::user("one more thing")]).unwrap();
    release.notify_one();
    let result = run.await.unwrap().unwrap();

    let calls = script.calls();
    assert_eq!(
        calls.len(),
        2,
        "the pending input is answered by another call"
    );
    assert_eq!(
        texts(&calls[1].input),
        vec!["do the work", "first answer", "one more thing"]
    );
    assert_eq!(result.final_text(), "second answer");
}

#[tokio::test]
async fn input_pending_before_the_first_model_call_waits_for_the_second() {
    let script = Arc::new(Script::default());
    let pending = PendingInput::new();
    pending.push(vec![Message::user("queued early")]).unwrap();
    script.push(Step::Respond(final_message("m-1", "first answer")));
    script.push(Step::Respond(final_message("m-2", "second answer")));

    let result = Runner::run(run_request(&script, &pending)).await.unwrap();

    // The run's own input is answered first, as Codex samples a turn's input before what is
    // pending; the queued input is then answered by a follow-up.
    let calls = script.calls();
    assert_eq!(texts(&calls[0].input), vec!["do the work"]);
    assert_eq!(
        texts(&calls[1].input),
        vec!["do the work", "first answer", "queued early"]
    );
    assert_eq!(result.final_text(), "second answer");
}

#[tokio::test]
async fn pending_input_is_followed_up_before_a_stop_hook_is_asked() {
    let script = Arc::new(Script::default());
    let pending = PendingInput::new();
    let stop = Arc::new(CountingStop::default());
    let (release, wait) = gate();
    script.push(Step::After(wait, final_message("m-1", "first answer")));
    script.push(Step::Respond(final_message("m-2", "second answer")));

    let request = run_request(&script, &pending).with_config(RunConfig::new().with_user_hook(
        UserHookRegistration::new(HookEventName::Stop, Arc::clone(&stop) as Arc<dyn UserHook>),
    ));
    let run = tokio::spawn(Runner::run(request));
    script.wait_for_calls(1).await;
    pending.push(vec![Message::user("one more thing")]).unwrap();
    release.notify_one();
    run.await.unwrap().unwrap();

    // Asked only about the conclusion that ended the run: the first had input pending.
    assert_eq!(*stop.calls.lock().unwrap(), 1);
    assert_eq!(script.calls().len(), 2);
}

#[tokio::test]
async fn input_pushed_after_a_run_finishes_is_refused() {
    let script = Arc::new(Script::default());
    let pending = PendingInput::new();
    script.push(Step::Respond(final_message("m-1", "done")));

    Runner::run(run_request(&script, &pending)).await.unwrap();

    let refused = pending.push(vec![Message::user("too late")]);
    assert_eq!(refused.unwrap_err().len(), 1);
    assert_eq!(script.calls().len(), 1);
}

#[tokio::test]
async fn a_run_that_stops_for_approval_leaves_its_queue_open() {
    let script = Arc::new(Script::default());
    let pending = PendingInput::new();
    script.push(Step::Respond(tool_call("m-1", "c-1", "guarded_look")));

    let result = Runner::run(run_request(&script, &pending)).await.unwrap();

    assert!(!result.outcome().interruptions().is_empty());
    assert!(!pending.is_closed());
    pending.push(vec![Message::user("meanwhile")]).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Starting and steering turns
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn turns_continue_the_thread_history() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    script.push(Step::Respond(final_message("m-1", "hello")));
    script.push(Step::Respond(final_message("m-2", "again")));

    let first = started(&thread, text("hi")).await;
    assert_eq!(completes(&thread, &first).await.as_deref(), Some("hello"));
    assert!(!thread.is_running());
    let second = started(&thread, text("and now")).await;
    assert_ne!(first, second);
    assert_eq!(completes(&thread, &second).await.as_deref(), Some("again"));

    let calls = script.calls();
    assert_eq!(texts(&calls[1].input), vec!["hi", "hello", "and now"]);
    assert_eq!(
        texts(&thread.history()),
        vec!["hi", "hello", "and now", "again"]
    );
}

#[tokio::test]
async fn start_or_steer_steers_the_running_turn() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("m-1", "c-1", "quick_look")));
    script.push(Step::Respond(final_message("m-2", "done")));

    let turn_id = started(&thread, text("run it")).await;
    script.wait_for_calls(1).await;
    assert_eq!(
        thread.start_or_steer_turn(text("and this")).await,
        TurnInputSubmission::Steered {
            turn_id: turn_id.clone()
        }
    );
    release.notify_one();
    completes(&thread, &turn_id).await;

    assert_eq!(
        texts(&script.calls()[1].input),
        vec!["run it", "<call quick_look>", "<tool output>", "and this"]
    );
}

#[tokio::test]
async fn steer_turn_names_the_running_turn() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    assert_eq!(
        thread.steer_turn(&text("steer"), "turn-does-not-exist"),
        SteerSubmission::NotSubmitted {
            reason: NotSubmittedReason::NoActiveTurn
        }
    );

    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("m-1", "c-1", "quick_look")));
    script.push(Step::Respond(final_message("m-2", "done")));
    let turn_id = started(&thread, text("run it")).await;
    script.wait_for_calls(1).await;

    assert_eq!(
        thread.steer_turn(&text("steer"), "wrong-turn"),
        SteerSubmission::NotSubmitted {
            reason: NotSubmittedReason::ExpectedTurnMismatch {
                expected: "wrong-turn".to_owned(),
                actual: turn_id.clone(),
            }
        }
    );
    assert_eq!(
        thread.steer_turn(&TurnInputRequest::new(Vec::new()), &turn_id),
        SteerSubmission::NotSubmitted {
            reason: NotSubmittedReason::EmptyInput
        }
    );
    assert_eq!(
        thread
            .start_or_steer_turn(TurnInputRequest::new(Vec::new()))
            .await,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::EmptyInput
        }
    );
    assert_eq!(
        thread.steer_turn(&text("steer"), &turn_id),
        SteerSubmission::Steered {
            turn_id: turn_id.clone()
        }
    );
    release.notify_one();
    completes(&thread, &turn_id).await;
}

#[tokio::test]
async fn an_empty_start_answers_the_history_as_it_is() {
    let script = Arc::new(Script::default());
    let thread = AgentThread::builder(
        SessionId::new("thread-1"),
        AgentBinding::direct(agent()),
        Arc::new(Resolver(Arc::clone(&script))) as Arc<dyn ModelResolver>,
    )
    .with_history(vec![ModelInputItem::Message(Message::user("earlier"))])
    .build();
    script.push(Step::Respond(final_message("m-1", "answered")));

    let turn_id = started(&thread, TurnInputRequest::new(Vec::new())).await;
    completes(&thread, &turn_id).await;

    assert_eq!(texts(&script.calls()[0].input), vec!["earlier"]);
}

#[tokio::test]
async fn a_start_while_the_turn_finishes_waits_and_starts_the_next_turn() {
    let script = Arc::new(Script::default());
    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let config = RunConfig::new().with_lifecycle_hook(Arc::new(HoldAtEnd {
        reached: Arc::clone(&reached),
        release: Arc::clone(&release),
    }));
    let thread = thread_with(&script, config);
    script.push(Step::Respond(final_message("m-1", "first")));
    script.push(Step::Respond(final_message("m-2", "second")));

    let first = started(&thread, text("one")).await;
    // The run has closed its queue and is ending: the input cannot steer it any more.
    timeout(PATIENCE, reached.notified()).await.unwrap();
    assert_eq!(
        thread.steer_turn(&text("two"), &first),
        SteerSubmission::NotSubmitted {
            reason: NotSubmittedReason::NoActiveTurn
        }
    );
    let start = {
        let thread = thread.clone();
        tokio::spawn(async move { thread.start_or_steer_turn(text("two")).await })
    };
    completes_after_release(&thread, &first, &release).await;
    let second = match timeout(PATIENCE, start).await.unwrap().unwrap() {
        TurnInputSubmission::Started { turn_id } => turn_id,
        other => panic!("expected the next turn to start, got {other:?}"),
    };
    assert_ne!(first, second);
    timeout(PATIENCE, reached.notified()).await.unwrap();
    release.notify_one();
    completes(&thread, &second).await;
    assert_eq!(texts(&script.calls()[1].input), vec!["one", "first", "two"]);
}

async fn completes_after_release(thread: &AgentThread, turn_id: &str, release: &Notify) {
    match next_event(thread).await {
        ThreadEvent::TurnStarted { turn_id: id, .. } => assert_eq!(id, turn_id),
        other => panic!("expected the turn to start, got {other:?}"),
    }
    release.notify_one();
    match next_event(thread).await {
        ThreadEvent::TurnComplete { turn_id: id, .. } => assert_eq!(id, turn_id),
        other => panic!("expected the turn to complete, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Interrupting
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn interrupt_aborts_the_running_turn_and_drops_undelivered_input() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    script.push(Step::Hang);
    assert!(!thread.interrupt(), "nothing to interrupt yet");

    let turn_id = started(&thread, text("run forever")).await;
    script.wait_for_calls(1).await;
    assert!(matches!(
        thread.start_or_steer_turn(text("never read")).await,
        TurnInputSubmission::Steered { .. }
    ));
    assert!(thread.interrupt());

    match next_event(&thread).await {
        ThreadEvent::TurnStarted { turn_id: id, .. } => assert_eq!(id, turn_id),
        other => panic!("expected the turn to start, got {other:?}"),
    }
    match next_event(&thread).await {
        ThreadEvent::TurnAborted {
            turn_id: id,
            reason,
            ..
        } => {
            assert_eq!(id, turn_id);
            assert_eq!(reason, TurnAbortReason::Interrupted);
        }
        other => panic!("expected the turn to be aborted, got {other:?}"),
    }
    assert!(!thread.is_running());
    let history = thread.history();
    assert_eq!(texts(&history)[0], "run forever");
    assert!(
        !texts(&history).iter().any(|text| text == "never read"),
        "{history:?}"
    );
    assert!(
        matches!(history.last(), Some(ModelInputItem::Message(message)) if is_interrupted_turn_marker(message)),
        "{history:?}"
    );
}

/// Codex's `Op::Shutdown`: a running turn is aborted as an interrupt aborts it and waited for,
/// input is refused from then on, and shutting down again returns at once.
#[tokio::test]
async fn shutdown_aborts_the_running_turn_and_refuses_input() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    script.push(Step::Hang);
    let turn_id = started(&thread, text("run forever")).await;
    script.wait_for_calls(1).await;
    assert!(!thread.is_shut_down());

    timeout(PATIENCE, thread.shutdown())
        .await
        .expect("shutdown waits only for the aborted turn");
    assert!(thread.is_shut_down());
    assert!(!thread.is_running());
    let _started = next_event(&thread).await;
    match next_event(&thread).await {
        ThreadEvent::TurnAborted {
            turn_id: id,
            reason,
            ..
        } => {
            assert_eq!(id, turn_id);
            assert_eq!(reason, TurnAbortReason::Interrupted);
        }
        other => panic!("expected the turn to be aborted, got {other:?}"),
    }
    assert!(
        matches!(thread.history().last(), Some(ModelInputItem::Message(message)) if is_interrupted_turn_marker(message)),
        "{:?}",
        thread.history()
    );

    assert_eq!(
        thread.start_or_steer_turn(text("too late")).await,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::ShutDown
        }
    );
    assert_eq!(
        thread.steer_turn(&text("too late"), &turn_id),
        SteerSubmission::NotSubmitted {
            reason: NotSubmittedReason::ShutDown
        }
    );
    timeout(PATIENCE, thread.shutdown())
        .await
        .expect("a thread already down shuts down at once");
    assert_eq!(script.calls().len(), 1);
}

/// An idle thread shuts down at once and starts no turn afterwards.
#[tokio::test]
async fn shutdown_of_an_idle_thread_refuses_the_next_start() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    timeout(PATIENCE, thread.shutdown())
        .await
        .expect("an idle thread shuts down at once");
    assert_eq!(
        thread.start_or_steer_turn(text("hello")).await,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::ShutDown
        }
    );
    assert!(script.calls().is_empty());
}

#[tokio::test]
async fn a_turn_waiting_for_approval_stays_active_until_answered() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    script.push(Step::Respond(tool_call("m-1", "c-1", "guarded_look")));
    script.push(Step::Respond(final_message("m-2", "looked")));

    let turn_id = started(&thread, text("look")).await;
    match next_event(&thread).await {
        ThreadEvent::TurnStarted { .. } => {}
        other => panic!("expected the turn to start, got {other:?}"),
    }
    awaits_approval(&thread, &turn_id).await;
    let mut state = timeout(PATIENCE, async {
        loop {
            if let Some(state) = thread.paused_run() {
                return state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the run pauses for approval");
    assert_eq!(thread.active_turn_id(), Some(turn_id.clone()));
    // Input steered while it waits is delivered once it continues.
    assert!(matches!(
        thread.steer_turn(&text("while you wait"), &turn_id),
        SteerSubmission::Steered { .. }
    ));

    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    // Only the checkpoint of the run that is waiting continues it.
    let foreign = RunState::start(RunId::new("another-run"));
    assert!(thread.resume_turn(foreign).is_err());
    thread.resume_turn(state).unwrap();
    match next_event(&thread).await {
        ThreadEvent::TurnComplete {
            turn_id: id,
            last_agent_message,
            ..
        } => {
            assert_eq!(id, turn_id);
            assert_eq!(last_agent_message.as_deref(), Some("looked"));
        }
        other => panic!("expected the turn to complete, got {other:?}"),
    }
    assert!(
        texts(&script.calls()[1].input).contains(&"while you wait".to_owned()),
        "{:?}",
        script.calls()[1].input
    );
}

#[tokio::test]
async fn approval_after_handoff_resumes_the_target_and_preserves_capability_ownership() {
    for prepared in [false, true] {
        for hand_back in [false, true] {
            let script = Arc::new(Script::default());
            let (thread, _) = handoff_thread(&script, prepared);
            script.push(Step::Respond(tool_call(
                "handoff",
                "handoff",
                "transfer_to_reviewer",
            )));
            script.push(Step::Respond(tool_call(
                "guarded",
                "guarded",
                "guarded_look",
            )));
            if hand_back {
                script.push(Step::Respond(tool_call(
                    "return",
                    "return",
                    "transfer_to_lead",
                )));
                script.push(Step::Respond(tool_call("look", "look", "capability_look")));
            }
            script.push(Step::Respond(final_message("done", "done")));

            let turn_id = started(&thread, text("review")).await;
            assert!(matches!(
                next_event(&thread).await,
                ThreadEvent::TurnStarted { .. }
            ));
            awaits_approval(&thread, &turn_id).await;
            let mut state: RunState =
                serde_json::from_value(serde_json::to_value(thread.paused_run().unwrap()).unwrap())
                    .unwrap();
            assert_eq!(state.starting_agent(), Some(&AgentId::new("lead")));
            assert_eq!(state.current_agent(), Some(&AgentId::new("reviewer")));
            let approval = state.pending_interruption_items().next().unwrap().clone();
            state.approve(&approval, false).unwrap();
            thread.resume_turn(state).unwrap();
            let ending = next_event(&thread).await;
            assert!(matches!(ending,
                ThreadEvent::TurnComplete { turn_id: id, last_agent_message: Some(message), .. }
                    if id == turn_id && message == "done"));

            let calls = script.calls();
            assert_eq!(calls.len(), if hand_back { 5 } else { 3 });
            assert_lead_capabilities(&calls[0], 0.2);
            assert_reviewer_without_capabilities(&calls[1]);
            assert_reviewer_without_capabilities(&calls[2]);
            assert!(texts(&calls[2].input).contains(&"<tool output>".to_owned()));
            if hand_back {
                assert_lead_capabilities(&calls[3], 0.2);
                assert_lead_capabilities(&calls[4], 0.2);
            }
        }
    }
}

#[tokio::test]
async fn approval_resume_rejects_an_unregistered_current_agent_without_consuming_the_pause() {
    let script = Arc::new(Script::default());
    let (thread, _) = handoff_thread(&script, true);
    script.push(Step::Respond(tool_call(
        "handoff",
        "handoff",
        "transfer_to_reviewer",
    )));
    script.push(Step::Respond(tool_call(
        "guarded",
        "guarded",
        "guarded_look",
    )));
    script.push(Step::Respond(final_message("done", "done")));
    let turn_id = started(&thread, text("review")).await;
    next_event(&thread).await;
    awaits_approval(&thread, &turn_id).await;
    let mut state = thread.paused_run().unwrap();
    let approval = state.pending_interruption_items().next().unwrap().clone();
    state.approve(&approval, false).unwrap();
    let mut unknown = state.clone();
    unknown.set_current_agent(AgentId::new("unregistered"));
    let expected = serde_json::to_value(&unknown).unwrap();
    let rejected = thread.resume_turn(unknown).unwrap_err();
    assert_eq!(serde_json::to_value(*rejected).unwrap(), expected);
    assert_eq!(thread.active_turn_id().as_deref(), Some(turn_id.as_str()));
    assert_eq!(
        thread.paused_run().unwrap().current_agent(),
        Some(&AgentId::new("reviewer"))
    );
    assert_eq!(script.calls().len(), 2);

    thread.resume_turn(state).unwrap();
    assert!(matches!(
        next_event(&thread).await,
        ThreadEvent::TurnComplete { last_agent_message: Some(message), .. }
            if message == "done"
    ));
}

#[tokio::test]
async fn first_capability_preparation_after_approval_uses_the_live_model_on_hand_back() {
    use ra_runtime::thread::TurnSettingsUpdateOutcome;
    for prepared in [false, true] {
        let script = Arc::new(Script::default());
        let (thread, capability) = handoff_thread(&script, prepared);
        let (release, wait) = gate();
        script.push(Step::Respond(tool_call(
            "handoff",
            "handoff",
            "transfer_to_reviewer",
        )));
        script.push(Step::Respond(tool_call(
            "guarded",
            "guarded",
            "guarded_look",
        )));
        script.push(Step::After(
            wait,
            tool_call("return", "return", "transfer_to_lead"),
        ));
        script.push(Step::Respond(final_message("done", "done")));
        let turn_id = started(&thread, text("review")).await;
        next_event(&thread).await;
        awaits_approval(&thread, &turn_id).await;
        let mut state = thread.paused_run().unwrap();
        let approval = state.pending_interruption_items().next().unwrap().clone();
        state.approve(&approval, false).unwrap();
        thread.resume_turn(state).unwrap();
        script.wait_for_calls(3).await;
        assert_eq!(
            capability.sampling.lock().unwrap().len(),
            1,
            "the resumed segment has not prepared the starting declaration yet"
        );
        assert_eq!(
            thread.update_turn_settings(
                &turn_id,
                &ThreadSettingsOverrides::new().with_model("next-model"),
            ),
            TurnSettingsUpdateOutcome::Applied
        );
        release.notify_one();
        assert!(matches!(
            next_event(&thread).await,
            ThreadEvent::TurnComplete { last_agent_message: Some(message), .. }
                if message == "done"
        ));

        let calls = script.calls();
        assert_eq!(calls.len(), 4);
        assert_reviewer_without_capabilities(&calls[2]);
        assert_eq!(calls[2].model, None);
        assert_eq!(calls[3].model.as_deref(), Some("next-model"));
        assert_lead_capabilities(&calls[3], 0.8);
        let sampling = capability.sampling.lock().unwrap();
        assert_eq!(sampling.len(), 2);
        assert_eq!(sampling[0].model(), Some("canonical-model"));
        assert_eq!(sampling[1].model(), Some("next-model"));
        assert_eq!(
            sampling[1].provider(),
            Some(&ProviderKey::new("test-provider"))
        );
    }
}

#[tokio::test]
async fn interrupting_a_turn_waiting_for_approval_aborts_it() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    script.push(Step::Respond(tool_call("m-1", "c-1", "guarded_look")));

    let turn_id = started(&thread, text("look")).await;
    timeout(PATIENCE, async {
        while thread.paused_run().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the run pauses for approval");
    assert!(thread.interrupt());

    let _started = next_event(&thread).await;
    awaits_approval(&thread, &turn_id).await;
    match next_event(&thread).await {
        ThreadEvent::TurnAborted { turn_id: id, .. } => assert_eq!(id, turn_id),
        other => panic!("expected the turn to be aborted, got {other:?}"),
    }
    assert!(
        matches!(thread.history().last(), Some(ModelInputItem::Message(message)) if is_interrupted_turn_marker(message))
    );
}

#[tokio::test]
async fn input_a_failed_turn_never_read_stays_in_the_history() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let (release, wait) = gate();
    script.push(Step::FailAfter(wait));

    let turn_id = started(&thread, text("try")).await;
    script.wait_for_calls(1).await;
    assert!(matches!(
        thread.steer_turn(&text("and this"), &turn_id),
        SteerSubmission::Steered { .. }
    ));
    release.notify_one();
    let _started = next_event(&thread).await;
    assert!(matches!(
        next_event(&thread).await,
        ThreadEvent::Error { .. }
    ));
    assert!(matches!(
        next_event(&thread).await,
        ThreadEvent::TurnComplete { .. }
    ));

    // As Codex records the input left pending when its task finishes.
    assert_eq!(texts(&thread.history()), vec!["try", "and this"]);
}

#[tokio::test]
async fn a_failed_turn_reports_the_error_before_it_completes() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);

    let turn_id = started(&thread, text("fail")).await;
    let _started = next_event(&thread).await;
    match next_event(&thread).await {
        ThreadEvent::Error {
            turn_id: id,
            message,
        } => {
            assert_eq!(id, turn_id);
            assert!(message.contains("ran out of responses"), "{message}");
        }
        other => panic!("expected an error, got {other:?}"),
    }
    match next_event(&thread).await {
        ThreadEvent::TurnComplete {
            turn_id: id,
            last_agent_message,
            ..
        } => {
            assert_eq!(id, turn_id);
            assert_eq!(last_agent_message, None);
        }
        other => panic!("expected the turn to complete, got {other:?}"),
    }
    assert_eq!(texts(&thread.history()), vec!["fail"]);
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn settings_apply_to_the_turns_started_after_them() {
    let script = Arc::new(Script::default());
    let thread = thread_with(&script, RunConfig::new().with_model("model-a"));
    assert_eq!(thread.settings().model(), Some("model-a"));
    assert_eq!(thread.settings().effort(), None);

    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("m-1", "c-1", "quick_look")));
    script.push(Step::Respond(final_message("m-2", "first")));
    script.push(Step::Respond(final_message("m-3", "second")));
    script.push(Step::Respond(final_message("m-4", "third")));

    let first = started(&thread, text("one")).await;
    script.wait_for_calls(1).await;
    // A running turn keeps the settings it started with.
    thread.update_settings(
        &ThreadSettingsOverrides::new()
            .with_model("model-b")
            .with_effort(Effort::High),
    );
    release.notify_one();
    completes(&thread, &first).await;

    let second = started(&thread, text("two")).await;
    completes(&thread, &second).await;

    // Settings a start carries apply to the turn it starts.
    let third = started(
        &thread,
        text("three").with_thread_settings(ThreadSettingsOverrides::new().with_model("model-c")),
    )
    .await;
    completes(&thread, &third).await;

    let calls = script.calls();
    let models: Vec<_> = calls.iter().map(|call| call.model.clone()).collect();
    assert_eq!(
        models,
        vec![
            Some("model-a".to_owned()),
            Some("model-a".to_owned()),
            Some("model-b".to_owned()),
            Some("model-c".to_owned()),
        ]
    );
    let efforts: Vec<_> = calls.iter().map(|call| call.effort).collect();
    assert_eq!(
        efforts,
        vec![None, None, Some(Effort::High), Some(Effort::High)]
    );
    assert_eq!(thread.settings().model(), Some("model-c"));
}

#[tokio::test]
async fn settings_a_steer_carries_apply_from_the_next_turn() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("m-1", "c-1", "quick_look")));
    script.push(Step::Respond(final_message("m-2", "first")));
    script.push(Step::Respond(final_message("m-3", "second")));

    let first = started(&thread, text("one")).await;
    script.wait_for_calls(1).await;
    let steer =
        text("steer").with_thread_settings(ThreadSettingsOverrides::new().with_model("next"));
    assert!(matches!(
        thread.steer_turn(&steer, &first),
        SteerSubmission::Steered { .. }
    ));
    release.notify_one();
    completes(&thread, &first).await;
    let second = started(&thread, text("two")).await;
    completes(&thread, &second).await;

    let models: Vec<_> = script
        .calls()
        .iter()
        .map(|call| call.model.clone())
        .collect();
    assert_eq!(models, vec![None, None, Some("next".to_owned())]);
}

#[tokio::test]
async fn settings_a_start_carries_when_it_steers_apply_from_the_next_turn() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("m-1", "c-1", "quick_look")));
    script.push(Step::Respond(final_message("m-2", "first")));
    script.push(Step::Respond(final_message("m-3", "second")));

    let first = started(&thread, text("one")).await;
    script.wait_for_calls(1).await;
    let steer = text("steer").with_thread_settings(
        ThreadSettingsOverrides::new()
            .with_model("next")
            .with_effort(Effort::Low),
    );
    assert_eq!(
        thread.start_or_steer_turn(steer).await,
        TurnInputSubmission::Steered {
            turn_id: first.clone()
        }
    );
    release.notify_one();
    completes(&thread, &first).await;
    let second = started(&thread, text("two")).await;
    completes(&thread, &second).await;

    let calls = script.calls();
    let models: Vec<_> = calls.iter().map(|call| call.model.clone()).collect();
    assert_eq!(models, vec![None, None, Some("next".to_owned())]);
    assert_eq!(calls[2].effort, Some(Effort::Low));
}

#[derive(Default)]
struct PendingRecorder(Mutex<Vec<ra_core::session::rollout::RolloutItem>>);

#[async_trait]
impl ra_core::session::rollout::RolloutRecorder for PendingRecorder {
    fn record(&self, item: ra_core::session::rollout::RolloutItem) {
        self.0.lock().unwrap().push(item);
    }
    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn failed_turn_persists_unconsumed_steering_before_terminal_event() {
    use ra_core::session::rollout::RolloutItem;
    let script = Arc::new(Script::default());
    let recorder = Arc::new(PendingRecorder::default());
    let thread = AgentThread::builder(
        SessionId::new("thread-1"),
        AgentBinding::direct(agent()),
        Arc::new(Resolver(Arc::clone(&script))),
    )
    .with_rollout_recorder(recorder.clone())
    .build();
    let (release, wait) = gate();
    script.push(Step::FailAfter(wait));
    let turn_id = started(&thread, text("initial")).await;
    script.wait_for_calls(1).await;
    assert!(matches!(
        thread.steer_turn(&text("accepted steer"), &turn_id),
        SteerSubmission::Steered { .. }
    ));
    release.notify_one();
    next_event(&thread).await;
    next_event(&thread).await;
    assert!(matches!(
        next_event(&thread).await,
        ThreadEvent::TurnComplete { .. }
    ));
    let count = || {
        recorder.0.lock().unwrap().iter().filter(|item| matches!(item,
        RolloutItem::Item(item) if matches!(item.kind(), RunItemKind::Message(message) if message.text_content() == "accepted steer"))).count()
    };
    assert_eq!(
        count(),
        1,
        "accepted input is durable before completion is observed"
    );
    let records: Vec<_> = recorder
        .0
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, item)| {
            ra_session::rollout::RolloutRecord::new(
                index as u64,
                ra_core::event::EventTimestamp::now(),
                item.into(),
            )
            .unwrap()
        })
        .collect();
    let recovered = ra_session::rollout::reconstruct_history(&records).unwrap();
    assert!(
        texts(recovered.history()).contains(&"accepted steer".into()),
        "a restarted session recovers accepted steering"
    );

    script.push(Step::Respond(final_message("done", "done")));
    started(&thread, text("next")).await;
    next_event(&thread).await;
    next_event(&thread).await;
    assert_eq!(
        count(),
        1,
        "already recorded steering is not duplicated on the next run"
    );
    assert!(texts(&script.calls()[1].input).contains(&"accepted steer".into()));
}

#[tokio::test]
async fn live_settings_capture_next_step_without_changing_thread_defaults() {
    use ra_runtime::thread::TurnSettingsUpdateOutcome;
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("call", "call", "quick_look")));
    script.push(Step::Respond(final_message("done", "done")));
    let turn_id = started(&thread, text("run")).await;
    script.wait_for_calls(1).await;
    assert_eq!(
        thread.update_turn_settings(
            &turn_id,
            &ThreadSettingsOverrides::new()
                .with_model("next-model")
                .with_effort(Effort::High)
        ),
        TurnSettingsUpdateOutcome::Applied
    );
    assert_eq!(thread.run_config().model(), None);
    release.notify_one();
    next_event(&thread).await;
    next_event(&thread).await;
    let calls = script.calls();
    assert_eq!(calls[0].model, None);
    assert_eq!(calls[1].model.as_deref(), Some("next-model"));
    assert_eq!(calls[1].effort, Some(Effort::High));
    assert_eq!(
        thread.update_turn_settings(&turn_id, &ThreadSettingsOverrides::new()),
        TurnSettingsUpdateOutcome::TargetUnavailable
    );
}

#[tokio::test]
async fn model_settings_switching_requires_explicit_feature_selection() {
    use ra_runtime::thread::TurnSettingsUpdateOutcome;
    let script = Arc::new(Script::default());
    let thread = AgentThread::builder(
        SessionId::new("feature-thread"),
        AgentBinding::direct(agent()),
        Arc::new(Resolver(script)),
    )
    .build();
    assert!(
        matches!(thread.update_turn_settings("absent", &ThreadSettingsOverrides::new()),
        TurnSettingsUpdateOutcome::Rejected { reason } if reason.contains("step_model_switching"))
    );
    assert_eq!(
        thread.update_turn_settings(
            "absent",
            &ThreadSettingsOverrides::new().with_config(RunConfig::new())
        ),
        TurnSettingsUpdateOutcome::TargetUnavailable,
        "non-model product updates do not require model switching"
    );
}

struct CapturedContextTool(
    Mutex<Vec<String>>,
    ToolOrigin,
    ToolSchema,
    ToolApprovalPolicy,
);

#[async_trait]
impl Tool for CapturedContextTool {
    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(self.3)
    }
    fn origin(&self) -> &ToolOrigin {
        &self.1
    }
    fn schema(&self) -> &ToolSchema {
        &self.2
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.0
            .lock()
            .unwrap()
            .push(context.run().app_context::<String>().unwrap().clone());
        Ok(ToolOutput::text("captured"))
    }
}

#[tokio::test]
async fn settings_published_during_model_call_do_not_change_its_tool_context() {
    let script = Arc::new(Script::default());
    let tool = Arc::new(CapturedContextTool(Mutex::new(vec![]), ToolOrigin::new("capture").unwrap(),
        ToolSchema::new("capture", json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false})).unwrap(), ToolApprovalPolicy::Never));
    let agent = AgentSpec::builder()
        .id(ra_core::agent::AgentId::new("lead"))
        .name("lead")
        .tools(vec![tool.clone() as Arc<dyn Tool>])
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new("context-thread"),
        AgentBinding::direct(agent),
        Arc::new(Resolver(script.clone())),
    )
    .with_config(RunConfig::new().with_app_context(Arc::new("before".to_owned())))
    .with_step_model_switching(true)
    .build();
    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("call", "call", "capture")));
    script.push(Step::Respond(final_message("final", "done")));
    let turn_id = started(&thread, text("run")).await;
    script.wait_for_calls(1).await;
    assert_eq!(
        thread.update_turn_settings(
            &turn_id,
            &ThreadSettingsOverrides::new()
                .with_config(RunConfig::new().with_app_context(Arc::new("after".to_owned())))
        ),
        ra_runtime::thread::TurnSettingsUpdateOutcome::Applied
    );
    release.notify_one();
    next_event(&thread).await;
    next_event(&thread).await;
    assert_eq!(*tool.0.lock().unwrap(), ["before"]);
}

#[tokio::test]
async fn approval_resume_retains_the_step_settings_that_issued_the_tool() {
    let script = Arc::new(Script::default());
    let tool = Arc::new(CapturedContextTool(Mutex::new(vec![]), ToolOrigin::new("capture").unwrap(),
        ToolSchema::new("capture", json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false})).unwrap(), ToolApprovalPolicy::Always));
    let agent = AgentSpec::builder()
        .id(ra_core::agent::AgentId::new("lead"))
        .name("lead")
        .tools(vec![
            tool.clone() as Arc<dyn Tool>,
            QuickTool::shared("quick_look", ToolApprovalPolicy::Never),
        ])
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new("approval-context-thread"),
        AgentBinding::direct(agent),
        Arc::new(Resolver(script.clone())),
    )
    .with_config(RunConfig::new().with_app_context(Arc::new("initial".to_owned())))
    .with_step_model_switching(true)
    .build();
    let (first_release, first_wait) = gate();
    let (second_release, second_wait) = gate();
    script.push(Step::After(
        first_wait,
        tool_call("first", "first", "quick_look"),
    ));
    script.push(Step::After(
        second_wait,
        tool_call("second", "second", "capture"),
    ));
    script.push(Step::Respond(final_message("final", "done")));
    let turn_id = started(&thread, text("run")).await;
    script.wait_for_calls(1).await;
    assert_eq!(
        thread.update_turn_settings(
            &turn_id,
            &ThreadSettingsOverrides::new()
                .with_config(RunConfig::new().with_app_context(Arc::new("issued".to_owned())))
        ),
        ra_runtime::thread::TurnSettingsUpdateOutcome::Applied
    );
    first_release.notify_one();
    script.wait_for_calls(2).await;
    assert_eq!(
        thread.update_turn_settings(
            &turn_id,
            &ThreadSettingsOverrides::new()
                .with_config(RunConfig::new().with_app_context(Arc::new("future".to_owned())))
        ),
        ra_runtime::thread::TurnSettingsUpdateOutcome::Applied
    );
    second_release.notify_one();
    let mut state = timeout(PATIENCE, async {
        loop {
            if let Some(state) = thread.paused_run() {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pending: Vec<_> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    thread.resume_turn(state).unwrap();
    let _started = next_event(&thread).await;
    awaits_approval(&thread, &turn_id).await;
    next_event(&thread).await;
    assert_eq!(*tool.0.lock().unwrap(), ["issued"]);
}

/// Every event the thread reports up to and including the end of a turn, records among them.
async fn events_until_turn_ends(thread: &AgentThread) -> Vec<ThreadEvent> {
    let mut events = Vec::new();
    loop {
        let event = timeout(PATIENCE, thread.next_event())
            .await
            .expect("the thread reports an event")
            .expect("the thread keeps reporting");
        let ended = matches!(
            event,
            ThreadEvent::TurnComplete { .. } | ThreadEvent::TurnAborted { .. }
        );
        events.push(event);
        if ended {
            return events;
        }
    }
}

/// The records a turn's events carry, by kind and text, in order.
fn reported_records(events: &[ThreadEvent], turn_id: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            ThreadEvent::Item { turn_id: id, item } => {
                assert_eq!(id, turn_id);
                Some(match item.kind() {
                    RunItemKind::Message(message) => {
                        format!("{} {}", item.id().as_str(), message.text_content())
                    }
                    other => format!("{} {}", item.id().as_str(), other.label()),
                })
            }
            _ => None,
        })
        .collect()
}

/// A turn reports, between its start and its end, each record it adds to the thread, in the order
/// its rollout keeps them: the input it started on, under the id `turn_input_item_id` gives it,
/// then what the run produced.
#[tokio::test]
async fn a_turn_reports_its_records_between_its_start_and_its_end() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    script.push(Step::Respond(tool_call("m-1", "c-1", "quick_look")));
    script.push(Step::Respond(final_message("m-2", "seen")));

    let turn_id = started(&thread, text("look")).await;
    let events = events_until_turn_ends(&thread).await;
    assert!(matches!(&events[0], ThreadEvent::TurnStarted { turn_id: id, .. } if *id == turn_id));
    assert_eq!(
        reported_records(&events, &turn_id),
        [
            format!("{} look", turn_input_item_id(&turn_id, 0).as_str()),
            "m-1 tool_call".to_owned(),
            "c-1.output tool_call_output".to_owned(),
            "m-2 seen".to_owned(),
        ]
    );
    assert!(matches!(
        events.last(),
        Some(ThreadEvent::TurnComplete { .. })
    ));
}

/// What a turn's end adds to the thread is reported as its records too: input it never read, and
/// the marker an interrupt leaves.
#[tokio::test]
async fn what_a_turns_end_adds_is_reported_as_its_records() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let (release, wait) = gate();
    script.push(Step::FailAfter(wait));

    let turn_id = started(&thread, text("try")).await;
    script.wait_for_calls(1).await;
    assert!(matches!(
        thread.steer_turn(&text("and this"), &turn_id),
        SteerSubmission::Steered { .. }
    ));
    release.notify_one();
    let events = events_until_turn_ends(&thread).await;
    let records = reported_records(&events, &turn_id);
    assert_eq!(records.len(), 2, "{records:?}");
    assert!(records[1].ends_with(" and this"), "{records:?}");

    script.push(Step::Respond(tool_call("m-1", "c-1", "guarded_look")));
    let turn_id = started(&thread, text("look")).await;
    timeout(PATIENCE, async {
        while thread.paused_run().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the run pauses for approval");
    assert!(thread.paused_run_with_config(&turn_id).is_some());
    assert!(thread.paused_run_with_config("other-turn").is_none());
    assert!(thread.interrupt());
    assert!(thread.paused_run_with_config(&turn_id).is_none());
    let events = events_until_turn_ends(&thread).await;
    let marker = events.iter().rev().find_map(|event| match event {
        ThreadEvent::Item { item, .. } => Some(item.clone()),
        _ => None,
    });
    assert!(
        matches!(
            marker.as_ref().map(RunItem::kind),
            Some(RunItemKind::Message(message)) if is_interrupted_turn_marker(message)
        ),
        "{events:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// A turn's output schema
// ---------------------------------------------------------------------------------------------

fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("parse schema")
}

fn with_schema(request: TurnInputRequest, schema: Value) -> TurnInputRequest {
    request.on_start(TurnStartOptions::new().with_final_output_json_schema(schema))
}

/// What a turn started with `schema` asks the model for.
fn turn_output_schema(schema: Value) -> ModelOutputSchema {
    OutputSchema::json_schema(TURN_OUTPUT_SCHEMA_NAME, schema)
        .to_model_output_schema()
        .unwrap()
}

/// Codex's `start_or_steer_turn_requires_matching_active_output_schema`.
#[tokio::test]
async fn start_or_steer_turn_requires_matching_active_output_schema() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let active_schema = schema(
        r#"{"type":"object","properties":{"answer":{"type":"string"},"count":{"type":"number"}},"required":["answer","count"]}"#,
    );
    let matching_schema_with_different_object_order = schema(
        r#"{"required":["answer","count"],"properties":{"count":{"type":"number"},"answer":{"type":"string"}},"type":"object"}"#,
    );
    let different_schema = schema(
        r#"{"type":"object","properties":{"answer":{"type":"number"}},"required":["answer"]}"#,
    );
    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("m-1", "c-1", "quick_look")));
    script.push(Step::Respond(final_message("m-2", "{}")));

    let turn_id = started(
        &thread,
        with_schema(text("start turn"), active_schema.clone()),
    )
    .await;
    script.wait_for_calls(1).await;

    let rejected = with_schema(
        text("rejected steer").with_thread_settings(
            ThreadSettingsOverrides::new()
                .with_model("next")
                .with_effort(Effort::Low),
        ),
        different_schema,
    );
    assert_eq!(
        thread.start_or_steer_turn(rejected).await,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::ActiveTurnOutputSchemaMismatch,
        }
    );
    // A refused input applies none of its settings.
    assert_eq!(thread.settings().model(), None);
    assert_eq!(thread.settings().effort(), None);

    assert_eq!(
        thread
            .start_or_steer_turn(with_schema(
                text("accepted steer"),
                matching_schema_with_different_object_order,
            ))
            .await,
        TurnInputSubmission::Steered {
            turn_id: turn_id.clone()
        }
    );
    // Input that names no schema steers whatever the turn uses.
    assert_eq!(
        thread.start_or_steer_turn(text("no schema")).await,
        TurnInputSubmission::Steered {
            turn_id: turn_id.clone()
        }
    );

    release.notify_one();
    completes(&thread, &turn_id).await;

    let calls = script.calls();
    assert_eq!(calls.len(), 2);
    let second_request = texts(&calls[1].input);
    assert!(second_request.contains(&"accepted steer".to_owned()));
    assert!(second_request.contains(&"no schema".to_owned()));
    assert!(!second_request.contains(&"rejected steer".to_owned()));
    // Every model call of the turn is asked for its schema.
    for call in &calls {
        assert_eq!(
            call.output_schema,
            Some(turn_output_schema(active_schema.clone()))
        );
    }
}

#[tokio::test]
async fn steer_turn_requires_matching_active_output_schema() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let active_schema = schema(r#"{"type":"object"}"#);
    let (release, wait) = gate();
    script.push(Step::After(wait, tool_call("m-1", "c-1", "quick_look")));
    script.push(Step::Respond(final_message("m-2", "{}")));
    let turn_id = started(&thread, with_schema(text("run it"), active_schema.clone())).await;
    script.wait_for_calls(1).await;

    assert_eq!(
        thread.steer_turn(
            &with_schema(text("rejected"), schema(r#"{"type":"array"}"#)),
            &turn_id
        ),
        SteerSubmission::NotSubmitted {
            reason: NotSubmittedReason::ActiveTurnOutputSchemaMismatch
        }
    );
    // The turn named comes first, as in Codex.
    assert_eq!(
        thread.steer_turn(
            &with_schema(text("rejected"), schema(r#"{"type":"array"}"#)),
            "wrong-turn"
        ),
        SteerSubmission::NotSubmitted {
            reason: NotSubmittedReason::ExpectedTurnMismatch {
                expected: "wrong-turn".to_owned(),
                actual: turn_id.clone(),
            }
        }
    );
    assert_eq!(
        thread.steer_turn(&with_schema(text("accepted"), active_schema), &turn_id),
        SteerSubmission::Steered {
            turn_id: turn_id.clone()
        }
    );
    release.notify_one();
    completes(&thread, &turn_id).await;
    let second_request = texts(&script.calls()[1].input);
    assert!(second_request.contains(&"accepted".to_owned()));
    assert!(!second_request.contains(&"rejected".to_owned()));
}

/// A schema steering cannot match: a turn started without one.
#[tokio::test]
async fn a_schema_does_not_steer_a_turn_started_without_one() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let (release, wait) = gate();
    script.push(Step::After(wait, final_message("m-1", "done")));
    let turn_id = started(&thread, text("run it")).await;
    script.wait_for_calls(1).await;

    assert_eq!(
        thread
            .start_or_steer_turn(with_schema(text("steer"), schema(r#"{"type":"object"}"#)))
            .await,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::ActiveTurnOutputSchemaMismatch,
        }
    );
    release.notify_one();
    completes(&thread, &turn_id).await;
    assert_eq!(script.calls()[0].output_schema, None);
}

/// Codex's `turn_start_output_schema_is_per_turn_v2`, on the thread: a turn's schema replaces what
/// the agent declares for that turn alone.
#[tokio::test]
async fn a_turns_output_schema_holds_for_that_turn_alone() {
    let script = Arc::new(Script::default());
    let agent_schema = json!({
        "type": "object",
        "properties": {"verdict": {"type": "string"}},
        "required": ["verdict"],
        "additionalProperties": false
    });
    let agent = AgentSpec::builder()
        .id(ra_core::agent::AgentId::new("lead"))
        .name("Lead")
        .output_schema(OutputSchema::json_schema("verdict", agent_schema.clone()))
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new("schema-thread"),
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::clone(&script))) as Arc<dyn ModelResolver>,
    )
    .build();
    let turn_schema = json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
        "additionalProperties": false
    });
    // The turn schema is only forwarded; the agent's declaration is validated.
    for (id, answer) in [("first", "{}"), ("second", r#"{"verdict":"done"}"#)] {
        script.push(Step::Respond(final_message(id, answer)));
    }

    let first = started(&thread, with_schema(text("Hello"), turn_schema.clone())).await;
    completes(&thread, &first).await;
    let second = started(&thread, text("Hello again")).await;
    completes(&thread, &second).await;

    let calls = script.calls();
    let first_schema = calls[0].output_schema.clone().unwrap();
    assert_eq!(first_schema, turn_output_schema(turn_schema));
    assert_eq!(first_schema.name(), "codex_output_schema");
    assert!(first_schema.strict());
    assert_eq!(
        calls[1].output_schema,
        OutputSchema::json_schema("verdict", agent_schema).to_model_output_schema()
    );
}

/// A turn that stops for approval keeps its schema when it continues.
#[tokio::test]
async fn a_turns_output_schema_holds_across_an_approval_pause() {
    let script = Arc::new(Script::default());
    let thread = thread(&script);
    let turn_schema = schema(r#"{"type":"object"}"#);
    script.push(Step::Respond(tool_call("m-1", "c-1", "guarded_look")));
    script.push(Step::Respond(final_message("m-2", "{}")));

    let turn_id = started(&thread, with_schema(text("look"), turn_schema.clone())).await;
    let mut state = timeout(PATIENCE, async {
        loop {
            if let Some(state) = thread.paused_run() {
                return state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the run pauses for approval");
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    thread.resume_turn(state).unwrap();
    let _started = next_event(&thread).await;
    awaits_approval(&thread, &turn_id).await;
    match next_event(&thread).await {
        ThreadEvent::TurnComplete { turn_id: id, .. } => assert_eq!(id, turn_id),
        other => panic!("expected the turn to complete, got {other:?}"),
    }

    let calls = script.calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(
            call.output_schema,
            Some(turn_output_schema(turn_schema.clone()))
        );
    }
}

/// A run's own declaration replaces the agent's, plain text included.
#[tokio::test]
async fn a_runs_output_schema_replaces_the_agents_declaration() {
    let script = Arc::new(Script::default());
    let agent = AgentSpec::builder()
        .id(ra_core::agent::AgentId::new("lead"))
        .name("Lead")
        .output_schema(OutputSchema::json_schema(
            "verdict",
            json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
        ))
        .build()
        .unwrap();
    let request = |output_schema: Option<OutputSchema>| {
        let request = RunRequest::new(
            AgentBinding::direct(Arc::clone(&agent)),
            Arc::new(Resolver(Arc::clone(&script))) as Arc<dyn ModelResolver>,
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("answer"))],
        );
        match output_schema {
            Some(output_schema) => request.with_output_schema(output_schema),
            None => request,
        }
    };
    let declared = OutputSchema::json_schema("run", json!({"type": "array"})).with_strict(false);
    for (output_schema, id) in [
        (None, "m-1"),
        (Some(OutputSchema::plain_text()), "m-2"),
        (Some(declared.clone()), "m-3"),
    ] {
        // Without an override the agent's object declaration is validated.
        script.push(Step::Respond(final_message(
            id,
            if id == "m-1" { "{}" } else { "[]" },
        )));
        Runner::run(request(output_schema)).await.unwrap();
    }

    let schemas: Vec<_> = script
        .calls()
        .into_iter()
        .map(|call| call.output_schema.map(|schema| schema.name().to_owned()))
        .collect();
    assert_eq!(
        schemas,
        vec![Some("verdict".to_owned()), None, Some("run".to_owned())]
    );
    assert_eq!(
        script.calls()[2].output_schema,
        declared.to_model_output_schema()
    );
}
