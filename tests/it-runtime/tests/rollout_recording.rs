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
        AgentEvent, HostEventBody, InMemoryHostEventSink,
        file::{FileChangeKind, FileChangedEvent, FileEvent, FileReadEvent},
    },
    finish::FinishReason,
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelRetryAdviceRequest,
        ModelRetrySettings, ModelSelector, ModelSettings, ModelStream, ModelStreamEvent,
        NetworkErrorRetryPolicy, NormalizedProviderError, ProviderKey, ReplaySafety, ResolvedModel,
        RetryAdvice, RetryBackoffSettings,
    },
    session::rollout::{RolloutItem, RolloutRecorder, RolloutRunEnd},
    state::RunId,
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
        ToolServices,
    },
    usage::{RequestUsage, Usage},
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry, control::AgentControl, tool::AgentAsTool},
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
