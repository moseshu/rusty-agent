//! A run recorded into its session's rollout, as Codex records a turn: its start and input, the
//! context of its first model call, its session records, the host events attributed to it, the
//! usage of each model call, and how it ended.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::{StreamExt, future::BoxFuture, stream};
use ra_core::{
    agent::{AgentId, AgentSpec, control::AgentPath},
    cancel::{CancelReason, CancelScope},
    error::{Error, ProviderErrorKind, Result},
    event::{
        AgentEvent, HostEventBody, HostEventSink, InMemoryHostEventSink,
        file::{FileChangeKind, FileChangedEvent, FileEvent, FileReadEvent},
    },
    finish::FinishReason,
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    item::{
        CallId, InputItemNormalizer, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase,
        RunItem, RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelRetryAdviceRequest,
        ModelRetrySettings, ModelSelector, ModelSettings, ModelStream, ModelStreamEvent,
        NetworkErrorRetryPolicy, NormalizedProviderError, ProviderKey, ReplaySafety, ResolvedModel,
        RetryAdvice, RetryBackoffSettings,
    },
    session::{
        SessionId,
        rollout::{
            RolloutItem, RolloutRecorder, RolloutRunEnd, RolloutThreadSpawn, RolloutThreadStore,
        },
    },
    state::RunId,
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
        ToolServices,
    },
    usage::{RequestUsage, Usage},
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry, control::AgentControl, tool::AgentAsTool},
    hook::UserHookRegistration,
    runner::{RunConfig, RunRequest, RunStreamEvent, Runner},
};
use ra_tools::agent_ns::{WaitAgentTimeoutOptions, collaboration_tools_with};
use serde_json::{Value, json};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

enum Step {
    Respond(ModelResponse),
    After(BoxFuture<'static, ()>, ModelResponse),
    Hang,
}

#[derive(Default)]
struct Scripts {
    steps: Mutex<HashMap<String, VecDeque<Step>>>,
    /// The input of every request each agent's model was sent, in order.
    requests: Mutex<HashMap<String, Vec<Vec<ModelInputItem>>>>,
}

impl Scripts {
    fn push(&self, agent: &str, step: Step) {
        self.steps
            .lock()
            .unwrap()
            .entry(agent.to_owned())
            .or_default()
            .push_back(step);
    }

    fn requests(&self, agent: &str) -> Vec<Vec<ModelInputItem>> {
        self.requests
            .lock()
            .unwrap()
            .get(agent)
            .cloned()
            .unwrap_or_default()
    }
}

struct ScriptedResolver(Arc<Scripts>);

impl ModelResolver for ScriptedResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(ScriptedModel(Arc::clone(&self.0))) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ScriptedModel(Arc<Scripts>);

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let agent = request.system_instructions().unwrap_or_default().to_owned();
        self.0
            .requests
            .lock()
            .unwrap()
            .entry(agent.clone())
            .or_default()
            .push(request.input().to_vec());
        let step = self
            .0
            .steps
            .lock()
            .unwrap()
            .get_mut(&agent)
            .and_then(VecDeque::pop_front);
        match step {
            Some(Step::Respond(response)) => Ok(response),
            Some(Step::After(effect, response)) => {
                effect.await;
                Ok(response)
            }
            Some(Step::Hang) => std::future::pending().await,
            None => Err(Error::caller(format!("`{agent}` ran out of responses"))),
        }
    }
}

fn billed(response: ModelResponse, input_tokens: u64) -> ModelResponse {
    response.with_usage(Usage::from_request(RequestUsage::new(input_tokens, 1)))
}

fn final_message(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn tool_call(id: &str, call_id: &str, name: &str, arguments: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, arguments)),
    )])
}

fn empty_schema(name: &str) -> ToolSchema {
    ToolSchema::new(
        name,
        json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
    )
    .unwrap()
}

/// A tool that reads and then changes a file, reporting both on the host event channel.
struct TouchTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl TouchTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("touch").unwrap(),
            schema: empty_schema("touch"),
        }
    }
}

#[async_trait]
impl Tool for TouchTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::default()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        if let Some(emitter) = context.event_emitter() {
            let call_id = context.call_id().clone();
            emitter.emit_file(FileEvent::Read(FileReadEvent::new(
                call_id.clone(),
                "notes.md",
                12,
            )))?;
            emitter.emit_file(FileEvent::Changed(FileChangedEvent::new(
                call_id,
                "notes.md",
                FileChangeKind::Updated,
            )))?;
        }
        Ok(ToolOutput::text("touched"))
    }
}

/// A tool that always needs approval.
struct GuardedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl GuardedTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("deploy").unwrap(),
            schema: empty_schema("deploy"),
        }
    }
}

#[async_trait]
impl Tool for GuardedTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::default().with_approval(ToolApprovalPolicy::Always)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("deployed"))
    }
}

/// Keeps what it is given, in order.
#[derive(Default)]
struct MemoryRecorder {
    items: Mutex<Vec<RolloutItem>>,
    flushes: Mutex<usize>,
    fail_flush: bool,
}

impl MemoryRecorder {
    fn failing() -> Self {
        Self {
            fail_flush: true,
            ..Self::default()
        }
    }

    fn items(&self) -> Vec<RolloutItem> {
        self.items.lock().unwrap().clone()
    }

    fn flushes(&self) -> usize {
        *self.flushes.lock().unwrap()
    }
}

#[async_trait]
impl RolloutRecorder for MemoryRecorder {
    fn record(&self, item: RolloutItem) {
        self.items.lock().unwrap().push(item);
    }

    async fn flush(&self) -> Result<()> {
        *self.flushes.lock().unwrap() += 1;
        if self.fail_flush {
            return Err(Error::caller("the disk is full"));
        }
        Ok(())
    }
}

fn agent(id: &str, tools: Vec<Arc<dyn Tool>>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name(id)
        .instructions(id)
        .tools(tools)
        .build()
        .unwrap()
}

fn request(
    scripts: &Arc<Scripts>,
    agent: Arc<AgentSpec>,
    run_id: &str,
    recorder: &Arc<MemoryRecorder>,
) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(ScriptedResolver(Arc::clone(scripts))) as Arc<dyn ModelResolver>,
        RunId::new(run_id),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("do it"))],
    )
    .with_rollout_recorder(Arc::clone(recorder) as Arc<dyn RolloutRecorder>)
}

/// A compact label for each recorded item, so a sequence reads as a timeline.
fn label(item: &RolloutItem) -> String {
    match item {
        RolloutItem::RunStarted(started) => format!("run_started {}", started.run_id()),
        RolloutItem::TurnContext(context) => format!("turn_context {}", context.run_id()),
        RolloutItem::Item(item) => match item.kind() {
            RunItemKind::ToolCall(call) => format!("item tool_call {}", call.name()),
            RunItemKind::ToolCallOutput(_) => "item tool_output".to_owned(),
            RunItemKind::Message(message) => format!("item message {}", message.text_content()),
            RunItemKind::ToolApproval(_) => "item tool_approval".to_owned(),
            other => format!("item {other:?}"),
        },
        RolloutItem::Event(event) => match event.body() {
            HostEventBody::File(FileEvent::Read(_)) => {
                format!("event file_read {}", event.run_id())
            }
            HostEventBody::File(FileEvent::Changed(_)) => {
                format!("event file_changed {}", event.run_id())
            }
            HostEventBody::Agent(AgentEvent::SubAgentActivity(activity)) => format!(
                "event {} {} {}",
                activity.activity(),
                activity.agent_path(),
                event.run_id()
            ),
            other => format!("event {other:?}"),
        },
        RolloutItem::ModelUsage(usage) => format!("model_usage {}", usage.run_id()),
        RolloutItem::RunEnded(ended) => format!("run_ended {} {:?}", ended.run_id(), ended.end()),
        other => format!("{other:?}"),
    }
}

fn labels(recorder: &MemoryRecorder) -> Vec<String> {
    recorder.items().iter().map(label).collect()
}

fn recorded_usage(recorder: &MemoryRecorder) -> Usage {
    recorder
        .items()
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ModelUsage(usage) => Some(usage.usage().clone()),
            _ => None,
        })
        .fold(Usage::default(), |total, usage| total.accumulate(&usage))
}

fn ended(recorder: &MemoryRecorder) -> ra_core::session::rollout::RolloutRunEnded {
    recorder
        .items()
        .into_iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::RunEnded(ended) => Some(ended),
            _ => None,
        })
        .expect("the run's end is recorded")
}

async fn eventually(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// What a run records
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_recorded_run_records_its_start_context_records_events_usage_and_end_in_order() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    scripts.push(
        "lead",
        Step::Respond(billed(tool_call("l-1", "l-1-call", "touch", json!({})), 10)),
    );
    scripts.push(
        "lead",
        Step::Respond(billed(final_message("l-2", "done"), 5)),
    );

    let result = Runner::run(request(
        &scripts,
        agent("lead", vec![Arc::new(TouchTool::new())]),
        "run-1",
        &recorder,
    ))
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");

    assert_eq!(
        labels(&recorder),
        vec![
            "run_started run-1",
            "turn_context run-1",
            "model_usage run-1",
            // The call is recorded before its tool runs, so its effects follow it.
            "item tool_call touch",
            "event file_read run-1",
            "event file_changed run-1",
            "item tool_output",
            "model_usage run-1",
            "item message done",
            "run_ended run-1 Completed",
        ]
    );
    let items = recorder.items();
    let RolloutItem::RunStarted(started) = &items[0] else {
        panic!("the run's start comes first");
    };
    assert_eq!(started.agent_id().as_str(), "lead");
    assert_eq!(
        started.input(),
        &[ModelInputItem::Message(Message::user("do it"))]
    );
    assert!(started.parent_run_id().is_none());
    let RolloutItem::TurnContext(context) = &items[1] else {
        panic!("the turn context follows the start");
    };
    assert_eq!(context.model(), Some("canonical-model"));
    assert_eq!(ended(&recorder).finish_reason(), Some(FinishReason::Final));
    // The usage records add up to the run's ledger.
    assert_eq!(recorded_usage(&recorder), result.usage());
    assert_eq!(recorder.flushes(), 1);
}

#[tokio::test]
async fn the_rollout_records_exactly_the_session_records_the_stream_reports() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    scripts.push(
        "lead",
        Step::Respond(tool_call("l-1", "l-1-call", "touch", json!({}))),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "done")));

    let mut stream = Runner::run_streamed(request(
        &scripts,
        agent("lead", vec![Arc::new(TouchTool::new())]),
        "run-1",
        &recorder,
    ));
    let mut streamed = Vec::new();
    while let Some(event) = stream.next_event().await {
        if let RunStreamEvent::Item(item) = event {
            streamed.push(item);
        }
    }
    let result = stream.finish().await.unwrap();

    let recorded: Vec<RunItem> = recorder
        .items()
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::Item(item) => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(recorded, streamed);
    assert_eq!(recorded, result.new_items());
}

#[tokio::test]
async fn events_are_recorded_without_a_host_sink_and_still_reach_one_that_is_installed() {
    for with_sink in [false, true] {
        let scripts = Arc::new(Scripts::default());
        let recorder = Arc::new(MemoryRecorder::default());
        let sink = InMemoryHostEventSink::new();
        scripts.push(
            "lead",
            Step::Respond(tool_call("l-1", "l-1-call", "touch", json!({}))),
        );
        scripts.push("lead", Step::Respond(final_message("l-2", "done")));
        let mut run = request(
            &scripts,
            agent("lead", vec![Arc::new(TouchTool::new())]),
            "run-1",
            &recorder,
        );
        if with_sink {
            run = run.with_services(ToolServices::new().with_event_sink(Arc::new(sink.clone())));
        }
        Runner::run(run).await.unwrap();

        let recorded = recorder
            .items()
            .iter()
            .filter(|item| matches!(item, RolloutItem::Event(_)))
            .count();
        assert_eq!(recorded, 2, "with_sink = {with_sink}");
        assert_eq!(sink.len(), if with_sink { 2 } else { 0 });
    }
}

// ---------------------------------------------------------------------------------------------
// How a run ends
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_run_paused_for_approval_ends_interrupted_and_its_continuation_starts_again_under_the_same_run()
 {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    let lead = agent("lead", vec![Arc::new(GuardedTool::new())]);
    scripts.push(
        "lead",
        Step::Respond(tool_call("l-1", "l-1-call", "deploy", json!({}))),
    );
    let first = Runner::run(request(&scripts, Arc::clone(&lead), "run-1", &recorder))
        .await
        .unwrap();
    assert_eq!(ended(&recorder).end(), RolloutRunEnd::Interrupted);
    let paused_at = recorder.items().len();

    let mut state = first.state().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    scripts.push("lead", Step::Respond(final_message("l-2", "shipped")));
    let continued = RunRequest::new(
        AgentBinding::direct(lead),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-1"),
        CancelScope::root(),
        Vec::new(),
    )
    .with_state(state)
    .with_rollout_recorder(Arc::clone(&recorder) as Arc<dyn RolloutRecorder>);
    assert_eq!(
        Runner::run(continued).await.unwrap().final_text(),
        "shipped"
    );

    let after: Vec<String> = labels(&recorder)[paused_at..].to_vec();
    assert_eq!(after.first().unwrap(), "run_started run-1");
    assert!(after.contains(&"item tool_output".to_owned()));
    assert_eq!(after.last().unwrap(), "run_ended run-1 Completed");
    let RolloutItem::RunStarted(restarted) = &recorder.items()[paused_at] else {
        panic!("the continuation records its start");
    };
    assert!(restarted.input().is_empty());
    assert!(!restarted.input_is_continuation_base());
}

#[tokio::test]
async fn cancelled_and_failed_runs_record_how_they_ended() {
    // Cancelled while its model call is in flight.
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    scripts.push("lead", Step::Hang);
    let cancel = CancelScope::root();
    let run = RunRequest::new(
        AgentBinding::direct(agent("lead", Vec::new())),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-cancelled"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("do it"))],
    )
    .with_rollout_recorder(Arc::clone(&recorder) as Arc<dyn RolloutRecorder>);
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel(CancelReason::UserInterrupt);
    });
    assert!(Runner::run(run).await.unwrap_err().is_cancelled());
    canceller.await.unwrap();
    assert_eq!(ended(&recorder).end(), RolloutRunEnd::Cancelled);
    assert_eq!(recorder.flushes(), 1);

    // Failed: the model has nothing to answer with.
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    let error = Runner::run(request(
        &scripts,
        agent("lead", Vec::new()),
        "run-failed",
        &recorder,
    ))
    .await
    .unwrap_err();
    let failed = ended(&recorder);
    assert_eq!(failed.end(), RolloutRunEnd::Failed);
    assert_eq!(failed.error(), Some(error.to_string().as_str()));
}

#[tokio::test]
async fn a_flush_that_fails_is_logged_and_the_run_still_succeeds() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::failing());
    scripts.push("lead", Step::Respond(final_message("l-1", "done")));
    let result = Runner::run(request(
        &scripts,
        agent("lead", Vec::new()),
        "run-1",
        &recorder,
    ))
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(recorder.flushes(), 1);
}

// ---------------------------------------------------------------------------------------------
// Only the recorded run
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_nested_agent_tool_run_is_not_recorded_but_the_usage_it_bills_to_the_run_is() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    let nested = agent("nested", vec![Arc::new(TouchTool::new())]);
    let lead = agent("lead", vec![Arc::new(nested.as_tool().build().unwrap())]);
    scripts.push(
        "lead",
        Step::Respond(billed(
            tool_call("l-1", "l-1-call", "nested", json!({"input": "look"})),
            10,
        )),
    );
    scripts.push(
        "nested",
        Step::Respond(billed(tool_call("n-1", "n-1-call", "touch", json!({})), 20)),
    );
    scripts.push(
        "nested",
        Step::Respond(billed(final_message("n-2", "looked"), 30)),
    );
    scripts.push(
        "lead",
        Step::Respond(billed(final_message("l-2", "done"), 40)),
    );

    let result = Runner::run(request(&scripts, lead, "run-1", &recorder))
        .await
        .unwrap();

    let recorded = labels(&recorder);
    // The nested run's own records and events are not this run's: only the agent-tool call and its
    // output are, as in the run's result.
    assert!(
        recorded
            .iter()
            .all(|label| label.contains(" run-1") || label.starts_with("item")),
        "{recorded:?}"
    );
    assert!(!recorded.iter().any(|label| label.contains("touch")));
    assert!(!recorded.iter().any(|label| label.starts_with("event")));
    assert_eq!(
        recorded
            .iter()
            .filter(|label| label.starts_with("item"))
            .count(),
        result.new_items().len()
    );
    // Its usage is billed to this run, and recorded with it.
    assert_eq!(recorded_usage(&recorder), result.usage());
    assert_eq!(result.usage().input_tokens(), 100);
}

#[tokio::test]
async fn a_spawned_agents_events_stay_out_while_its_late_completion_is_recorded() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    let control = AgentControl::new();
    let worker_turn = Arc::new(Notify::new());
    let collaboration: Vec<Arc<dyn Tool>> =
        collaboration_tools_with(WaitAgentTimeoutOptions::new().with_bounds(2_000, 0, 60_000))
            .unwrap()
            .into_iter()
            .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
            .collect();
    let mut worker_tools = vec![Arc::new(TouchTool::new()) as Arc<dyn Tool>];
    worker_tools.extend(collaboration.iter().cloned());
    let registry = AgentRegistry::builder()
        .register(agent("worker", worker_tools))
        .build()
        .unwrap();

    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-1",
            "l-1-call",
            "spawn_agent",
            json!({"task_name": "worker", "message": "touch the notes", "agent_type": "worker"}),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    let gate = Arc::clone(&worker_turn);
    scripts.push(
        "worker",
        Step::After(
            Box::pin(async move { gate.notified().await }),
            tool_call("w-1", "w-1-call", "touch", json!({})),
        ),
    );
    scripts.push("worker", Step::Respond(final_message("w-2", "touched")));

    let run = request(&scripts, agent("lead", collaboration), "run-1", &recorder)
        .with_config(RunConfig::new().with_agent_registry(registry))
        .with_agent_control(control.root());
    Runner::run(run).await.unwrap();
    worker_turn.notify_one();
    let worker = AgentPath::root().join("worker").unwrap();
    eventually(
        || {
            labels(&recorder)
                .iter()
                .any(|label| label.starts_with("event completed"))
        },
        "the worker's completion is recorded",
    )
    .await;

    let recorded = labels(&recorder);
    let end = recorded
        .iter()
        .position(|label| label == "run_ended run-1 Completed")
        .unwrap();
    assert_eq!(
        recorded[end + 1..].to_vec(),
        vec![format!("event completed {worker} run-1")],
        "the completion arrives after the run ended, on its timeline"
    );
    assert!(recorded.contains(&format!("event started {worker} run-1")));
    // The worker's file events went to its own runs, not this one.
    assert!(!recorded.iter().any(|label| label.starts_with("event file")));
    control.shutdown().await;
}

#[tokio::test]
async fn a_run_started_by_another_run_records_its_parent() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    scripts.push("lead", Step::Respond(final_message("l-1", "done")));
    let run = request(&scripts, agent("lead", Vec::new()), "run-child", &recorder)
        .with_parent_run_id(RunId::new("run-parent"))
        .unwrap();
    Runner::run(run).await.unwrap();
    let RolloutItem::RunStarted(started) = &recorder.items()[0] else {
        panic!("the run's start comes first");
    };
    assert_eq!(
        started.parent_run_id().map(RunId::as_str),
        Some("run-parent")
    );
}

#[tokio::test]
async fn a_run_without_a_recorder_records_nothing_anywhere() {
    // The control: no recorder, no change to what the host's sink receives.
    let scripts = Arc::new(Scripts::default());
    let sink = InMemoryHostEventSink::new();
    scripts.push(
        "lead",
        Step::Respond(tool_call("l-1", "l-1-call", "touch", json!({}))),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "done")));
    let run = RunRequest::new(
        AgentBinding::direct(agent("lead", vec![Arc::new(TouchTool::new())])),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-1"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("do it"))],
    )
    .with_services(ToolServices::new().with_event_sink(Arc::new(sink.clone())));
    Runner::run(run).await.unwrap();
    assert_eq!(sink.len(), 2);
}

#[tokio::test]
async fn a_run_recorded_into_a_rollout_file_reads_back_as_its_timeline() {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("rollout_recording_file");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let writer = ra_session::RolloutWriter::create_for_session(
        &dir,
        ra_core::session::SessionId::new("session-1"),
    )
    .await
    .unwrap();
    let path = writer.path().to_path_buf();
    let recorder: Arc<dyn RolloutRecorder> =
        Arc::new(ra_session::RolloutFileRecorder::spawn(writer));

    let scripts = Arc::new(Scripts::default());
    scripts.push(
        "lead",
        Step::Respond(billed(tool_call("l-1", "l-1-call", "touch", json!({})), 10)),
    );
    scripts.push(
        "lead",
        Step::Respond(billed(final_message("l-2", "done"), 5)),
    );
    let result = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent("lead", vec![Arc::new(TouchTool::new())])),
            Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
            RunId::new("run-1"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("do it"))],
        )
        .with_rollout_recorder(recorder),
    )
    .await
    .unwrap();

    // The run flushed before returning, so the file already holds everything.
    let reader = ra_session::RolloutReader::open(&path);
    let records = reader.read_all().await.unwrap();
    let types: Vec<&str> = records.iter().map(|record| record.type_name()).collect();
    assert_eq!(
        types,
        vec![
            "run_started",
            "turn_context",
            "model_usage",
            "item",
            // The file read is narration and is left out; the change is kept.
            "event",
            "item",
            "model_usage",
            "item",
            "run_ended",
        ]
    );
    let totals = reader.scan_summary().await.unwrap();
    assert_eq!(
        totals.usage_totals().input_tokens(),
        result.usage().input_tokens()
    );
    assert_eq!(totals.usage_totals().requests(), result.usage().requests());
}

// ---------------------------------------------------------------------------------------------
// A call is recorded before its tool runs
// ---------------------------------------------------------------------------------------------

/// A tool that reports a file change and then works until it is cancelled.
struct BlockingTouchTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    reported: Arc<Notify>,
}

impl BlockingTouchTool {
    fn new(reported: &Arc<Notify>) -> Self {
        Self {
            origin: ToolOrigin::new("rewrite").unwrap(),
            schema: empty_schema("rewrite"),
            reported: Arc::clone(reported),
        }
    }
}

#[async_trait]
impl Tool for BlockingTouchTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::default()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        if let Some(emitter) = context.event_emitter() {
            emitter.emit_file(FileEvent::Changed(FileChangedEvent::new(
                context.call_id().clone(),
                "notes.md",
                FileChangeKind::Updated,
            )))?;
        }
        self.reported.notify_one();
        std::future::pending().await
    }
}

/// One model call, as stream events; `Hang` keeps the stream open until the call is cancelled.
enum StreamStep {
    Event(Result<ModelStreamEvent>),
    Hang,
}

/// A model that answers each call with the next scripted stream, and calls a failed attempt safe
/// to replay.
struct StreamScriptModel(Mutex<VecDeque<Vec<StreamStep>>>);

#[async_trait]
impl Model for StreamScriptModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("this fixture only streams"))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        let steps = self.0.lock().unwrap().pop_front().unwrap_or_default();
        stream::unfold(VecDeque::from(steps), |mut steps| async move {
            match steps.pop_front()? {
                StreamStep::Event(event) => Some((event, steps)),
                StreamStep::Hang => std::future::pending().await,
            }
        })
        .boxed()
    }

    fn get_retry_advice(&self, _request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
        Some(RetryAdvice::new().with_replay_safety(ReplaySafety::Safe))
    }
}

struct FixedResolver(Arc<dyn Model>);

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn narrated(item: &RunItem) -> StreamStep {
    StreamStep::Event(Ok(ModelStreamEvent::RunItem(
        ra_core::model::RunItemStreamEvent::new("item", item.clone()),
    )))
}

fn completed(response: ModelResponse) -> StreamStep {
    StreamStep::Event(Ok(ModelStreamEvent::Completed(Box::new(response))))
}

fn commentary(id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Commentary)),
    )
}

fn call_item(id: &str, call_id: &str, name: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, json!({}))),
    )
}

/// The call id of the file event and of each recorded tool call, in recorded order.
fn call_ids_in_order(recorder: &MemoryRecorder) -> Vec<String> {
    recorder
        .items()
        .iter()
        .filter_map(|item| match item {
            RolloutItem::Item(item) => match item.kind() {
                RunItemKind::ToolCall(call) => Some(format!("call {}", call.call_id())),
                _ => None,
            },
            RolloutItem::Event(event) => match event.body() {
                HostEventBody::File(FileEvent::Changed(changed)) => {
                    Some(format!("effect {}", changed.call_id()))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

async fn cancel_once_reported(cancel: CancelScope, reported: Arc<Notify>) {
    reported.notified().await;
    cancel.cancel(CancelReason::UserInterrupt);
}

#[tokio::test]
async fn a_run_cancelled_while_a_settlement_started_tool_runs_keeps_the_call_its_effect_answers() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    let reported = Arc::new(Notify::new());
    let response = ModelResponse::new(vec![
        commentary("l-0", "Rewriting the notes."),
        call_item("l-1", "l-1-call", "rewrite"),
    ]);
    scripts.push("lead", Step::Respond(billed(response, 10)));
    let cancel = CancelScope::root();
    let run = RunRequest::new(
        AgentBinding::direct(agent(
            "lead",
            vec![Arc::new(BlockingTouchTool::new(&reported))],
        )),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-1"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("do it"))],
    )
    .with_rollout_recorder(Arc::clone(&recorder) as Arc<dyn RolloutRecorder>);
    let canceller = tokio::spawn(cancel_once_reported(cancel, Arc::clone(&reported)));
    assert!(Runner::run(run).await.unwrap_err().is_cancelled());
    canceller.await.unwrap();

    assert_eq!(
        labels(&recorder),
        vec![
            "run_started run-1",
            "turn_context run-1",
            "model_usage run-1",
            "item message Rewriting the notes.",
            "item tool_call rewrite",
            "event file_changed run-1",
            "run_ended run-1 Cancelled",
        ]
    );
    assert_eq!(
        call_ids_in_order(&recorder),
        vec!["call l-1-call", "effect l-1-call"]
    );
    // Recorded as settlement would have attributed it.
    let call = recorder
        .items()
        .into_iter()
        .find_map(|item| match item {
            RolloutItem::Item(item) if matches!(item.kind(), RunItemKind::ToolCall(_)) => {
                Some(item)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(call.provenance().unwrap().agent_id().as_str(), "lead");
}

#[tokio::test]
async fn a_run_cancelled_while_a_stream_started_tool_runs_keeps_the_call_its_effect_answers() {
    let recorder = Arc::new(MemoryRecorder::default());
    let reported = Arc::new(Notify::new());
    // The call starts from the stream; the response itself never completes.
    let model = Arc::new(StreamScriptModel(Mutex::new(VecDeque::from(vec![vec![
        narrated(&commentary("l-0", "Rewriting the notes.")),
        narrated(&call_item("l-1", "l-1-call", "rewrite")),
        StreamStep::Hang,
    ]]))));
    let cancel = CancelScope::root();
    let run = RunRequest::new(
        AgentBinding::direct(agent(
            "lead",
            vec![Arc::new(BlockingTouchTool::new(&reported))],
        )),
        Arc::new(FixedResolver(model)) as Arc<dyn ModelResolver>,
        RunId::new("run-1"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("do it"))],
    )
    .with_rollout_recorder(Arc::clone(&recorder) as Arc<dyn RolloutRecorder>);
    let canceller = tokio::spawn(cancel_once_reported(cancel, Arc::clone(&reported)));
    assert!(Runner::run(run).await.unwrap_err().is_cancelled());
    canceller.await.unwrap();

    assert_eq!(
        labels(&recorder),
        vec![
            "run_started run-1",
            "turn_context run-1",
            "item message Rewriting the notes.",
            "item tool_call rewrite",
            "event file_changed run-1",
            "run_ended run-1 Cancelled",
        ]
    );
}

#[tokio::test]
async fn a_streamed_call_recorded_early_is_not_recorded_again_when_its_turn_settles() {
    let recorder = Arc::new(MemoryRecorder::default());
    let call = call_item("l-1", "l-1-call", "touch");
    let note = commentary("l-0", "Touching the notes.");
    let model = Arc::new(StreamScriptModel(Mutex::new(VecDeque::from(vec![
        vec![
            narrated(&note),
            narrated(&call),
            completed(ModelResponse::new(vec![note.clone(), call.clone()])),
        ],
        vec![completed(final_message("l-2", "done"))],
    ]))));
    let mut stream = Runner::run_streamed(
        RunRequest::new(
            AgentBinding::direct(agent("lead", vec![Arc::new(TouchTool::new())])),
            Arc::new(FixedResolver(model)) as Arc<dyn ModelResolver>,
            RunId::new("run-1"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("do it"))],
        )
        .with_rollout_recorder(Arc::clone(&recorder) as Arc<dyn RolloutRecorder>),
    );
    let mut streamed = Vec::new();
    while let Some(event) = stream.next_event().await {
        if let RunStreamEvent::Item(item) = event {
            streamed.push(item);
        }
    }
    let result = stream.finish().await.unwrap();

    let recorded: Vec<RunItem> = recorder
        .items()
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::Item(item) => Some(item),
            _ => None,
        })
        .collect();
    // The same records the session holds, once each and in its order — the early copies included.
    assert_eq!(recorded, result.new_items());
    assert_eq!(recorded, streamed);
    assert_eq!(
        call_ids_in_order(&recorder),
        vec!["call l-1-call", "effect l-1-call"]
    );
}

#[tokio::test]
async fn items_streamed_by_an_attempt_that_is_retried_are_never_recorded() {
    let recorder = Arc::new(MemoryRecorder::default());
    let stale = commentary("stale", "This attempt will fail.");
    let call = call_item("l-1", "l-1-call", "touch");
    let model = Arc::new(StreamScriptModel(Mutex::new(VecDeque::from(vec![
        vec![
            narrated(&stale),
            StreamStep::Event(Err(NormalizedProviderError::new(
                ProviderErrorKind::Network,
                "connection reset",
            )
            .into_error())),
        ],
        vec![
            narrated(&call),
            completed(ModelResponse::new(vec![call.clone()])),
        ],
        vec![completed(final_message("l-2", "done"))],
    ]))));
    let retry = ModelRetrySettings::new()
        .with_max_retries(1)
        .with_backoff(
            RetryBackoffSettings::new()
                .with_initial_delay(Duration::ZERO)
                .with_jitter(false),
        )
        .with_policy(Arc::new(NetworkErrorRetryPolicy));
    let result = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent("lead", vec![Arc::new(TouchTool::new())])),
            Arc::new(FixedResolver(model)) as Arc<dyn ModelResolver>,
            RunId::new("run-1"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("do it"))],
        )
        .with_config(RunConfig::new().with_model_settings(ModelSettings::new().with_retry(retry)))
        .with_rollout_recorder(Arc::clone(&recorder) as Arc<dyn RolloutRecorder>),
    )
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert!(
        !labels(&recorder)
            .iter()
            .any(|label| label.contains("This attempt will fail.")),
        "{:?}",
        labels(&recorder)
    );
}

// ---------------------------------------------------------------------------------------------
// Rebuilt from the file
// ---------------------------------------------------------------------------------------------

/// Runs one streamed request to its end and returns its result, or the error it ended with.
async fn run_streamed(request: RunRequest) -> Result<ra_runtime::runner::RunResult> {
    let mut stream = Runner::run_streamed(request);
    while stream.next_event().await.is_some() {}
    stream.finish().await
}

#[tokio::test]
async fn a_session_rebuilt_from_its_rollout_file_is_what_its_runs_continue_from() {
    use ra_core::item::MessageRole;
    use ra_runtime::runner::ContinuationInput;

    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("rollout_recording_rebuilt");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let writer = ra_session::RolloutWriter::create_for_session(
        &dir,
        ra_core::session::SessionId::new("session-1"),
    )
    .await
    .unwrap();
    let path = writer.path().to_path_buf();
    let recorder: Arc<dyn RolloutRecorder> =
        Arc::new(ra_session::RolloutFileRecorder::spawn(writer));

    // The first run's message arrives without a phase: it is recorded before its call's tool
    // starts, and recorded again under its id once settlement gives it one.
    let unphased = RunItem::new(
        ItemId::new("l-0"),
        RunItemKind::Message(Message::text(MessageRole::Assistant, "Touching the notes.")),
    );
    let touch = call_item("l-1", "l-1-call", "touch");
    let rewrite = call_item("r-1", "r-1-call", "rewrite");
    let model = Arc::new(StreamScriptModel(Mutex::new(VecDeque::from(vec![
        vec![
            narrated(&unphased),
            narrated(&touch),
            completed(ModelResponse::new(vec![unphased.clone(), touch.clone()])),
        ],
        vec![completed(final_message("l-2", "done"))],
        // The second run reuses an item id of the first: ids are only unique within a run.
        vec![completed(final_message("l-2", "again"))],
        vec![narrated(&rewrite), StreamStep::Hang],
    ]))));
    let reported = Arc::new(Notify::new());
    let lead = agent(
        "lead",
        vec![
            Arc::new(TouchTool::new()),
            Arc::new(BlockingTouchTool::new(&reported)),
        ],
    );
    let request = |run: &str, text: &str, cancel: CancelScope| {
        RunRequest::new(
            AgentBinding::direct(Arc::clone(&lead)),
            Arc::new(FixedResolver(Arc::clone(&model) as Arc<dyn Model>)) as Arc<dyn ModelResolver>,
            RunId::new(run),
            cancel,
            vec![ModelInputItem::Message(Message::user(text))],
        )
        .with_rollout_recorder(Arc::clone(&recorder))
    };

    let first = run_streamed(request("run-1", "do it", CancelScope::root()))
        .await
        .unwrap();
    let second = run_streamed(request("run-2", "and again", CancelScope::root()))
        .await
        .unwrap();
    let cancel = CancelScope::root();
    let canceller = tokio::spawn(cancel_once_reported(cancel.clone(), Arc::clone(&reported)));
    assert!(
        run_streamed(request("run-3", "rewrite them", cancel))
            .await
            .unwrap_err()
            .is_cancelled()
    );
    canceller.await.unwrap();

    let after_first = first.continuation_input(ContinuationInput::PreserveAll);
    let mut after_second = after_first.clone();
    after_second.extend(second.continuation_input(ContinuationInput::PreserveAll));
    let mut after_third = after_second.clone();
    after_third.push(ModelInputItem::Message(Message::user("rewrite them")));
    after_third.extend(rewrite.to_model_input());

    let records = ra_session::RolloutReader::open(&path)
        .read_all()
        .await
        .unwrap();
    let rebuilt = ra_session::reconstruct_history(&records).unwrap();
    assert_eq!(rebuilt.history(), after_third);
    let last = rebuilt.last_run().unwrap();
    assert_eq!(last.run_id(), &RunId::new("run-3"));
    assert_eq!(last.end().unwrap().end(), RolloutRunEnd::Cancelled);
    assert_eq!(
        rebuilt.turn_context().unwrap().model(),
        Some("canonical-model")
    );

    let through_second =
        ra_session::truncate_rollout_after_run(records.clone(), &RunId::new("run-2")).unwrap();
    assert_eq!(
        ra_session::reconstruct_history(&through_second)
            .unwrap()
            .history(),
        after_second
    );
    let before_second =
        ra_session::truncate_rollout_before_run(records, &RunId::new("run-2")).unwrap();
    assert_eq!(
        ra_session::reconstruct_history(&before_second)
            .unwrap()
            .history(),
        after_first
    );
}

#[tokio::test]
async fn a_run_continued_on_its_callers_projection_is_rebuilt_without_repeating_it() {
    use ra_runtime::runner::ContinuationInput;

    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("rollout_recording_continued");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let writer = ra_session::RolloutWriter::create_for_session(
        &dir,
        ra_core::session::SessionId::new("session-1"),
    )
    .await
    .unwrap();
    let path = writer.path().to_path_buf();
    let recorder: Arc<dyn RolloutRecorder> =
        Arc::new(ra_session::RolloutFileRecorder::spawn(writer));
    let scripts = Arc::new(Scripts::default());
    scripts.push("lead", Step::Respond(final_message("l-1", "answer 1")));
    scripts.push("lead", Step::Respond(final_message("l-2", "answer 2")));
    let lead = agent("lead", Vec::new());
    let request = |input: Vec<ModelInputItem>| {
        RunRequest::new(
            AgentBinding::direct(Arc::clone(&lead)),
            Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
            RunId::new("run-1"),
            CancelScope::root(),
            input,
        )
        .with_rollout_recorder(Arc::clone(&recorder))
    };

    let first = Runner::run(request(vec![ModelInputItem::Message(Message::user(
        "user 1",
    ))]))
    .await
    .unwrap();
    // The documented way to add a turn to a checkpointed run: its projection, then the new message.
    let mut input = first.continuation_input(ContinuationInput::default());
    input.push(ModelInputItem::Message(Message::user("user 2")));
    let second = Runner::run(request(input).with_state(first.state().clone()))
        .await
        .unwrap();
    assert_eq!(second.final_text(), "answer 2");

    let records = ra_session::RolloutReader::open(&path)
        .read_all()
        .await
        .unwrap();
    let starts: Vec<bool> = records
        .iter()
        .filter_map(|record| match record.payload().unwrap() {
            ra_session::RolloutPayload::RunStarted(started) => {
                Some(started.input_is_continuation_base())
            }
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![false, true]);

    let answer = |id: &str, text: &str| final_message(id, text).output()[0].to_model_input();
    let expected: Vec<ModelInputItem> = [
        Some(ModelInputItem::Message(Message::user("user 1"))),
        answer("l-1", "answer 1"),
        Some(ModelInputItem::Message(Message::user("user 2"))),
        answer("l-2", "answer 2"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let rebuilt = ra_session::reconstruct_history(&records).unwrap();
    assert_eq!(rebuilt.history(), expected);
    assert_eq!(
        rebuilt.history(),
        second.continuation_input(ContinuationInput::PreserveAll)
    );
}

// ---------------------------------------------------------------------------------------------
// A spawned agent records into a rollout of its own
// ---------------------------------------------------------------------------------------------

/// Gives every spawned agent's thread a recorder of its own, and keeps what it was told.
#[derive(Default)]
struct MemoryThreadStore {
    threads: Mutex<Vec<(SessionId, RolloutThreadSpawn, Arc<MemoryRecorder>)>>,
    refuse: bool,
}

impl MemoryThreadStore {
    fn refusing() -> Self {
        Self {
            refuse: true,
            ..Self::default()
        }
    }

    fn threads(&self) -> Vec<(SessionId, RolloutThreadSpawn, Arc<MemoryRecorder>)> {
        self.threads.lock().unwrap().clone()
    }
}

#[async_trait]
impl RolloutThreadStore for MemoryThreadStore {
    async fn create_thread(
        &self,
        session_id: &SessionId,
        spawn: &RolloutThreadSpawn,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        if self.refuse {
            return Err(Error::caller("the disk is full"));
        }
        let recorder = Arc::new(MemoryRecorder::default());
        self.threads.lock().unwrap().push((
            session_id.clone(),
            spawn.clone(),
            Arc::clone(&recorder),
        ));
        Ok(recorder)
    }
}

/// On the first sub-agent completion it sees, notes what the spawned agents' rollouts held then.
struct CompletionWitness {
    store: Arc<MemoryThreadStore>,
    seen: Mutex<Option<Vec<Vec<String>>>>,
}

impl HostEventSink for CompletionWitness {
    fn emit(&self, event: ra_core::event::HostEvent) {
        if let HostEventBody::Agent(AgentEvent::SubAgentActivity(activity)) = event.body()
            && activity.activity().as_str() == "completed"
        {
            let mut seen = self.seen.lock().unwrap();
            if seen.is_none() {
                *seen = Some(
                    self.store
                        .threads()
                        .iter()
                        .map(|(_, _, recorder)| labels(recorder))
                        .collect(),
                );
            }
        }
    }
}

fn collaboration_tools() -> Vec<Arc<dyn Tool>> {
    collaboration_tools_with(WaitAgentTimeoutOptions::new().with_bounds(2_000, 0, 60_000))
        .unwrap()
        .into_iter()
        .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
        .collect()
}

fn spawn_call(id: &str, task_name: &str, agent_type: &str) -> ModelResponse {
    tool_call(
        id,
        &format!("{id}-call"),
        "spawn_agent",
        json!({"task_name": task_name, "message": format!("do the {task_name} work"), "agent_type": agent_type}),
    )
}

fn followup_call(id: &str, target: &str, message: &str) -> ModelResponse {
    tool_call(
        id,
        &format!("{id}-call"),
        "followup_task",
        json!({"target": target, "message": message}),
    )
}

/// The sub-agent activity a rollout recorded: what happened, to which agent, and its session.
fn activities(recorder: &MemoryRecorder) -> Vec<(String, AgentPath, Option<SessionId>)> {
    recorder
        .items()
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::Event(event) => match event.body() {
                HostEventBody::Agent(AgentEvent::SubAgentActivity(activity)) => Some((
                    activity.activity().to_string(),
                    activity.agent_path().clone(),
                    activity.agent_session_id().cloned(),
                )),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn started_inputs(records: &[ra_session::RolloutRecord]) -> Vec<Vec<ModelInputItem>> {
    records
        .iter()
        .filter_map(|record| match record.payload().unwrap() {
            ra_session::RolloutPayload::RunStarted(started) => Some(started.input().to_vec()),
            _ => None,
        })
        .collect()
}

fn normalized(items: &[ModelInputItem]) -> Vec<ModelInputItem> {
    InputItemNormalizer::new()
        .normalize_model_items(items)
        .unwrap()
        .into_items()
}

async fn rollout_records(path: &std::path::Path) -> Vec<ra_session::RolloutRecord> {
    ra_session::RolloutReader::open(path)
        .read_all()
        .await
        .unwrap()
}

/// Waits until the rollout at `path` has recorded the end of `runs` runs.
async fn wait_for_ended_runs(path: &std::path::Path, runs: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ended = rollout_records(path)
            .await
            .iter()
            .filter(|record| record.type_name() == "run_ended")
            .count();
        if ended >= runs {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {runs} runs to end in {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("rusty_agent_tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn a_spawned_agent_records_its_runs_into_a_rollout_of_its_own_tied_to_its_parent() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    let store = Arc::new(MemoryThreadStore::default());
    let root_session = SessionId::new("session-root");
    let control = AgentControl::new()
        .with_rollout_store(
            Arc::clone(&store) as Arc<dyn RolloutThreadStore>,
            root_session.clone(),
        )
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(agent("worker", vec![Arc::new(TouchTool::new())]))
        .build()
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_call("l-1", "worker", "worker")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "touch", json!({}))),
    );
    scripts.push("worker", Step::Respond(final_message("w-2", "touched")));
    let witness = Arc::new(CompletionWitness {
        store: Arc::clone(&store),
        seen: Mutex::new(None),
    });

    let run = request(
        &scripts,
        agent("lead", collaboration_tools()),
        "run-1",
        &recorder,
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_services(
        ToolServices::new().with_event_sink(Arc::clone(&witness) as Arc<dyn HostEventSink>),
    )
    .with_agent_control(control.root());
    Runner::run(run).await.unwrap();
    let worker = AgentPath::root().join("worker").unwrap();
    eventually(
        || {
            activities(&recorder)
                .iter()
                .any(|(activity, _, _)| activity == "completed")
        },
        "the worker's completion is recorded on the lead's timeline",
    )
    .await;

    let threads = store.threads();
    assert_eq!(threads.len(), 1, "one rollout for the one spawned agent");
    let (session, spawn, child) = &threads[0];
    assert_eq!(spawn.root_session_id(), &root_session);
    assert_eq!(spawn.parent_session_id(), &root_session);
    assert_eq!(spawn.depth(), 1);
    assert_eq!(spawn.agent_path(), &worker);
    assert_eq!(spawn.agent_type(), Some(&AgentId::new("worker")));

    // The worker's run is in its own rollout, whole, and ends there before its parent hears.
    let recorded = labels(child);
    let RolloutItem::RunStarted(started) = &child.items()[0] else {
        panic!("the worker's rollout starts with its run");
    };
    let worker_run = started.run_id().clone();
    assert_eq!(started.parent_run_id(), Some(&RunId::new("run-1")));
    assert!(recorded.contains(&"item tool_call touch".to_owned()));
    assert!(recorded.contains(&format!("event file_changed {worker_run}")));
    assert_eq!(
        recorded.last().unwrap(),
        &format!("run_ended {worker_run} Completed")
    );
    assert!(child.flushes() >= 1);
    // Its end was written before its parent was told it completed.
    let seen = witness.seen.lock().unwrap().clone().unwrap();
    assert_eq!(
        seen[0].last(),
        Some(&format!("run_ended {worker_run} Completed"))
    );
    // And none of it is in the lead's.
    assert!(
        !labels(&recorder)
            .iter()
            .any(|label| label.contains(worker_run.as_str()))
    );

    // The lead's timeline names the worker's session, as the tree lists it.
    assert_eq!(
        activities(&recorder),
        vec![
            ("started".to_owned(), worker.clone(), Some(session.clone())),
            (
                "completed".to_owned(),
                worker.clone(),
                Some(session.clone())
            ),
        ]
    );
    let agents = control.agents();
    assert_eq!(agents[0].session_id(), Some(&root_session));
    assert_eq!(agents[1].session_id(), Some(session));
    control.shutdown().await;
}

#[tokio::test]
async fn each_run_of_a_spawned_agent_records_only_what_its_rollout_does_not_hold_yet() {
    let dir = temp_dir("rollout_recording_subagent_followups");
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(&dir));
    let root_session = SessionId::new("session-root");
    let control = AgentControl::new()
        .with_rollout_store(
            Arc::clone(&store) as Arc<dyn RolloutThreadStore>,
            root_session.clone(),
        )
        .unwrap();
    let scripts = Arc::new(Scripts::default());
    let registry = AgentRegistry::builder()
        .register(agent("worker", vec![Arc::new(TouchTool::new())]))
        .build()
        .unwrap();
    let lead = agent("lead", collaboration_tools());
    let root_recorder = Arc::new(MemoryRecorder::default());
    let root_run = |run: &str| {
        RunRequest::new(
            AgentBinding::direct(Arc::clone(&lead)),
            Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
            RunId::new(run),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(format!(
                "{run} task"
            )))],
        )
        .with_config(RunConfig::new().with_agent_registry(registry.clone()))
        .with_agent_control(control.root())
        .with_rollout_recorder(Arc::clone(&root_recorder) as Arc<dyn RolloutRecorder>)
    };

    // Forked with the lead's history on its first run, then two follow-ups, one of them with a
    // tool call.
    scripts.push("lead", Step::Respond(spawn_call("l-1", "worker", "worker")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", Step::Respond(final_message("w-1", "one")));
    Runner::run(root_run("run-1")).await.unwrap();
    let worker_session = control.agents()[1].session_id().cloned().unwrap();
    let path = store.rollout_path(&worker_session).unwrap();
    wait_for_ended_runs(&path, 1).await;

    scripts.push(
        "lead",
        Step::Respond(followup_call("l-3", "worker", "second task")),
    );
    scripts.push("lead", Step::Respond(final_message("l-4", "followed up")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-2", "w-2-call", "touch", json!({}))),
    );
    scripts.push("worker", Step::Respond(final_message("w-3", "two")));
    Runner::run(root_run("run-2")).await.unwrap();
    wait_for_ended_runs(&path, 2).await;

    scripts.push(
        "lead",
        Step::Respond(followup_call("l-5", "worker", "third task")),
    );
    scripts.push(
        "lead",
        Step::Respond(final_message("l-6", "followed up again")),
    );
    scripts.push("worker", Step::Respond(final_message("w-4", "three")));
    Runner::run(root_run("run-3")).await.unwrap();
    wait_for_ended_runs(&path, 3).await;

    // One request for the first run, two for the second, one for the third.
    let requests = scripts.requests("worker");
    assert_eq!(requests.len(), 4);
    let records = rollout_records(&path).await;
    let inputs = started_inputs(&records);
    assert_eq!(inputs.len(), 3);
    // The first run records all it started on: the history it was forked with and its task.
    assert_eq!(inputs[0], requests[0]);
    assert!(inputs[0].len() > 1, "the worker was forked with history");
    let parents: Vec<_> = records
        .iter()
        .filter_map(|record| match record.payload().unwrap() {
            ra_session::RolloutPayload::RunStarted(started) => {
                Some(started.parent_run_id().cloned())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        parents,
        vec![
            Some(RunId::new("run-1")),
            Some(RunId::new("run-2")),
            Some(RunId::new("run-3"))
        ]
    );
    // Each later run records only its mail; the rest is the agent's history, recorded already.
    assert_eq!(inputs[1], vec![requests[1].last().unwrap().clone()]);
    assert_eq!(inputs[2], vec![requests[3].last().unwrap().clone()]);

    // So the rollout rebuilds the history the agent runs on, once.
    let rebuilt = ra_session::reconstruct_history(&records).unwrap();
    let mut expected = requests[3].clone();
    expected.extend(final_message("w-4", "three").output()[0].to_model_input());
    assert_eq!(normalized(rebuilt.history()), normalized(&expected));

    // The follow-ups on the lead's timeline name the session the spawn did.
    let worker = AgentPath::root().join("worker").unwrap();
    let named: Vec<(String, Option<SessionId>)> = activities(&root_recorder)
        .into_iter()
        .filter(|(_, path, _)| path == &worker)
        .map(|(activity, _, session)| (activity, session))
        .collect();
    assert_eq!(
        named
            .iter()
            .filter(|(activity, _)| activity == "interacted")
            .count(),
        2,
        "{named:?}"
    );
    assert!(
        named
            .iter()
            .all(|(_, session)| session.as_ref() == Some(&worker_session)),
        "{named:?}"
    );
    control.shutdown().await;
}

#[tokio::test]
async fn a_spawned_agents_run_stopped_while_it_waits_for_approval_is_recorded_cancelled() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    let store = Arc::new(MemoryThreadStore::default());
    let control = AgentControl::new()
        .with_rollout_store(
            Arc::clone(&store) as Arc<dyn RolloutThreadStore>,
            SessionId::new("session-root"),
        )
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(agent("worker", vec![Arc::new(GuardedTool::new())]))
        .build()
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_call("l-1", "worker", "worker")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "deploy", json!({}))),
    );

    let run = request(
        &scripts,
        agent("lead", collaboration_tools()),
        "run-1",
        &recorder,
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root());
    Runner::run(run).await.unwrap();
    let paused = tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .unwrap();
    let worker_run = paused.state().run_id().clone();
    control.close(paused.path()).await.unwrap();

    let (_, _, child) = &store.threads()[0];
    let recorded = labels(child);
    let ends: Vec<&String> = recorded
        .iter()
        .filter(|label| label.starts_with("run_ended"))
        .collect();
    assert_eq!(
        ends,
        vec![
            &format!("run_ended {worker_run} Interrupted"),
            &format!("run_ended {worker_run} Cancelled"),
        ],
        "the segment paused, then the run was cancelled while it waited"
    );
    assert_eq!(recorded.last(), ends.last().copied());
    assert!(child.flushes() >= 2, "the cancellation was flushed");
    control.shutdown().await;
}

#[tokio::test]
async fn a_spawn_whose_rollout_cannot_be_created_fails_and_leaves_no_agent() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    let control = AgentControl::new()
        .with_rollout_store(
            Arc::new(MemoryThreadStore::refusing()),
            SessionId::new("session-root"),
        )
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(agent("worker", Vec::new()))
        .build()
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_call("l-1", "worker", "worker")));
    scripts.push(
        "lead",
        Step::Respond(final_message("l-2", "could not spawn")),
    );

    let run = request(
        &scripts,
        agent("lead", collaboration_tools()),
        "run-1",
        &recorder,
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root());
    Runner::run(run).await.unwrap();

    assert_eq!(control.agents().len(), 1, "only the root");
    assert!(activities(&recorder).is_empty());
    // The model is told why.
    let lead_requests = scripts.requests("lead");
    let told = serde_json::to_string(lead_requests.last().unwrap()).unwrap();
    assert!(told.contains("could not be created"), "{told}");
    control.shutdown().await;
}

#[tokio::test]
async fn without_a_store_a_spawned_agent_still_has_a_session_naming_its_life() {
    let scripts = Arc::new(Scripts::default());
    let recorder = Arc::new(MemoryRecorder::default());
    let control = AgentControl::new();
    let registry = AgentRegistry::builder()
        .register(agent("worker", Vec::new()))
        .build()
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_call("l-1", "worker", "worker")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", Step::Respond(final_message("w-1", "done")));

    let run = request(
        &scripts,
        agent("lead", collaboration_tools()),
        "run-1",
        &recorder,
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root());
    Runner::run(run).await.unwrap();
    let worker = AgentPath::root().join("worker").unwrap();
    let agents = control.agents();
    assert_eq!(
        agents[0].session_id(),
        None,
        "the tree was not told the root's"
    );
    let session = agents[1]
        .session_id()
        .cloned()
        .expect("a spawned agent has one");
    assert_eq!(
        activities(&recorder)[0],
        ("started".to_owned(), worker, Some(session))
    );
    control.shutdown().await;
}

#[test]
fn a_tree_takes_one_rollout_store() {
    let control = AgentControl::new()
        .with_rollout_store(
            Arc::new(MemoryThreadStore::default()),
            SessionId::new("session-root"),
        )
        .unwrap();
    assert!(
        control
            .with_rollout_store(
                Arc::new(MemoryThreadStore::default()),
                SessionId::new("session-other"),
            )
            .is_err()
    );
}

#[tokio::test]
async fn spawned_agents_rollout_files_name_the_session_they_were_spawned_from() {
    let dir = temp_dir("rollout_recording_subagent_tree");
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(&dir));
    let root_session = SessionId::new("session-root");
    let control = AgentControl::new()
        .with_rollout_store(
            Arc::clone(&store) as Arc<dyn RolloutThreadStore>,
            root_session.clone(),
        )
        .unwrap();
    let root_path = store.rollout_path(&root_session).unwrap();
    let root_recorder: Arc<dyn RolloutRecorder> =
        Arc::new(ra_session::RolloutFileRecorder::create_with_session_meta(
            &root_path,
            ra_session::RolloutSessionMeta::new(root_session.clone()),
        ));
    let scripts = Arc::new(Scripts::default());
    let registry = AgentRegistry::builder()
        .register(agent("worker", collaboration_tools()))
        .register(agent("helper", Vec::new()))
        .build()
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_call("l-1", "worker", "worker")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(spawn_call("w-1", "helper", "helper")),
    );
    scripts.push("worker", Step::Respond(final_message("w-2", "delegated")));
    scripts.push("helper", Step::Respond(final_message("h-1", "helped")));

    Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent("lead", collaboration_tools())),
            Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
            RunId::new("run-1"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("do it"))],
        )
        .with_config(RunConfig::new().with_agent_registry(registry))
        .with_agent_control(control.root())
        .with_rollout_recorder(root_recorder),
    )
    .await
    .unwrap();
    let helper = AgentPath::root()
        .join("worker")
        .unwrap()
        .join("helper")
        .unwrap();
    let helper_session = {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(agent) = control
                .agents()
                .into_iter()
                .find(|agent| agent.agent_path() == &helper)
            {
                break agent.session_id().cloned().unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the helper is spawned"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let helper_path = store.rollout_path(&helper_session).unwrap();
    wait_for_ended_runs(&helper_path, 1).await;

    // The root's rollout is a root's: it was spawned from nothing.
    let root_meta = ra_session::RolloutReader::open(&root_path)
        .session_meta()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root_meta.session_id(), &root_session);
    assert!(root_meta.thread_spawn().is_none());

    // Its one child is the worker, whose own child is the helper, both in the root's tree.
    let children = store.children(&root_session).await.unwrap();
    assert_eq!(children.len(), 1);
    let (worker_reader, worker_meta) = &children[0];
    let worker_session = worker_meta.session_id().clone();
    let worker_spawn = worker_meta.thread_spawn().unwrap();
    assert_eq!(
        worker_spawn.agent_path(),
        &AgentPath::root().join("worker").unwrap()
    );
    assert_eq!(worker_spawn.depth(), 1);
    assert_eq!(worker_spawn.root_session_id(), &root_session);
    assert_eq!(worker_meta.parent_session_id(), Some(&root_session));

    let grandchildren = store.children(&worker_session).await.unwrap();
    assert_eq!(grandchildren.len(), 1);
    let (_, helper_meta) = &grandchildren[0];
    assert_eq!(helper_meta.session_id(), &helper_session);
    let helper_spawn = helper_meta.thread_spawn().unwrap();
    assert_eq!(helper_spawn.agent_path(), &helper);
    assert_eq!(helper_spawn.depth(), 2);
    assert_eq!(helper_spawn.root_session_id(), &root_session);
    assert_eq!(helper_spawn.parent_session_id(), &worker_session);
    assert!(store.children(&helper_session).await.unwrap().is_empty());

    // Each parent's timeline names the session of the agent it started.
    let started_session = |records: &[ra_session::RolloutRecord]| {
        records
            .iter()
            .find_map(|record| match record.payload().unwrap() {
                ra_session::RolloutPayload::Event(event) => match event.body() {
                    HostEventBody::Agent(AgentEvent::SubAgentActivity(activity))
                        if activity.activity().as_str() == "started" =>
                    {
                        activity.agent_session_id().cloned()
                    }
                    _ => None,
                },
                _ => None,
            })
    };
    assert_eq!(
        started_session(&rollout_records(&root_path).await),
        Some(worker_session.clone())
    );
    assert_eq!(
        started_session(&worker_reader.read_all().await.unwrap()),
        Some(helper_session)
    );
    // Every rollout starts with its session's metadata.
    for path in [&root_path, worker_reader.path(), &helper_path] {
        assert_eq!(rollout_records(path).await[0].type_name(), "session_meta");
    }
    control.shutdown().await;
}

/// Notes the name of every hook event it is given.
#[derive(Default)]
struct HookNames(Mutex<Vec<HookEventName>>);

#[async_trait]
impl UserHook for HookNames {
    fn name(&self) -> &str {
        "hook names"
    }

    async fn call(
        &self,
        _context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        self.0.lock().unwrap().push(event.name());
        Ok(HookDecision::default())
    }
}

#[tokio::test]
async fn trigger_mail_sets_run_parents_and_mail_naming_no_run_falls_back_to_the_spawning_run() {
    use ra_core::agent::control::AgentControlPort;

    let scripts = Arc::new(Scripts::default());
    let hooks = Arc::new(HookNames::default());
    let store = Arc::new(MemoryThreadStore::default());
    let control = AgentControl::new()
        .with_rollout_store(store.clone(), SessionId::new("session-root"))
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(agent("worker", vec![Arc::new(GuardedTool::new())]))
        .build()
        .unwrap();
    let root_run = |run: &str| {
        RunRequest::new(
            AgentBinding::direct(agent("lead", collaboration_tools())),
            Arc::new(ScriptedResolver(scripts.clone())),
            RunId::new(run),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("work"))],
        )
        .with_config(
            RunConfig::new()
                .with_agent_registry(registry.clone())
                .with_user_hook(UserHookRegistration::new(
                    HookEventName::Stop,
                    Arc::clone(&hooks) as Arc<dyn UserHook>,
                ))
                .with_user_hook(UserHookRegistration::new(
                    HookEventName::SubagentStop,
                    Arc::clone(&hooks) as Arc<dyn UserHook>,
                )),
        )
        .with_agent_control(control.root())
    };
    scripts.push("lead", Step::Respond(spawn_call("l-1", "worker", "worker")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-call", "deploy", json!({}))),
    );
    Runner::run(root_run("spawn-run")).await.unwrap();
    let paused = tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .unwrap();
    assert_eq!(
        paused.state().parent_run_id(),
        Some(&RunId::new("spawn-run"))
    );

    scripts.push(
        "lead",
        Step::Respond(followup_call("l-3", "worker", "also tag")),
    );
    scripts.push("lead", Step::Respond(final_message("l-4", "sent")));
    Runner::run(root_run("mail-during-pause")).await.unwrap();
    let mut state = paused.state().clone();
    let pending = state.pending_interruption_items().next().unwrap().clone();
    state.approve(&pending, false).unwrap();
    scripts.push("worker", Step::Respond(final_message("w-2", "done")));
    control.resume(paused.path(), state).unwrap();
    let child = store.threads()[0].2.clone();
    eventually(|| child.items().iter().filter(|item| matches!(item, RolloutItem::RunEnded(end) if end.end() == RolloutRunEnd::Completed)).count() == 1, "the resumed worker completed").await;

    // With no running sender, host-triggered mail names no run: the run that spawned the agent is
    // its run's parent, so that run is still a sub-agent's.
    scripts.push(
        "worker",
        Step::Respond(final_message("w-3", "host task done")),
    );
    control
        .root()
        .send(
            "worker",
            "host task".to_owned(),
            ra_core::agent::control::MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap();
    eventually(|| child.items().iter().filter(|item| matches!(item, RolloutItem::RunEnded(end) if end.end() == RolloutRunEnd::Completed)).count() == 2, "the host task completed").await;
    scripts.push(
        "lead",
        Step::Respond(followup_call("l-5", "worker", "another task")),
    );
    scripts.push("lead", Step::Respond(final_message("l-6", "sent")));
    scripts.push(
        "worker",
        Step::Respond(final_message("w-4", "follow-up done")),
    );
    Runner::run(root_run("later-followup")).await.unwrap();
    eventually(
        || child.items().iter().filter(|item| matches!(item, RolloutItem::RunEnded(end) if end.end() == RolloutRunEnd::Completed)).count() == 3,
        "the later follow-up completed",
    ).await;
    let starts: Vec<_> = child
        .items()
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::RunStarted(start) => Some(start),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 4);
    assert_eq!(starts[0].run_id(), starts[1].run_id());
    assert_eq!(starts[0].parent_run_id(), Some(&RunId::new("spawn-run")));
    assert_eq!(starts[1].parent_run_id(), Some(&RunId::new("spawn-run")));
    assert_eq!(starts[2].parent_run_id(), Some(&RunId::new("spawn-run")));
    assert_eq!(
        starts[3].parent_run_id(),
        Some(&RunId::new("later-followup"))
    );
    // Each of the worker's three runs ended as a sub-agent's, and each of the lead's as a root's.
    let names = hooks.0.lock().unwrap().clone();
    let count = |name: HookEventName| names.iter().filter(|seen| **seen == name).count();
    assert_eq!(count(HookEventName::SubagentStop), 3, "{names:?}");
    assert_eq!(count(HookEventName::Stop), 3, "{names:?}");
    control.shutdown().await;
}

#[tokio::test]
async fn trigger_mail_from_different_runs_falls_back_to_the_spawning_run() {
    use ra_core::agent::control::AgentControlPort;

    let scripts = Arc::new(Scripts::default());
    let store = Arc::new(MemoryThreadStore::default());
    let control = AgentControl::new()
        .with_rollout_store(store.clone(), SessionId::new("session-root"))
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(agent("worker", Vec::new()))
        .build()
        .unwrap();
    let root_run = |run: &str| {
        RunRequest::new(
            AgentBinding::direct(agent("lead", collaboration_tools())),
            Arc::new(ScriptedResolver(scripts.clone())),
            RunId::new(run),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("work"))],
        )
        .with_config(RunConfig::new().with_agent_registry(registry.clone()))
        .with_agent_control(control.root())
    };
    scripts.push("lead", Step::Respond(spawn_call("l-1", "worker", "worker")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", Step::Hang);
    Runner::run(root_run("spawn-run")).await.unwrap();
    eventually(
        || scripts.requests("worker").len() == 1,
        "the worker waits in its model call",
    )
    .await;
    for (run, call) in [("followup-1", "l-3"), ("followup-2", "l-5")] {
        scripts.push("lead", Step::Respond(followup_call(call, "worker", run)));
        scripts.push(
            "lead",
            Step::Respond(final_message(&format!("{call}-done"), "sent")),
        );
        Runner::run(root_run(run)).await.unwrap();
    }
    scripts.push(
        "worker",
        Step::Respond(final_message("w-2", "both tasks done")),
    );
    control.root().interrupt("worker").await.unwrap();
    let child = store.threads()[0].2.clone();
    eventually(|| child.items().iter().any(|item| matches!(item, RolloutItem::RunEnded(end) if end.end() == RolloutRunEnd::Completed)), "the combined follow-ups completed").await;
    let parents: Vec<_> = child
        .items()
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::RunStarted(start) => Some(start.parent_run_id().cloned()),
            _ => None,
        })
        .collect();
    assert_eq!(
        parents,
        vec![Some(RunId::new("spawn-run")), Some(RunId::new("spawn-run"))]
    );
    control.shutdown().await;
}

#[tokio::test]
async fn a_thread_resumed_from_its_rollout_continues_where_it_left_off() {
    use ra_runtime::runner::ContinuationInput;
    use ra_session::{
        ResumeThreadParams, ResumedThread, RolloutFileRecorder, RolloutSessionMeta,
        RolloutThreadDirectory, reconstruct_history,
    };

    let directory = RolloutThreadDirectory::new(temp_dir("rollout_recording_resumed"));
    let session_id = SessionId::new("session-1");
    let path = directory.rollout_path(&session_id).unwrap();
    let scripts = Arc::new(Scripts::default());
    scripts.push("lead", Step::Respond(final_message("m-1", "done")));
    // Item ids are only unique within a run, so the second run may reuse one.
    scripts.push("lead", Step::Respond(final_message("m-1", "again")));
    let lead = agent("lead", Vec::new());
    let start = |run: &str, input: Vec<ModelInputItem>| {
        RunRequest::new(
            AgentBinding::direct(Arc::clone(&lead)),
            Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
            RunId::new(run),
            CancelScope::root(),
            input,
        )
    };

    // The thread's first life ends with its writer shut down, as a process does before it exits.
    let recorder: Arc<dyn RolloutRecorder> =
        Arc::new(RolloutFileRecorder::create_with_session_meta(
            &path,
            RolloutSessionMeta::new(session_id.clone()),
        ));
    let first = run_streamed(
        start(
            "run-1",
            vec![ModelInputItem::Message(Message::user("do it"))],
        )
        .with_rollout_recorder(Arc::clone(&recorder)),
    )
    .await
    .unwrap();
    recorder.shutdown().await.unwrap();

    let resumed = ResumedThread::resume(&directory, &ResumeThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    let last = resumed.reconstruction().last_run().unwrap();
    assert_eq!(last.run_id(), &RunId::new("run-1"));
    assert_eq!(last.end().unwrap().end(), RolloutRunEnd::Completed);
    let history = resumed.reconstruction().history().to_vec();
    assert_eq!(
        history,
        first.continuation_input(ContinuationInput::PreserveAll)
    );

    // The next run starts on the history and records only what is new to the thread.
    let mut input = history.clone();
    input.push(ModelInputItem::Message(Message::user("and again")));
    let second = run_streamed(
        start("run-2", input.clone())
            .with_rollout_recorder(Arc::clone(resumed.recorder()))
            .with_recorded_input(history.len()),
    )
    .await
    .unwrap();
    resumed.recorder().shutdown().await.unwrap();

    assert_eq!(scripts.requests("lead")[1], normalized(&input));
    let records = rollout_records(&path).await;
    assert_eq!(
        records
            .iter()
            .filter(|record| record.type_name() == "session_meta")
            .count(),
        1
    );
    assert!(
        records
            .windows(2)
            .all(|pair| pair[0].timeline_seq() < pair[1].timeline_seq())
    );
    assert_eq!(
        started_inputs(&records),
        vec![
            vec![ModelInputItem::Message(Message::user("do it"))],
            vec![ModelInputItem::Message(Message::user("and again"))],
        ]
    );
    assert_eq!(
        reconstruct_history(&records).unwrap().history(),
        second.continuation_input(ContinuationInput::PreserveAll)
    );
}
