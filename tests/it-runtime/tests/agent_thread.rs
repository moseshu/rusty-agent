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
    agent::AgentSpec,
    cancel::CancelScope,
    error::{Error, Result},
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    item::{
        CallId, ContentBlock, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    lifecycle::{AgentEndInput, LifecycleHook, LifecycleScope},
    model::{
        ApiProtocol, Effort, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings,
        ProviderKey, ResolvedModel,
    },
    session::{SessionId, interrupt::is_interrupted_turn_marker},
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    hook::UserHookRegistration,
    runner::{PendingInput, RunConfig, RunRequest, Runner},
    thread::{
        AgentThread, NotSubmittedReason, SteerSubmission, ThreadEvent, ThreadSettingsOverrides,
        TurnAbortReason, TurnInputRequest, TurnInputSubmission,
    },
};
use serde_json::json;
use tokio::{sync::Notify, time::timeout};

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
    input: Vec<ModelInputItem>,
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
            input: request.input().to_vec(),
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
        if !matches!(event, ThreadEvent::ThreadSettingsApplied) {
            return event;
        }
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
    next_event(&thread).await;
    next_event(&thread).await;
    assert_eq!(*tool.0.lock().unwrap(), ["issued"]);
}
