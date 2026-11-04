//! A compaction hook that stops, and background hook results at the thread's boundaries.
//!
//! Codex's PreCompact and PostCompact hooks answer `Stopped` with `TurnAborted`, which ends the
//! turn as an interrupt does: before compaction nothing is compacted, after it the compaction stays
//! and the context re-established after compaction waits for the next turn. An agent-tool run has
//! no turn of its own, so its stop ends that run alone and reaches the parent as an ordinary tool
//! failure, which the parent's failure handling decides about.
//!
//! Background hook results reach the turn at the boundaries where the runtime delivers hook
//! context: their warnings are shown there, and a thread shutting down drops what is still queued.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{FutureExt, future::BoxFuture};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope},
    capability::{
        ContextProcessor, ContextProcessorRequest, ContextProcessorResult, ContextSummarizer,
    },
    error::{Error, Result},
    hook::{
        CompactTrigger, HookContextQueue, HookDecision, HookEvent, HookEventName, UserHook,
        UserHookContext,
    },
    item::{
        CallId, Compaction, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::{SessionId, interrupt::is_interrupted_turn_marker},
    state::RunId,
    tool::ToolContext,
};
use ra_runtime::{
    agent::{
        AgentBinding,
        tool::{AgentAsTool, AgentTool, AgentToolErrorFunction, AgentToolStreamEvent},
    },
    hook::{UserHookRegistration, UserHooks},
    runner::{RunConfig, RunRequest, RunStreamEvent, Runner},
    thread::{AgentThread, ThreadEvent, TurnAbortReason, TurnInputRequest, TurnInputSubmission},
};
use serde_json::{Value, json};
use tokio::{sync::Notify, time::timeout};

const PATIENCE: Duration = Duration::from_secs(10);
const STOP_REASON: &str = "hook_stopped_compaction";

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

enum Step {
    Respond(ModelResponse),
    After(BoxFuture<'static, ()>, ModelResponse),
}

#[derive(Default)]
struct Script {
    steps: Mutex<VecDeque<Step>>,
    inputs: Mutex<Vec<Vec<ModelInputItem>>>,
    called: Notify,
}

impl Script {
    fn new(steps: impl IntoIterator<Item = Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            ..Self::default()
        })
    }

    fn push(&self, step: Step) {
        self.steps.lock().unwrap().push_back(step);
    }

    fn inputs(&self) -> Vec<Vec<ModelInputItem>> {
        self.inputs.lock().unwrap().clone()
    }

    async fn wait_for_calls(&self, count: usize) {
        timeout(PATIENCE, async {
            loop {
                let called = self.called.notified();
                if self.inputs.lock().unwrap().len() >= count {
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
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(Scripted(Arc::clone(&self.0))) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct Scripted(Arc<Script>);

#[async_trait]
impl Model for Scripted {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.0.inputs.lock().unwrap().push(request.input().to_vec());
        self.0.called.notify_waiters();
        let step = self.0.steps.lock().unwrap().pop_front();
        match step {
            Some(Step::Respond(response)) => Ok(response),
            Some(Step::After(gate, response)) => {
                gate.await;
                Ok(response)
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

fn tool_call(id: &str, name: &str, arguments: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), name, arguments)),
    )])
}

fn gate() -> (Arc<Notify>, BoxFuture<'static, ()>) {
    let gate = Arc::new(Notify::new());
    let waiter = Arc::clone(&gate);
    (gate, async move { waiter.notified().await }.boxed())
}

/// Compacts on its `fire_on`-th invocation (counting from one), announcing the attempt and the
/// result as the framework's compaction does.
struct Compactor {
    fire_on: usize,
    calls: AtomicUsize,
}

impl Compactor {
    fn new(fire_on: usize) -> Arc<Self> {
        Arc::new(Self {
            fire_on,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ContextProcessor for Compactor {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 != self.fire_on {
            return Ok(ContextProcessorResult::new(request.input().to_vec()));
        }
        request
            .notify_pre_compact(CompactTrigger::Automatic)
            .await?;
        let compaction = Compaction::new("the compacted summary", Vec::new());
        request
            .notify_post_compact(CompactTrigger::Automatic, &compaction)
            .await?;
        Ok(
            ContextProcessorResult::new(request.input().to_vec()).with_generated_items(vec![
                RunItem::new(
                    request.record_id().clone(),
                    RunItemKind::Compaction(compaction),
                ),
            ]),
        )
    }
}

/// A later processor that counts the times it runs.
#[derive(Default)]
struct Counting(AtomicUsize);

#[async_trait]
impl ContextProcessor for Counting {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ContextProcessorResult::new(request.input().to_vec()))
    }
}

/// Stops at one compaction event and records every event it sees.
struct StopAt {
    event: HookEventName,
    seen: Mutex<Vec<String>>,
    /// Cancelled with a user interrupt before the stop is answered, as an interrupt racing it.
    interrupt_first: Option<CancelScope>,
}

impl StopAt {
    fn new(event: HookEventName) -> Arc<Self> {
        Arc::new(Self {
            event,
            seen: Mutex::new(Vec::new()),
            interrupt_first: None,
        })
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn count(&self, label: &str) -> usize {
        self.seen().iter().filter(|seen| *seen == label).count()
    }

    fn hooks(self: &Arc<Self>) -> UserHooks {
        [
            HookEventName::PreCompact,
            HookEventName::PostCompact,
            HookEventName::Interrupt,
            HookEventName::SessionStart,
        ]
        .into_iter()
        .fold(
            UserHooks::default().with_session_id(SessionId::new("thread-1")),
            |hooks, event| {
                hooks.with_hook(UserHookRegistration::new(
                    event,
                    Arc::clone(self) as Arc<dyn UserHook>,
                ))
            },
        )
    }
}

#[async_trait]
impl UserHook for StopAt {
    fn name(&self) -> &str {
        "stop-at"
    }

    async fn call(
        &self,
        _context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        let label = match event {
            HookEvent::Interrupt { reason } => format!("interrupt:{}", reason.code()),
            HookEvent::SessionStart { source, .. } => format!("session_start:{}", source.as_str()),
            event => event.name().as_str().to_owned(),
        };
        self.seen.lock().unwrap().push(label);
        if event.name() != self.event {
            return Ok(HookDecision::Continue);
        }
        if let Some(scope) = &self.interrupt_first {
            scope.cancel(CancelReason::UserInterrupt);
        }
        Ok(HookDecision::Deny {
            message: "compaction is not allowed now".to_owned(),
        })
    }
}

fn agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .build()
        .unwrap()
}

fn thread(script: &Arc<Script>, config: RunConfig) -> AgentThread {
    AgentThread::builder(
        SessionId::new("thread-1"),
        AgentBinding::direct(agent()),
        Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
    )
    .with_config(config)
    .build()
}

async fn start(thread: &AgentThread, text: &str) -> String {
    match thread
        .start_or_steer_turn(TurnInputRequest::new(vec![
            ra_core::item::ContentBlock::text(text),
        ]))
        .await
    {
        TurnInputSubmission::Started { turn_id } => turn_id,
        other => panic!("the turn did not start: {other:?}"),
    }
}

/// The thread's events up to and including its turn's end.
async fn until_turn_end(thread: &AgentThread) -> Vec<ThreadEvent> {
    let mut events = Vec::new();
    loop {
        let event = timeout(PATIENCE, thread.next_event())
            .await
            .expect("the thread reports the turn")
            .expect("the thread is alive");
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

fn compactions(events: &[ThreadEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(event, ThreadEvent::Item { item, .. }
                if matches!(item.kind(), RunItemKind::Compaction(_)))
        })
        .count()
}

fn warnings(events: &[ThreadEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            ThreadEvent::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn texts(history: &[ModelInputItem]) -> Vec<String> {
    history
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) => Some(message.text_content()),
            _ => None,
        })
        .collect()
}

fn no_interrupt_marker(history: &[ModelInputItem]) -> bool {
    !history.iter().any(|item| match item {
        ModelInputItem::Message(message) => is_interrupted_turn_marker(message),
        _ => false,
    })
}

// ---------------------------------------------------------------------------------------------
// A thread's turn
// ---------------------------------------------------------------------------------------------

/// Ported from Codex's PreCompact `Stopped`: nothing is compacted, the turn ends aborted as an
/// interrupt ends it — without an error and without the interrupted-turn marker — and its Interrupt
/// hooks run once. The turn's input stays in the history.
#[tokio::test]
async fn a_pre_compact_stop_aborts_the_turn_without_compacting() {
    let script = Script::new([]);
    let hook = StopAt::new(HookEventName::PreCompact);
    let thread = thread(
        &script,
        RunConfig::new()
            .with_context_processor(Compactor::new(1))
            .with_user_hooks(hook.hooks()),
    );

    start(&thread, "please compact").await;
    let events = until_turn_end(&thread).await;

    assert!(matches!(
        events.last(),
        Some(ThreadEvent::TurnAborted {
            reason: TurnAbortReason::Interrupted,
            ..
        })
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ThreadEvent::Error { .. }))
    );
    assert_eq!(compactions(&events), 0);
    assert!(script.inputs().is_empty(), "the model is never called");
    assert_eq!(hook.count(&format!("interrupt:{STOP_REASON}")), 1);
    assert_eq!(hook.count("post_compact"), 0);
    let history = thread.history();
    assert!(texts(&history).contains(&"please compact".to_owned()));
    assert!(no_interrupt_marker(&history));
}

/// Ported from Codex's PostCompact `Stopped`: the compaction stands in the history and the rollout,
/// the turn ends aborted with its Interrupt hooks run once, a later processor no longer runs under
/// the stopped turn, and SessionStart for the compaction runs at the start of the next turn rather
/// than in the stopped one. The next turn's model request is built on the compacted history.
#[tokio::test]
async fn a_post_compact_stop_keeps_the_compaction_and_defers_session_start() {
    let script = Script::new([]);
    let hook = StopAt::new(HookEventName::PostCompact);
    let later = Arc::new(Counting::default());
    let thread = thread(
        &script,
        RunConfig::new()
            .with_context_processor(Compactor::new(1))
            .with_context_processor(later.clone())
            .with_user_hooks(hook.hooks()),
    );

    start(&thread, "please compact").await;
    let events = until_turn_end(&thread).await;

    assert!(matches!(
        events.last(),
        Some(ThreadEvent::TurnAborted {
            reason: TurnAbortReason::Interrupted,
            ..
        })
    ));
    assert_eq!(compactions(&events), 1);
    assert_eq!(later.0.load(Ordering::SeqCst), 0);
    assert!(script.inputs().is_empty());
    assert_eq!(hook.count(&format!("interrupt:{STOP_REASON}")), 1);
    assert_eq!(hook.count("session_start:compact"), 0);
    assert!(no_interrupt_marker(&thread.history()));

    script.push(Step::Respond(final_message("answer", "done")));
    start(&thread, "go on").await;
    let events = until_turn_end(&thread).await;

    assert!(matches!(
        events.last(),
        Some(ThreadEvent::TurnComplete { .. })
    ));
    assert_eq!(hook.count("session_start:compact"), 1);
    let seen = hook.seen();
    let session_start = seen
        .iter()
        .rposition(|seen| seen == "session_start:compact")
        .unwrap();
    assert!(
        session_start
            > seen
                .iter()
                .rposition(|seen| seen.starts_with("interrupt"))
                .unwrap(),
        "{seen:?}"
    );
    let request = &script.inputs()[0];
    assert!(
        request
            .iter()
            .any(|item| matches!(item, ModelInputItem::Compaction(_))),
        "{request:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// An agent-tool run
// ---------------------------------------------------------------------------------------------

fn worker() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .instructions("work")
        .build()
        .unwrap()
}

fn orchestrator(tool: AgentTool) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("orchestrator"))
        .name("Orchestrator")
        .instructions("orchestrate")
        .tool(Arc::new(tool))
        .build()
        .unwrap()
}

fn delegate(
    tool: AgentTool,
    script: &Arc<Script>,
    hook: &Arc<StopAt>,
    cancel: CancelScope,
) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(orchestrator(tool)),
        Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
        RunId::new("run-parent"),
        cancel,
        vec![ModelInputItem::Message(Message::user("please delegate"))],
    )
    .with_config(
        RunConfig::new()
            // The parent processes its context first; the second invocation is the nested run's.
            .with_context_processor(Compactor::new(2))
            .with_user_hooks(hook.hooks()),
    )
}

fn tool_outputs(input: &[ModelInputItem]) -> Vec<String> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) => Some(output.output().to_string()),
            _ => None,
        })
        .collect()
}

/// By default the parent agent is shown the failure and carries on; nothing above the nested run
/// was cancelled, so no Interrupt hook runs. Both compaction events stop the nested run alone.
#[tokio::test]
async fn a_nested_stop_is_a_tool_failure_the_parent_agent_sees() {
    for event in [HookEventName::PreCompact, HookEventName::PostCompact] {
        let script = Script::new([
            Step::Respond(tool_call("p-1", "worker", json!({"input": "x"}))),
            Step::Respond(final_message("p-2", "recovered")),
        ]);
        let hook = StopAt::new(event);
        let result = Runner::run(delegate(
            worker().as_tool().build().unwrap(),
            &script,
            &hook,
            CancelScope::root(),
        ))
        .await
        .unwrap();

        assert_eq!(result.final_text(), "recovered", "{event:?}");
        let outputs = tool_outputs(&script.inputs()[1]);
        assert_eq!(outputs.len(), 1, "{event:?}");
        assert!(
            outputs[0].contains("tool.execution_failed"),
            "{event:?}: {outputs:?}"
        );
        assert!(
            !hook.seen().iter().any(|seen| seen.starts_with("interrupt")),
            "{event:?}: {:?}",
            hook.seen()
        );
    }
}

/// The same failure reaches the parent through a streamed nested run, and a nested PostCompact stop
/// keeps the compaction the nested run produced.
#[tokio::test]
async fn a_streamed_nested_stop_is_a_tool_failure_and_keeps_its_compaction() {
    for event in [HookEventName::PreCompact, HookEventName::PostCompact] {
        let script = Script::new([
            Step::Respond(tool_call("p-1", "worker", json!({"input": "x"}))),
            Step::Respond(final_message("p-2", "recovered")),
        ]);
        let hook = StopAt::new(event);
        let streamed = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&streamed);
        let handler = move |event: AgentToolStreamEvent| -> Result<()> {
            if let RunStreamEvent::Item(item) = event.event() {
                sink.lock().unwrap().push(item.clone());
            }
            Ok(())
        };
        let tool = worker()
            .as_tool()
            .on_stream(Arc::new(handler))
            .build()
            .unwrap();
        let result = Runner::run(delegate(tool, &script, &hook, CancelScope::root()))
            .await
            .unwrap();

        assert_eq!(result.final_text(), "recovered", "{event:?}");
        let compacted = streamed
            .lock()
            .unwrap()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::Compaction(_)));
        assert_eq!(compacted, event == HookEventName::PostCompact, "{event:?}");
    }
}

struct Apologize;

#[async_trait]
impl AgentToolErrorFunction for Apologize {
    async fn error_message(&self, context: &ToolContext<'_>, error: &Error) -> Result<String> {
        Ok(format!(
            "{} stopped: {}",
            context.origin().name(),
            error.code()
        ))
    }
}

/// A custom `failure_error_function` renders the failure, as the reference's custom handler does.
#[tokio::test]
async fn a_nested_stop_goes_through_a_custom_failure_function() {
    let script = Script::new([
        Step::Respond(tool_call("p-1", "worker", json!({"input": "x"}))),
        Step::Respond(final_message("p-2", "handled")),
    ]);
    let hook = StopAt::new(HookEventName::PreCompact);
    let tool = worker()
        .as_tool()
        .failure_error_function(Arc::new(Apologize))
        .build()
        .unwrap();
    let result = Runner::run(delegate(tool, &script, &hook, CancelScope::root()))
        .await
        .unwrap();

    assert_eq!(result.final_text(), "handled");
    let outputs = tool_outputs(&script.inputs()[1]);
    assert!(
        outputs[0].contains("worker stopped: tool.execution_failed"),
        "{outputs:?}"
    );
}

/// Propagation fails the parent run with the tool failure, as the reference's
/// `failure_error_function=None` re-raises: not as a cancellation.
#[tokio::test]
async fn a_propagated_nested_stop_fails_the_parent_rather_than_cancelling_it() {
    let script = Script::new([Step::Respond(tool_call(
        "p-1",
        "worker",
        json!({"input": "x"}),
    ))]);
    let hook = StopAt::new(HookEventName::PreCompact);
    let tool = worker().as_tool().propagate_failures().build().unwrap();
    let error = Runner::run(delegate(tool, &script, &hook, CancelScope::root()))
        .await
        .unwrap_err();

    assert!(!error.is_cancelled(), "{error}");
    assert!(
        error.to_string().contains("compacted its context"),
        "{error}"
    );
}

/// An interrupt that cancels the parent before the stop is answered keeps its cancellation: the
/// parent does not carry on past it.
#[tokio::test]
async fn an_interrupt_racing_a_nested_stop_stays_a_cancellation() {
    let script = Script::new([
        Step::Respond(tool_call("p-1", "worker", json!({"input": "x"}))),
        Step::Respond(final_message("p-2", "should not run")),
    ]);
    let cancel = CancelScope::root();
    let hook = Arc::new(StopAt {
        event: HookEventName::PreCompact,
        seen: Mutex::new(Vec::new()),
        interrupt_first: Some(cancel.clone()),
    });
    let error = Runner::run(delegate(
        worker().as_tool().build().unwrap(),
        &script,
        &hook,
        cancel,
    ))
    .await
    .unwrap_err();

    assert!(error.is_cancelled(), "{error}");
    assert_eq!(script.inputs().len(), 1, "the parent never asks again");
}

/// In a thread, a nested stop leaves the turn to complete normally: the parent agent continues
/// and no Interrupt hook runs for the turn.
#[tokio::test]
async fn a_nested_stop_in_a_thread_completes_the_turn() {
    let script = Script::new([
        Step::Respond(tool_call("p-1", "worker", json!({"input": "x"}))),
        Step::Respond(final_message("p-2", "recovered")),
    ]);
    let hook = StopAt::new(HookEventName::PreCompact);
    let thread = AgentThread::builder(
        SessionId::new("thread-1"),
        AgentBinding::direct(orchestrator(worker().as_tool().build().unwrap())),
        Arc::new(Resolver(Arc::clone(&script))) as Arc<dyn ModelResolver>,
    )
    .with_config(
        RunConfig::new()
            // The turn's run processes its context first; the second invocation is the nested run's.
            .with_context_processor(Compactor::new(2))
            .with_user_hooks(hook.hooks()),
    )
    .build();
    start(&thread, "please delegate").await;
    let events = until_turn_end(&thread).await;

    assert!(
        matches!(events.last(), Some(ThreadEvent::TurnComplete { last_agent_message: Some(message), .. }) if message == "recovered"),
        "{events:?}"
    );
    assert!(!hook.seen().iter().any(|seen| seen.starts_with("interrupt")));
    assert_eq!(hook.count("pre_compact"), 1);
}

// ---------------------------------------------------------------------------------------------
// Background results
// ---------------------------------------------------------------------------------------------

/// Captures the turn's context queue, as a product delivering background results does.
#[derive(Default)]
struct Capture(Mutex<Option<HookContextQueue>>);

impl Capture {
    fn queue(&self) -> HookContextQueue {
        self.0
            .lock()
            .unwrap()
            .clone()
            .expect("a turn bound its queue")
    }
}

#[async_trait]
impl UserHook for Capture {
    fn name(&self) -> &str {
        "capture"
    }

    fn bind_context(&self, run: &ra_core::context::RunContext, output: &HookContextQueue) {
        if run.parent_run_id().is_none() {
            *self.0.lock().unwrap() = Some(output.clone());
        }
    }

    async fn call(&self, _: &UserHookContext<'_>, _: &HookEvent<'_>) -> Result<HookDecision> {
        Ok(HookDecision::Continue)
    }
}

fn capture_thread(script: &Arc<Script>) -> (AgentThread, Arc<Capture>) {
    let capture = Arc::new(Capture::default());
    let thread = thread(
        script,
        RunConfig::new().with_user_hook(UserHookRegistration::new(
            HookEventName::Stop,
            Arc::clone(&capture) as Arc<dyn UserHook>,
        )),
    );
    (thread, capture)
}

/// Codex drains finished background hooks after sampling: their warning is shown and their context
/// reaches the next model request of the running turn.
#[tokio::test]
async fn a_background_warning_is_shown_where_its_context_is_delivered() {
    let (opened, wait) = gate();
    // Background context that arrives during the turn's last model call has the turn continue.
    let script = Script::new([
        Step::After(wait, final_message("first", "first answer")),
        Step::Respond(final_message("second", "done")),
    ]);
    let (thread, capture) = capture_thread(&script);

    start(&thread, "work").await;
    script.wait_for_calls(1).await;
    capture
        .queue()
        .push_background_with_warnings(
            vec![Message::developer("background context")],
            vec!["background warning".to_owned()],
        )
        .unwrap();
    opened.notify_one();
    let events = until_turn_end(&thread).await;

    assert_eq!(warnings(&events), vec!["background warning".to_owned()]);
    assert!(texts(&script.inputs()[1]).contains(&"background context".to_owned()));
}

/// An interrupted turn records the queued background context and shows its warning before the turn
/// ends; a thread shutting down drops both, while context from a hook the run waited for stays.
#[tokio::test]
async fn queued_background_results_are_kept_on_interrupt_and_dropped_on_shutdown() {
    for shutting_down in [false, true] {
        let (_opened, wait) = gate();
        let script = Script::new([Step::After(wait, final_message("answer", "never"))]);
        let (thread, capture) = capture_thread(&script);

        start(&thread, "work").await;
        script.wait_for_calls(1).await;
        let queue = capture.queue();
        queue
            .push_background_with_warnings(
                vec![Message::developer("background context")],
                vec!["background warning".to_owned()],
            )
            .unwrap();
        queue
            .push(vec![Message::developer("waited-for context")])
            .unwrap();
        let events = if shutting_down {
            let shutdown = thread.shutdown();
            let (events, ()) = tokio::join!(until_turn_end(&thread), shutdown);
            events
        } else {
            assert!(thread.interrupt());
            until_turn_end(&thread).await
        };

        let history = texts(&thread.history());
        assert!(history.contains(&"waited-for context".to_owned()));
        if shutting_down {
            assert!(warnings(&events).is_empty(), "{events:?}");
            assert!(!history.contains(&"background context".to_owned()));
        } else {
            assert_eq!(warnings(&events), vec!["background warning".to_owned()]);
            assert!(history.contains(&"background context".to_owned()));
        }
    }
}

/// Background results that arrived between turns are drained before the new prompt: their context
/// is recorded ahead of it and their warning shown before the turn's model call.
#[tokio::test]
async fn background_results_waiting_for_a_turn_are_shown_before_its_prompt() {
    let script = Script::new([Step::Respond(final_message("answer", "done"))]);
    let pending = Arc::new(Mutex::new(Some((
        vec![Message::developer("earlier background context")],
        vec!["earlier background warning".to_owned()],
    ))));
    let thread = thread(
        &script,
        RunConfig::new().with_user_hook(UserHookRegistration::new(
            HookEventName::UserPromptSubmit,
            Arc::new(DeliverOnBind {
                pending: Arc::clone(&pending),
            }),
        )),
    );

    start(&thread, "new prompt").await;
    let events = until_turn_end(&thread).await;

    assert_eq!(
        warnings(&events),
        vec!["earlier background warning".to_owned()]
    );
    let request = texts(&script.inputs()[0]);
    let context = request
        .iter()
        .position(|text| text == "earlier background context")
        .expect("the context reaches the model");
    let new_prompt = request
        .iter()
        .position(|text| text == "new prompt")
        .unwrap();
    assert!(context < new_prompt, "{request:?}");
}

/// Delivers results that arrived between turns as a host does: when the next turn binds its queue.
struct DeliverOnBind {
    pending: Arc<Mutex<Option<(Vec<Message>, Vec<String>)>>>,
}

#[async_trait]
impl UserHook for DeliverOnBind {
    fn name(&self) -> &str {
        "deliver-on-bind"
    }

    fn bind_context(&self, run: &ra_core::context::RunContext, output: &HookContextQueue) {
        if run.parent_run_id().is_none()
            && let Some((messages, warnings)) = self.pending.lock().unwrap().take()
        {
            output
                .push_background_with_warnings(messages, warnings)
                .unwrap();
        }
    }

    async fn call(&self, _: &UserHookContext<'_>, _: &HookEvent<'_>) -> Result<HookDecision> {
        Ok(HookDecision::Continue)
    }
}

// ---------------------------------------------------------------------------------------------
// A run without a thread
// ---------------------------------------------------------------------------------------------

/// Pushes context into the run's queue while the model is called, then fails with a checkpoint,
/// which the runner fills with the run's state.
struct FailingAfterContext {
    capture: Arc<Capture>,
}

#[async_trait]
impl Model for FailingAfterContext {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        self.capture
            .queue()
            .push(vec![Message::developer("accepted context")])
            .unwrap();
        Err(Error::caller("the model failed")
            .with_run_state(ra_core::state::RunState::start(RunId::new("run-sdk"))))
    }
}

struct FailingResolver(Arc<Capture>);

impl ModelResolver for FailingResolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(FailingAfterContext {
                capture: Arc::clone(&self.0),
            }) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// A run nothing owns keeps context it accepted but never delivered in the checkpoint its error
/// carries, for whoever resumes from it; only a thread drains it from the queue itself.
#[tokio::test]
async fn a_failed_runs_checkpoint_keeps_its_undelivered_context() {
    let capture = Arc::new(Capture::default());
    let error = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent()),
            Arc::new(FailingResolver(Arc::clone(&capture))) as Arc<dyn ModelResolver>,
            RunId::new("run-sdk"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("hello"))],
        )
        .with_config(RunConfig::new().with_user_hook(UserHookRegistration::new(
            HookEventName::Stop,
            Arc::clone(&capture) as Arc<dyn UserHook>,
        ))),
    )
    .await
    .unwrap_err();

    let state = error.run_state().expect("the error carries a checkpoint");
    assert_eq!(
        state.clone().take_hook_context(),
        vec![Message::developer("accepted context")]
    );
}

// ---------------------------------------------------------------------------------------------
// The input a concluding turn was sampled with
// ---------------------------------------------------------------------------------------------

/// Projects the model input down to its last message, as a compaction or filter may.
struct LastMessageOnly;

#[async_trait]
impl ContextProcessor for LastMessageOnly {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        let input = request.input();
        Ok(ContextProcessorResult::new(
            input.last().cloned().into_iter().collect(),
        ))
    }
}

/// Records the user messages of the input each Stop says the turn was sampled with.
#[derive(Default)]
struct SampledAtStop(Mutex<Vec<Vec<String>>>);

#[async_trait]
impl UserHook for SampledAtStop {
    fn name(&self) -> &str {
        "sampled-at-stop"
    }

    async fn call(&self, _: &UserHookContext<'_>, event: &HookEvent<'_>) -> Result<HookDecision> {
        if let HookEvent::Stop(delivery) = event {
            self.0
                .lock()
                .unwrap()
                .push(texts(delivery.sampled_input().unwrap_or_default()));
        }
        Ok(HookDecision::Continue)
    }
}

/// Stop is told the input the concluding turn was actually sampled with: a thread's earlier turns
/// while they are in it, and not what a context projection removed.
#[tokio::test]
async fn stop_hooks_see_the_input_the_turn_was_sampled_with() {
    for projected in [false, true] {
        let script = Script::new([
            Step::Respond(final_message("first", "first answer")),
            Step::Respond(final_message("second", "second answer")),
        ]);
        let stop = Arc::new(SampledAtStop::default());
        let mut config = RunConfig::new().with_user_hook(UserHookRegistration::new(
            HookEventName::Stop,
            Arc::clone(&stop) as Arc<dyn UserHook>,
        ));
        if projected {
            config = config.with_context_processor(Arc::new(LastMessageOnly));
        }
        let thread = thread(&script, config);

        start(&thread, "first prompt").await;
        until_turn_end(&thread).await;
        start(&thread, "second prompt").await;
        until_turn_end(&thread).await;

        let seen = stop.0.lock().unwrap().clone();
        let expected: Vec<String> = if projected {
            vec!["second prompt".to_owned()]
        } else {
            vec![
                "first prompt".to_owned(),
                "first answer".to_owned(),
                "second prompt".to_owned(),
            ]
        };
        assert_eq!(seen.last(), Some(&expected), "projected: {projected}");
        assert_eq!(
            texts(&script.inputs()[1]),
            expected,
            "the hook saw exactly what the model was sent"
        );
    }
}
