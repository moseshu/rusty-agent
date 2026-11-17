//! The multi-agent control plane and its collaboration tools, run end to end. Ported from Codex's
//! `MultiAgentV2` behavior: background agents, mailbox delivery at model-call boundaries, final
//! answers reported to the parent, and the wait / follow-up / interrupt loop a parent uses to stop
//! waiting on a stalled child.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{FutureExt, future::BoxFuture};
use ra_core::session::InterruptedTurnHistoryMarker;
use ra_core::{
    agent::{
        AgentId, AgentSpec, HandoffSpec,
        control::{
            AgentControlPort, AgentPath, AgentStatus, InterAgentCommunication, MessageDeliveryMode,
            SpawnAgentForkMode, SpawnAgentRequest, completion_message, fork_history,
        },
    },
    cancel::CancelScope,
    capability::{Capability, CapabilityFamily},
    error::{Error, Result},
    event::{AgentEvent, HostEventBody, InMemoryHostEventSink, SubAgentActivityKind},
    finish::FinishReason,
    hook::{HookDecision, HookEvent, HookEventName, SessionStartSource, UserHook, UserHookContext},
    item::{
        CallId, ContentBlock, ItemId, Message, MessageRole, ModelInputItem, ModelResponse,
        OutputPhase, RunItem, RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
        ToolServices,
    },
    usage::{RequestUsage, Usage},
};
use ra_runtime::{
    agent::{
        AgentBinding, AgentRegistry,
        control::{
            AgentControl, PausedAgentRun, PreparedSpawnedAgent, RolloutBudgetConfig,
            SpawnedAgentFactory, SpawnedAgentRequest, SpawnedAgentResources,
        },
        tool::AgentAsTool,
    },
    hook::{UserHookRegistration, UserHooks},
    runner::{
        ContinuationInput, RunConfig, RunRequest, RunStreamEvent, Runner,
        result::{RunErrorHandler, RunErrorHandlerInput, RunErrorHandlerResult},
    },
};
use ra_tools::agent_ns::{WaitAgentTimeoutOptions, collaboration_tools_with};
use serde_json::{Value, json};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// What one agent's model does on one call.
enum Step {
    Respond(ModelResponse),
    /// Awaits a side effect — a gate, or a release of another agent — before answering.
    After(BoxFuture<'static, ()>, ModelResponse),
    /// Never answers; the call ends only by cancellation.
    Hang,
}

/// Per-agent scripts, keyed by the agent's instructions, so a parent and the children running
/// beside it each read their own.
#[derive(Default)]
struct Scripts {
    steps: Mutex<HashMap<String, VecDeque<Step>>>,
    calls: Mutex<Vec<(String, Vec<ModelInputItem>)>>,
    /// The tool names each call advertised, per agent.
    tools: Mutex<Vec<(String, Vec<String>)>>,
    /// The temperature each call was sent with, per agent.
    temperatures: Mutex<Vec<(String, Option<f64>)>>,
    cache_keys: Mutex<Vec<(String, Option<String>)>>,
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

    fn tools(&self, agent: &str) -> Vec<Vec<String>> {
        self.tools
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key == agent)
            .map(|(_, tools)| tools.clone())
            .collect()
    }

    fn temperatures(&self, agent: &str) -> Vec<Option<f64>> {
        self.temperatures
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key == agent)
            .map(|(_, temperature)| *temperature)
            .collect()
    }

    fn inputs(&self, agent: &str) -> Vec<Vec<ModelInputItem>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key == agent)
            .map(|(_, input)| input.clone())
            .collect()
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
            .cache_keys
            .lock()
            .unwrap()
            .push((agent.clone(), request.prompt_cache_key().map(str::to_owned)));
        self.0
            .calls
            .lock()
            .unwrap()
            .push((agent.clone(), request.input().to_vec()));
        self.0.tools.lock().unwrap().push((
            agent.clone(),
            request
                .tools()
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect(),
        ));
        self.0
            .temperatures
            .lock()
            .unwrap()
            .push((agent.clone(), request.model_settings().temperature()));
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

/// A tool that blocks until released, and records that it started and whether it was dropped.
struct GateTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    started: Arc<Notify>,
    release: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

impl GateTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("slow_task").unwrap(),
            schema: ToolSchema::new(
                "slow_task",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct SetOnDrop(Arc<AtomicBool>, bool);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        if !self.1 {
            self.0.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl Tool for GateTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::default()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        let mut guard = SetOnDrop(Arc::clone(&self.dropped), false);
        self.started.notify_one();
        self.release.notified().await;
        guard.1 = true;
        Ok(ToolOutput::text("slow task done"))
    }
}

/// A tool that answers at once.
struct QuickTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl QuickTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("quick_look").unwrap(),
            schema: ToolSchema::new(
                "quick_look",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
        }
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
        ToolOptions::default()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("12 modules"))
    }
}

/// Short waits so tests do not sit through Codex's 10-second floor.
fn wait_options() -> WaitAgentTimeoutOptions {
    WaitAgentTimeoutOptions::new().with_bounds(2_000, 0, 60_000)
}

fn collaboration() -> Vec<Arc<dyn Tool>> {
    collaboration_tools_with(wait_options())
        .unwrap()
        .into_iter()
        .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
        .collect()
}

fn parent_agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(collaboration())
        .build()
        .unwrap()
}

fn worker_agent(extra: Vec<Arc<dyn Tool>>) -> Arc<AgentSpec> {
    let mut tools = collaboration();
    tools.extend(extra);
    AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .instructions("worker")
        .tools(tools)
        .build()
        .unwrap()
}

fn root_request(
    scripts: &Arc<Scripts>,
    control: &AgentControl,
    worker: Arc<AgentSpec>,
) -> RunRequest {
    let registry = AgentRegistry::builder().register(worker).build().unwrap();
    RunRequest::new(
        AgentBinding::direct(parent_agent()),
        Arc::new(ScriptedResolver(Arc::clone(scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("do the work"))],
    )
    .with_config(
        RunConfig::new()
            .with_max_turns(32)
            .with_agent_registry(registry),
    )
    .with_agent_control(control.root())
}

fn texts(input: &[ModelInputItem]) -> Vec<String> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) => Some(message.text_content()),
            ModelInputItem::ToolCallOutput(output) => Some(
                ToolOutput::from_stored(output.output())
                    .ok()
                    .flatten()
                    .and_then(|output| output.as_text().map(str::to_owned))
                    .unwrap_or_else(|| output.output().to_string()),
            ),
            _ => None,
        })
        .collect()
}

fn spawn_worker(id: &str, message: &str) -> ModelResponse {
    tool_call(
        id,
        &format!("{id}-call"),
        "spawn_agent",
        json!({"task_name": "worker", "message": message, "agent_type": "worker"}),
    )
}

const NEW_TASK: &str = "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\n";
const FINAL_ANSWER: &str =
    "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\n";

fn worker_path() -> AgentPath {
    AgentPath::root().join("worker").unwrap()
}

async fn eventually_status(
    control: &AgentControl,
    path: &AgentPath,
    accept: impl FnMut(&AgentStatus) -> bool,
) -> AgentStatus {
    tokio::time::timeout(
        Duration::from_secs(10),
        control.wait_for_status(path, accept),
    )
    .await
    .expect("status reached in time")
    .expect("agent exists")
}

// ---------------------------------------------------------------------------------------------
// Spawn, report and wait
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn spawned_agent_runs_in_the_background_and_reports_its_final_answer() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let release_worker = Arc::new(Notify::new());

    scripts.push(
        "lead",
        Step::Respond(spawn_worker("l-1", "count the files")),
    );
    // The parent is free while the worker runs; it releases the worker only once it has moved on
    // to waiting, so the final answer arrives during the wait rather than before it.
    let release = Arc::clone(&release_worker);
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move { release.notify_one() }),
            tool_call(
                "l-2",
                "l-2-call",
                "wait_agent",
                json!({"timeout_ms": 10_000}),
            ),
        ),
    );
    scripts.push("lead", Step::Respond(final_message("l-3", "all done")));
    let gate = Arc::clone(&release_worker);
    scripts.push(
        "worker",
        Step::After(
            Box::pin(async move { gate.notified().await }),
            final_message("w-1", "42 files"),
        ),
    );

    let result = Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    assert_eq!(result.final_text(), "all done");

    let lead = scripts.inputs("lead");
    assert_eq!(lead.len(), 3);
    let after_spawn = texts(&lead[1]);
    assert_eq!(
        after_spawn.last().unwrap(),
        r#"{"task_name":"/root/worker"}"#
    );
    let after_wait = texts(&lead[2]);
    // The wait reports activity without the content; the content arrives as mail at the next
    // model call, after the wait's own output.
    let wait_output = &after_wait[after_wait.len() - 2];
    assert_eq!(
        serde_json::from_str::<Value>(wait_output).unwrap(),
        json!({"message": "Wait completed.", "timed_out": false})
    );
    assert_eq!(
        after_wait.last().unwrap(),
        &format!("{FINAL_ANSWER}42 files")
    );

    // By default the worker forks the conversation so far, then reads its task.
    let worker = scripts.inputs("worker");
    assert_eq!(
        texts(&worker[0]),
        vec![
            "do the work".to_owned(),
            format!("{NEW_TASK}count the files"),
        ]
    );
    assert_eq!(
        control.status(&worker_path()),
        Some(AgentStatus::Completed(Some("42 files".to_owned())))
    );
    assert_eq!(
        control.status(&AgentPath::root()),
        Some(AgentStatus::Completed(Some("all done".to_owned())))
    );
}

#[tokio::test]
async fn a_stalled_child_is_redirected_after_the_wait_times_out() {
    // The scenario this whole surface exists for: the parent stops waiting on a child stuck in a
    // long tool call, tells it to wrap up, and the child — still running — reads that at its next
    // model call and reports what it has.
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let slow = GateTool::new();
    let release_tool = Arc::clone(&slow.release);
    let tool_started = Arc::clone(&slow.started);

    scripts.push(
        "lead",
        Step::Respond(spawn_worker("l-1", "audit every crate")),
    );
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move { tool_started.notified().await }),
            tool_call("l-2", "l-2-call", "wait_agent", json!({"timeout_ms": 50})),
        ),
    );
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-3",
            "l-3-call",
            "followup_task",
            json!({"target": "worker", "message": "Stop now and report what you have."}),
        )),
    );
    // Only once the follow-up is queued is the stuck call let go, so the child's next model call
    // is the one that reads it.
    let release = Arc::clone(&release_tool);
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move { release.notify_one() }),
            tool_call(
                "l-4",
                "l-4-call",
                "wait_agent",
                json!({"timeout_ms": 10_000}),
            ),
        ),
    );
    scripts.push(
        "lead",
        Step::Respond(final_message("l-5", "partial audit accepted")),
    );

    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "slow_task", json!({}))),
    );
    scripts.push(
        "worker",
        Step::Respond(final_message("w-2", "audited 3 of 9 crates")),
    );

    let result = Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(vec![Arc::new(slow)]),
    ))
    .await
    .unwrap();
    assert_eq!(result.final_text(), "partial audit accepted");

    let lead = scripts.inputs("lead");
    let timed_out = texts(&lead[2]);
    assert_eq!(
        serde_json::from_str::<Value>(timed_out.last().unwrap()).unwrap(),
        json!({"message": "Wait timed out.", "timed_out": true})
    );
    let final_turn = texts(lead.last().unwrap());
    assert_eq!(
        final_turn.last().unwrap(),
        &format!("{FINAL_ANSWER}audited 3 of 9 crates")
    );

    // One run: the follow-up reached the child mid-run, after the tool output it was waiting on.
    let worker = scripts.inputs("worker");
    assert_eq!(worker.len(), 2);
    let second = texts(&worker[1]);
    assert_eq!(second[second.len() - 2], "slow task done");
    assert_eq!(
        second.last().unwrap(),
        &format!("{NEW_TASK}Stop now and report what you have.")
    );
}

#[tokio::test]
async fn an_interrupted_child_keeps_its_settled_turns_and_resumes_on_a_follow_up() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let slow = GateTool::new();
    let tool_started = Arc::clone(&slow.started);
    let tool_dropped = Arc::clone(&slow.dropped);
    let release_answer = Arc::new(Notify::new());

    scripts.push("lead", Step::Respond(spawn_worker("l-1", "index the repo")));
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move { tool_started.notified().await }),
            tool_call(
                "l-2",
                "l-2-call",
                "interrupt_agent",
                json!({"target": "worker"}),
            ),
        ),
    );
    let watch = control.clone();
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move {
                drop(
                    watch
                        .wait_for_status(&worker_path(), |status| {
                            *status == AgentStatus::Interrupted
                        })
                        .await,
                );
            }),
            tool_call(
                "l-3",
                "l-3-call",
                "followup_task",
                json!({"target": "/root/worker", "message": "Summarize what you saw."}),
            ),
        ),
    );
    let release = Arc::clone(&release_answer);
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move { release.notify_one() }),
            tool_call("l-4", "l-4-call", "wait_agent", json!({})),
        ),
    );
    scripts.push("lead", Step::Respond(final_message("l-5", "ok")));

    // The first run settles one turn, then gets stuck in a tool on the next.
    scripts.push(
        "worker",
        Step::Respond(ModelResponse::new(vec![
            RunItem::new(
                ItemId::new("w-1a"),
                RunItemKind::Message(Message::assistant(
                    "Looking around first.",
                    OutputPhase::Commentary,
                )),
            ),
            RunItem::new(
                ItemId::new("w-1b"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("w-1-call"),
                    "quick_look",
                    json!({}),
                )),
            ),
        ])),
    );
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-2", "w-2-call", "slow_task", json!({}))),
    );
    let gate = Arc::clone(&release_answer);
    scripts.push(
        "worker",
        Step::After(
            Box::pin(async move { gate.notified().await }),
            final_message("w-3", "12 modules indexed"),
        ),
    );

    let result = Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(vec![Arc::new(slow), Arc::new(QuickTool::new())]),
    ))
    .await
    .unwrap();
    assert_eq!(result.final_text(), "ok");
    assert!(
        tool_dropped.load(Ordering::SeqCst),
        "the interrupt reached the tool call"
    );

    let lead = scripts.inputs("lead");
    assert_eq!(
        serde_json::from_str::<Value>(texts(&lead[2]).last().unwrap()).unwrap(),
        json!({"previous_status": "running"})
    );
    assert_eq!(
        texts(lead.last().unwrap()).last().unwrap(),
        &format!("{FINAL_ANSWER}12 modules indexed")
    );

    // The second run carries the first one's settled turn, the marker Codex records where a turn
    // was interrupted — in an agent tree, its developer form — and the follow-up. The turn that
    // was in flight at the interrupt was never recorded, so its call does not reach the next
    // request.
    let marker = InterruptedTurnHistoryMarker::Developer.message().unwrap();
    let worker = scripts.inputs("worker");
    assert_eq!(worker.len(), 3);
    assert_eq!(
        texts(&worker[2]),
        vec![
            "do the work".to_owned(),
            format!("{NEW_TASK}index the repo"),
            "Looking around first.".to_owned(),
            "12 modules".to_owned(),
            marker.text_content(),
            format!("{NEW_TASK}Summarize what you saw."),
        ]
    );
    assert!(worker[2].contains(&ModelInputItem::Message(marker)));
    let calls: Vec<&str> = worker[2]
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::ToolCall(call) => Some(call.name()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, vec!["quick_look"]);
}

// ---------------------------------------------------------------------------------------------
// Mailbox delivery in the runner
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn mail_waits_for_the_second_model_call_when_the_run_brings_new_input() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let root = control.root();
    root.send(
        "/root",
        "note from the host".to_owned(),
        MessageDeliveryMode::QueueOnly,
    )
    .await
    .unwrap();

    scripts.push(
        "lead",
        Step::Respond(tool_call("l-1", "l-1-call", "list_agents", json!({}))),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "done")));
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();

    let lead = scripts.inputs("lead");
    assert_eq!(texts(&lead[0]), vec!["do the work".to_owned()]);
    let second = texts(&lead[1]);
    assert_eq!(
        serde_json::from_str::<Value>(&second[1]).unwrap(),
        json!({"agents": [{"agent_name": "/root", "agent_status": "running"}]})
    );
    assert_eq!(
        second[2],
        "Message Type: MESSAGE\nTask name: /root\nSender: /root\nPayload:\nnote from the host"
    );
    assert!(!root.has_pending_mail());
}

#[tokio::test]
async fn a_run_that_concludes_at_once_leaves_mail_for_the_next_run() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let root = control.root();
    root.send("/root", "later".to_owned(), MessageDeliveryMode::QueueOnly)
        .await
        .unwrap();

    scripts.push("lead", Step::Respond(final_message("l-1", "done")));
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    assert_eq!(
        texts(&scripts.inputs("lead")[0]),
        vec!["do the work".to_owned()]
    );
    assert!(root.has_pending_mail());
}

// ---------------------------------------------------------------------------------------------
// Refusals and limits
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn tools_answer_that_collaboration_is_off_without_a_control_plane() {
    let scripts = Arc::new(Scripts::default());
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "anything")));
    scripts.push("lead", Step::Respond(final_message("l-2", "fine")));
    let request = RunRequest::new(
        AgentBinding::direct(parent_agent()),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-plain"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("go"))],
    );
    Runner::run(request).await.unwrap();
    assert_eq!(
        texts(&scripts.inputs("lead")[1]).last().unwrap(),
        "multi-agent collaboration is not enabled for this run"
    );
}

#[tokio::test]
async fn spawn_and_message_refusals_use_codex_wording() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let release = Arc::new(Notify::new());

    scripts.push("lead", Step::Respond(spawn_worker("l-1", "first")));
    for (index, (name, arguments)) in [
        (
            "spawn_agent",
            json!({"task_name": "worker", "message": "again", "agent_type": "worker"}),
        ),
        (
            "spawn_agent",
            json!({"task_name": "Bad Name", "message": "x"}),
        ),
        (
            "spawn_agent",
            json!({"task_name": "other", "message": "  "}),
        ),
        (
            "spawn_agent",
            json!({"task_name": "other", "message": "x", "agent_type": "nobody"}),
        ),
        (
            "spawn_agent",
            json!({"task_name": "other", "message": "x", "fork_turns": "0"}),
        ),
        (
            "spawn_agent",
            json!({"task_name": "other", "message": "x", "fork_turns": "many"}),
        ),
        ("followup_task", json!({"target": "/root", "message": "x"})),
        ("send_message", json!({"target": "ghost", "message": "x"})),
        ("interrupt_agent", json!({"target": "/root"})),
        ("wait_agent", json!({"timeout_ms": 60_001})),
        ("wait_agent", json!({"timeout_ms": -5})),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("l-r{index}");
        scripts.push(
            "lead",
            Step::Respond(tool_call(&id, &format!("{id}-call"), name, arguments)),
        );
    }
    let gate = Arc::clone(&release);
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move { gate.notify_one() }),
            final_message("l-end", "done"),
        ),
    );
    let worker_gate = Arc::clone(&release);
    scripts.push(
        "worker",
        Step::After(
            Box::pin(async move { worker_gate.notified().await }),
            final_message("w-1", "ok"),
        ),
    );

    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();

    let last = scripts.inputs("lead").last().unwrap().clone();
    let outputs: Vec<String> = texts(&last).into_iter().skip(2).collect();
    assert_eq!(
        outputs,
        vec![
            "agent path `/root/worker` already exists".to_owned(),
            "agent_name must use only lowercase letters, digits, and underscores".to_owned(),
            "Empty message can't be sent to an agent".to_owned(),
            "collab spawn failed: unknown agent_type `nobody`".to_owned(),
            "fork_turns must be `none` or `all`".to_owned(),
            "fork_turns must be `none` or `all`".to_owned(),
            "Follow-up tasks can't target the root agent".to_owned(),
            "live agent path `/root/ghost` not found".to_owned(),
            "root is not a spawned agent".to_owned(),
            "timeout_ms must be at most 60000".to_owned(),
            r#"{"message":"Wait timed out.\n\nRequested timeout of -5ms was clamped to the minimum of 0ms.","timed_out":true}"#.to_owned(),
        ]
    );
}

#[tokio::test]
async fn short_waits_are_raised_to_the_floor_and_say_so() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let tools: Vec<Arc<dyn Tool>> =
        collaboration_tools_with(WaitAgentTimeoutOptions::new().with_bounds(1_000, 20, 60_000))
            .unwrap()
            .into_iter()
            .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
            .collect();
    let lead = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(tools)
        .build()
        .unwrap();
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-1",
            "l-1-call",
            "wait_agent",
            json!({"timeout_ms": 5}),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "done")));
    let request = RunRequest::new(
        AgentBinding::direct(lead),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("wait"))],
    )
    .with_agent_control(control.root());
    Runner::run(request).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(texts(&scripts.inputs("lead")[1]).last().unwrap()).unwrap(),
        json!({
            "message": "Wait timed out.\n\nRequested timeout of 5ms was clamped to the minimum of 20ms.",
            "timed_out": true,
        })
    );
}

#[tokio::test]
async fn spawning_past_the_concurrency_ceiling_is_refused() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::with_max_concurrent_threads(1);
    let release = Arc::new(Notify::new());

    scripts.push("lead", Step::Respond(spawn_worker("l-1", "first")));
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-2",
            "l-2-call",
            "spawn_agent",
            json!({"task_name": "second", "message": "more", "agent_type": "worker"}),
        )),
    );
    let gate = Arc::clone(&release);
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move { gate.notify_one() }),
            final_message("l-3", "done"),
        ),
    );
    let worker_gate = Arc::clone(&release);
    scripts.push(
        "worker",
        Step::After(
            Box::pin(async move { worker_gate.notified().await }),
            final_message("w-1", "ok"),
        ),
    );

    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    assert_eq!(
        texts(&scripts.inputs("lead")[2]).last().unwrap(),
        "collab spawn failed: agent thread limit reached"
    );
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
}

#[tokio::test]
async fn a_queue_only_message_does_not_wake_an_idle_agent_but_a_follow_up_does() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "first")));
    scripts.push("lead", Step::Respond(final_message("l-2", "done")));
    scripts.push("worker", Step::Respond(final_message("w-1", "first done")));
    scripts.push("worker", Step::Respond(final_message("w-2", "second done")));
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    let root = control.root();
    root.send("worker", "fyi".to_owned(), MessageDeliveryMode::QueueOnly)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        scripts.inputs("worker").len(),
        1,
        "queue-only mail starts nothing"
    );

    root.send(
        "worker",
        "now this".to_owned(),
        MessageDeliveryMode::TriggerTurn,
    )
    .await
    .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("second done".to_owned()))
    })
    .await;
    let second = texts(&scripts.inputs("worker")[1]);
    assert_eq!(
        second,
        vec![
            "do the work".to_owned(),
            format!("{NEW_TASK}first"),
            "first done".to_owned(),
            "Message Type: MESSAGE\nTask name: /root/worker\nSender: /root\nPayload:\nfyi"
                .to_owned(),
            format!("{NEW_TASK}now this"),
        ]
    );
    // Both final answers were reported to the root.
    assert!(root.has_pending_mail());
}

#[tokio::test]
async fn a_follow_up_that_misses_the_last_model_call_starts_the_next_run() {
    // Mail that arrives while the concluding model call is in progress is not part of that run;
    // the agent must pick it up as soon as the run ends rather than going idle on it.
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let in_call = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    scripts.push("lead", Step::Respond(spawn_worker("l-1", "first")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    let (signal, gate) = (Arc::clone(&in_call), Arc::clone(&release));
    scripts.push(
        "worker",
        Step::After(
            Box::pin(async move {
                signal.notify_one();
                gate.notified().await;
            }),
            final_message("w-1", "first done"),
        ),
    );
    scripts.push("worker", Step::Respond(final_message("w-2", "second done")));
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();

    in_call.notified().await;
    control
        .root()
        .send(
            "worker",
            "one more".to_owned(),
            MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap();
    release.notify_one();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("second done".to_owned()))
    })
    .await;
    assert_eq!(
        texts(&scripts.inputs("worker")[1]),
        vec![
            "do the work".to_owned(),
            format!("{NEW_TASK}first"),
            "first done".to_owned(),
            format!("{NEW_TASK}one more"),
        ]
    );
}

#[tokio::test]
async fn shutdown_stops_running_children() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "forever")));
    scripts.push(
        "lead",
        Step::Respond(final_message("l-2", "leaving it running")),
    );
    scripts.push("worker", Step::Hang);
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Running
    })
    .await;

    tokio::time::timeout(Duration::from_secs(10), control.shutdown())
        .await
        .expect("shutdown drains in time");
    assert_eq!(control.status(&worker_path()), Some(AgentStatus::Shutdown));
    let error = control
        .root()
        .send(
            "worker",
            "hello".to_owned(),
            MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "agent `/root/worker` is closed");
}

// ---------------------------------------------------------------------------------------------
// Spawned-agent factory
// ---------------------------------------------------------------------------------------------

/// Prepares every spawned agent as `prepared`, recording what each spawn asked for, or refuses.
struct PreparingFactory {
    prepared: Arc<AgentSpec>,
    requests: Mutex<Vec<(String, Option<String>, String)>>,
    released: Arc<AtomicUsize>,
    refuse: bool,
}

impl PreparingFactory {
    fn new(prepared: Arc<AgentSpec>, refuse: bool) -> Self {
        Self {
            prepared,
            requests: Mutex::new(Vec::new()),
            released: Arc::new(AtomicUsize::new(0)),
            refuse,
        }
    }
}

struct CountedRelease(Arc<AtomicUsize>);

#[async_trait]
impl SpawnedAgentResources for CountedRelease {
    async fn shutdown(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl SpawnedAgentFactory for PreparingFactory {
    async fn prepare(&self, request: SpawnedAgentRequest<'_>) -> Result<PreparedSpawnedAgent> {
        if self.refuse {
            return Err(Error::config("no host resources left"));
        }
        self.requests.lock().unwrap().push((
            request.path().to_string(),
            request.agent_type().map(ToString::to_string),
            request.agent().id().to_string(),
        ));
        Ok(PreparedSpawnedAgent::new(Arc::clone(&self.prepared))
            .with_resources(Arc::new(CountedRelease(Arc::clone(&self.released)))))
    }
}

fn prepared_worker() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("prepared-worker"))
        .name("Prepared worker")
        .instructions("prepared")
        .build()
        .unwrap()
}

/// Codex builds each spawned thread as a session of its own; the factory is where a host does the
/// same, and what it gives the agent is released once, after the agent's run has ended.
#[tokio::test]
async fn a_spawned_agent_factory_prepares_the_agent_and_releases_its_resources_once() {
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), false));
    let control = AgentControl::new()
        .with_spawned_agent_factory(Arc::clone(&factory) as Arc<dyn SpawnedAgentFactory>)
        .unwrap();
    assert!(
        AgentControl::new()
            .with_spawned_agent_factory(Arc::clone(&factory) as Arc<dyn SpawnedAgentFactory>)
            .unwrap()
            .with_spawned_agent_factory(factory.clone())
            .is_err(),
        "a tree takes one factory"
    );
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "work")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "prepared",
        Step::Respond(final_message("p-1", "prepared done")),
    );
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    // The spawn resolved the registered worker; the agent that ran is the one the factory made.
    assert_eq!(
        *factory.requests.lock().unwrap(),
        [(
            "/root/worker".to_owned(),
            Some("worker".to_owned()),
            "worker".to_owned()
        )]
    );
    assert!(scripts.tools("worker").is_empty());
    assert_eq!(scripts.tools("prepared").len(), 1);
    assert_eq!(factory.released.load(Ordering::SeqCst), 0);

    control.close(&worker_path()).await.unwrap();
    assert_eq!(factory.released.load(Ordering::SeqCst), 1);
    control.shutdown().await;
    assert_eq!(factory.released.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutting_the_tree_down_releases_each_prepared_agent() {
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), false));
    let control = AgentControl::new()
        .with_spawned_agent_factory(Arc::clone(&factory) as Arc<dyn SpawnedAgentFactory>)
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "forever")));
    scripts.push("lead", Step::Respond(final_message("l-2", "left running")));
    scripts.push("prepared", Step::Hang);
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Running
    })
    .await;

    tokio::time::timeout(Duration::from_secs(10), control.shutdown())
        .await
        .expect("shutdown drains in time");
    assert_eq!(factory.released.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_factory_that_cannot_prepare_the_agent_refuses_the_spawn() {
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), true));
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory as Arc<dyn SpawnedAgentFactory>)
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "work")));
    scripts.push("lead", Step::Respond(final_message("l-2", "refused")));
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();

    let refusal = texts(&scripts.inputs("lead")[1]).last().unwrap().clone();
    assert!(
        refusal.starts_with("agent `/root/worker` could not be prepared: ")
            && refusal.ends_with("no host resources left"),
        "{refusal}"
    );
    assert_eq!(paths(&control), ["/root"]);
}

// ---------------------------------------------------------------------------------------------
// Forked history, handoffs and approvals
// ---------------------------------------------------------------------------------------------

fn spawn_with_fork(id: &str, task_name: &str, fork_turns: Option<&str>) -> RunItem {
    let mut arguments = json!({"task_name": task_name, "message": "go", "agent_type": "worker"});
    if let Some(fork_turns) = fork_turns {
        arguments["fork_turns"] = json!(fork_turns);
    }
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(format!("{id}-call")),
            "spawn_agent",
            arguments,
        )),
    )
}

#[tokio::test]
async fn fork_turns_legacy_counts_inherit_full_history_like_all() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let mut tools = collaboration();
    tools.push(Arc::new(QuickTool::new()));
    let lead = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(tools)
        .build()
        .unwrap();

    // A turn of working state the children must not inherit, then three spawns in one response.
    scripts.push(
        "lead",
        Step::Respond(ModelResponse::new(vec![
            RunItem::new(
                ItemId::new("l-1a"),
                RunItemKind::Message(Message::assistant(
                    "Checking first.",
                    OutputPhase::Commentary,
                )),
            ),
            RunItem::new(
                ItemId::new("l-1b"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("l-1-call"),
                    "quick_look",
                    json!({}),
                )),
            ),
        ])),
    );
    scripts.push(
        "lead",
        Step::Respond(ModelResponse::new(vec![
            spawn_with_fork("l-2a", "none_fork", Some("none")),
            spawn_with_fork("l-2b", "legacy_count", Some("1")),
            spawn_with_fork("l-2c", "everything", None),
        ])),
    );
    scripts.push("lead", Step::Respond(final_message("l-3", "spawned")));
    for index in 0..3 {
        scripts.push(
            "worker",
            Step::Respond(final_message(&format!("w-{index}"), "ok")),
        );
    }

    let registry = AgentRegistry::builder()
        .register(worker_agent(Vec::new()))
        .build()
        .unwrap();
    let request = RunRequest::new(
        AgentBinding::direct(lead),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![
            ModelInputItem::Message(Message::user("first ask")),
            ModelInputItem::Message(Message::assistant("first answer", OutputPhase::Final)),
            ModelInputItem::Message(Message::user("second ask")),
        ],
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root());
    Runner::run(request).await.unwrap();
    for name in ["none_fork", "legacy_count", "everything"] {
        eventually_status(&control, &AgentPath::root().join(name).unwrap(), |status| {
            matches!(status, AgentStatus::Completed(_))
        })
        .await;
    }

    let inputs = scripts.inputs("worker");
    let child = |name: &str| {
        inputs
            .iter()
            .map(|input| texts(input))
            .find(|texts| {
                texts
                    .last()
                    .is_some_and(|task| task.contains(&format!("Task name: /root/{name}\n")))
            })
            .unwrap()
    };
    let task = |name: &str| {
        format!("Message Type: NEW_TASK\nTask name: /root/{name}\nSender: /root\nPayload:\ngo")
    };
    assert_eq!(child("none_fork"), vec![task("none_fork")]);
    assert_eq!(
        child("legacy_count"),
        vec![
            "first ask".to_owned(),
            "first answer".to_owned(),
            "second ask".to_owned(),
            task("legacy_count")
        ]
    );
    assert_eq!(
        child("everything"),
        vec![
            "first ask".to_owned(),
            "first answer".to_owned(),
            "second ask".to_owned(),
            task("everything"),
        ]
    );
}

#[test]
fn a_fork_keeps_the_conversation_and_counts_turns_from_user_input_and_new_tasks() {
    let envelope = |kind: &str, payload: &str| {
        ModelInputItem::Message(Message::user(format!(
            "Message Type: {kind}\nTask name: /root/worker\nSender: /root\nPayload:\n{payload}"
        )))
    };
    let history = vec![
        ModelInputItem::Message(Message::system("preamble")),
        envelope("NEW_TASK", "first task"),
        ModelInputItem::Message(Message::assistant("thinking", OutputPhase::Commentary)),
        ModelInputItem::ToolCall(ToolCall::new(CallId::new("c-1"), "quick_look", json!({}))),
        ModelInputItem::Message(Message::assistant("first result", OutputPhase::Final)),
        envelope("MESSAGE", "fyi"),
        envelope("FINAL_ANSWER", "child result"),
        ModelInputItem::Message(Message::user("a real user turn")),
        envelope("NEW_TASK", "second task"),
        ModelInputItem::Message(Message::assistant("second result", OutputPhase::Final)),
    ];
    let kept = |items: Vec<ModelInputItem>| texts(&items);

    // Inter-agent mail, tool traffic and commentary stay behind.
    assert_eq!(
        kept(fork_history(&history, SpawnAgentForkMode::FullHistory)),
        vec![
            "preamble",
            "first result",
            "a real user turn",
            "second result"
        ]
    );
    // A task from another agent starts a turn; a plain message or a child's report does not.
    assert_eq!(
        kept(fork_history(&history, SpawnAgentForkMode::LastNTurns(1))),
        vec!["second result"]
    );
    assert_eq!(
        kept(fork_history(&history, SpawnAgentForkMode::LastNTurns(2))),
        vec!["a real user turn", "second result"]
    );
    // The plain message and the child's report between the first task and the user turn are not
    // turns, so three turns reach back to the first task.
    assert_eq!(
        kept(fork_history(&history, SpawnAgentForkMode::LastNTurns(3))),
        vec!["first result", "a real user turn", "second result"]
    );
    // Asking for more turns than there are starts at the first turn, without the preamble.
    assert_eq!(
        kept(fork_history(&history, SpawnAgentForkMode::LastNTurns(9))),
        vec!["first result", "a real user turn", "second result"]
    );
    assert!(fork_history(&history, SpawnAgentForkMode::LastNTurns(0)).is_empty());
}

#[tokio::test]
async fn a_child_spawned_after_a_handoff_runs_the_agent_handed_to() {
    spawn_after_handoff(None).await;
}

async fn spawn_after_handoff(surface: Option<Vec<Arc<dyn Tool>>>) -> Arc<Scripts> {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let mut tools = collaboration();
    tools.extend(surface.iter().flatten().cloned());
    let specialist = AgentSpec::builder()
        .id(AgentId::new("specialist"))
        .name("Specialist")
        .instructions("specialist")
        .tools(tools.clone())
        .build()
        .unwrap();
    let lead = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(tools)
        .handoff(HandoffSpec::new(
            AgentId::new("specialist"),
            ToolSchema::new(
                "transfer_to_specialist",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
        ))
        .build()
        .unwrap();

    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-1",
            "l-1-call",
            "transfer_to_specialist",
            json!({}),
        )),
    );
    scripts.push(
        "specialist",
        Step::Respond(tool_call(
            "s-1",
            "s-1-call",
            "spawn_agent",
            json!({"task_name": "helper", "message": "help", "fork_turns": "none"}),
        )),
    );
    // The specialist's next turn and the helper's first both read this agent's script, in whatever
    // order they are scheduled; both conclude.
    scripts.push("specialist", Step::Respond(final_message("s-2", "done")));
    scripts.push("specialist", Step::Respond(final_message("h-1", "done")));

    let registry = AgentRegistry::builder()
        .register(Arc::clone(&lead))
        .register(specialist)
        .build()
        .unwrap();
    let mut request = RunRequest::new(
        AgentBinding::direct(lead),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("route this"))],
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root());
    if let Some(surface) = surface {
        request = request.with_prepared_capabilities(
            [Arc::new(LeadSurface(surface.clone())) as Arc<dyn Capability>],
            surface,
        );
    }
    assert_eq!(Runner::run(request).await.unwrap().final_text(), "done");

    let helper = AgentPath::root().join("helper").unwrap();
    eventually_status(&control, &helper, |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    // The helper ran the specialist's declaration — its model was asked with the specialist's
    // instructions — not the lead's, which the run started with.
    assert_eq!(scripts.inputs("lead").len(), 1);
    let helper_calls = scripts
        .inputs("specialist")
        .iter()
        .filter(|input| {
            texts(input)
                .last()
                .is_some_and(|task| task.contains("Task name: /root/helper\n"))
        })
        .count();
    assert_eq!(helper_calls, 1);
    assert_eq!(
        control.agents()[1].agent_id(),
        Some(&AgentId::new("specialist"))
    );
    scripts
}

// ---------------------------------------------------------------------------------------------
// Prepared capability surfaces across spawns
// ---------------------------------------------------------------------------------------------

/// A surface a host prepared for the lead: its tools are already on the lead's declaration, and it
/// contributes a sampling setting at run assembly.
struct LeadSurface(Vec<Arc<dyn Tool>>);

#[async_trait]
impl Capability for LeadSurface {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("lead_surface").unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.0.clone()
    }

    fn sampling_params(&self, settings: ModelSettings) -> ModelSettings {
        settings.with_temperature(0.31)
    }
}

/// The lead with `extra` installed beside its collaboration tools.
fn lead_with(extra: &[Arc<dyn Tool>]) -> Arc<AgentSpec> {
    let mut tools = collaboration();
    tools.extend(extra.iter().cloned());
    AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(tools)
        .build()
        .unwrap()
}

/// Runs the lead with `surface` prepared on it, spawning a worker by `agent_type`.
async fn spawn_worker_beside(surface: Vec<Arc<dyn Tool>>) -> Arc<Scripts> {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "check")));
    scripts.push("lead", Step::Respond(final_message("l-2", "done")));
    scripts.push("worker", Step::Respond(final_message("w-1", "checked")));

    let registry = AgentRegistry::builder()
        .register(worker_agent(Vec::new()))
        .build()
        .unwrap();
    let request = RunRequest::new(
        AgentBinding::direct(lead_with(&surface)),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("do the work"))],
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root())
    .with_prepared_capabilities(
        [Arc::new(LeadSurface(surface.clone())) as Arc<dyn Capability>],
        surface,
    );
    assert_eq!(Runner::run(request).await.unwrap().final_text(), "done");
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    scripts
}

/// Codex plans a sub-agent's tools for that sub-agent; a registered agent named by `agent_type`
/// does not hold the lead's prepared tool objects, so it runs without the lead's surface instead
/// of failing to bind it.
#[tokio::test]
async fn an_agent_spawned_by_type_does_not_inherit_the_parents_prepared_tools() {
    let quick: Arc<dyn Tool> = Arc::new(QuickTool::new());
    let scripts = spawn_worker_beside(vec![quick]).await;

    for tools in scripts.tools("lead") {
        assert!(tools.contains(&"quick_look".to_owned()));
    }
    assert_eq!(scripts.temperatures("lead"), vec![Some(0.31), Some(0.31)]);
    let worker = scripts.tools("worker");
    assert_eq!(worker.len(), 1);
    assert!(!worker[0].contains(&"quick_look".to_owned()));
    assert_eq!(scripts.temperatures("worker"), vec![None]);
}

/// A surface without tools has no tool identity to check, and the declaration decides: another
/// agent does not receive the lead's runtime contributions.
#[tokio::test]
async fn an_agent_spawned_by_type_does_not_inherit_a_tool_less_prepared_surface() {
    let scripts = spawn_worker_beside(Vec::new()).await;

    assert_eq!(scripts.temperatures("lead"), vec![Some(0.31), Some(0.31)]);
    assert_eq!(scripts.temperatures("worker"), vec![None]);
}

/// A handoff changes the current declaration without transferring ownership of the run's
/// prepared surface. Even shared tool objects do not make that surface the specialist's own.
#[tokio::test]
async fn a_child_spawned_after_a_handoff_does_not_inherit_the_initial_prepared_surface() {
    let quick: Arc<dyn Tool> = Arc::new(QuickTool::new());
    for surface in [Vec::new(), vec![quick]] {
        let scripts = spawn_after_handoff(Some(surface)).await;

        assert_eq!(scripts.temperatures("lead"), vec![Some(0.31)]);
        assert_eq!(scripts.temperatures("specialist"), vec![None; 3]);
    }
}

/// Resuming the receiving agent must preserve the original surface owner for later spawns.
#[tokio::test]
async fn a_child_spawned_after_handoff_approval_resume_does_not_inherit_the_initial_surface() {
    let quick: Arc<dyn Tool> = Arc::new(QuickTool::new());
    for surface in [Vec::new(), vec![quick]] {
        let scripts = Arc::new(Scripts::default());
        let control = AgentControl::new();
        let mut tools = collaboration();
        tools.extend(surface.iter().cloned());
        let lead = lead_with(&surface)
            .to_builder()
            .handoff(HandoffSpec::new(
                AgentId::new("specialist"),
                ToolSchema::new(
                    "transfer_to_specialist",
                    json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
                ).unwrap(),
            ))
            .build().unwrap();
        let specialist = AgentSpec::builder()
            .id(AgentId::new("specialist"))
            .name("Specialist")
            .instructions("specialist")
            .tools(tools)
            .tool(Arc::new(GuardedTool::new()))
            .build()
            .unwrap();
        let config = RunConfig::new()
            .with_agent_registry(
                AgentRegistry::builder()
                    .register(lead.clone())
                    .register(specialist.clone())
                    .build()
                    .unwrap(),
            )
            .with_prepared_capabilities(
                [Arc::new(LeadSurface(surface.clone())) as Arc<dyn Capability>],
                surface,
            );
        scripts.push(
            "lead",
            Step::Respond(tool_call(
                "l-1",
                "l-call-1",
                "transfer_to_specialist",
                json!({}),
            )),
        );
        scripts.push(
            "specialist",
            Step::Respond(tool_call("s-1", "s-call-1", "deploy", json!({}))),
        );
        let paused = Runner::run(
            RunRequest::new(
                AgentBinding::direct(lead),
                Arc::new(ScriptedResolver(scripts.clone())),
                RunId::new("run-handoff-resume-spawn"),
                CancelScope::root(),
                vec![ModelInputItem::Message(Message::user("go"))],
            )
            .with_config(config.clone())
            .with_agent_control(control.root()),
        )
        .await
        .unwrap();
        let mut state: RunState =
            serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
        let approval = state.pending_interruption_items().next().unwrap().clone();
        state.approve(&approval, false).unwrap();
        scripts.push(
            "specialist",
            Step::Respond(tool_call(
                "s-2",
                "s-call-2",
                "spawn_agent",
                json!({"task_name": "helper", "message": "help", "fork_turns": "none"}),
            )),
        );
        scripts.push("specialist", Step::Respond(final_message("s-3", "done")));
        scripts.push("specialist", Step::Respond(final_message("h-1", "done")));
        Runner::run(
            RunRequest::new(
                AgentBinding::direct(specialist),
                Arc::new(ScriptedResolver(scripts.clone())),
                state.run_id().clone(),
                CancelScope::root(),
                Vec::new(),
            )
            .with_config(config)
            .with_state(state)
            .with_agent_control(control.root()),
        )
        .await
        .unwrap();
        let helper = AgentPath::root().join("helper").unwrap();
        eventually_status(&control, &helper, |status| {
            matches!(status, AgentStatus::Completed(_))
        })
        .await;
        assert_eq!(scripts.temperatures("lead"), vec![Some(0.31)]);
        assert_eq!(scripts.temperatures("specialist"), vec![None; 4]);
    }
}

/// A factory can return a new declaration with the same ID and tool objects. The host must
/// explicitly select a prepared surface for that declaration rather than inheriting by ID.
#[tokio::test]
async fn a_factorys_new_declaration_with_the_same_id_does_not_inherit_the_prepared_surface() {
    let quick: Arc<dyn Tool> = Arc::new(QuickTool::new());
    for surface in [Vec::new(), vec![quick]] {
        let scripts = Arc::new(Scripts::default());
        let lead = lead_with(&surface);
        let other = lead.to_builder().instructions("other").build().unwrap();
        let control = AgentControl::new()
            .with_spawned_agent_factory(Arc::new(PreparingFactory::new(other, false)))
            .unwrap();
        scripts.push(
            "lead",
            Step::Respond(tool_call(
                "l-1",
                "l-1-call",
                "spawn_agent",
                json!({"task_name": "helper", "message": "help", "fork_turns": "none"}),
            )),
        );
        scripts.push("lead", Step::Respond(final_message("l-2", "done")));
        scripts.push("other", Step::Respond(final_message("h-1", "done")));

        let request = RunRequest::new(
            AgentBinding::direct(lead),
            Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
            RunId::new("run-root"),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("do the work"))],
        )
        .with_agent_control(control.root())
        .with_prepared_capabilities(
            [Arc::new(LeadSurface(surface.clone())) as Arc<dyn Capability>],
            surface,
        );
        assert_eq!(Runner::run(request).await.unwrap().final_text(), "done");
        let helper = AgentPath::root().join("helper").unwrap();
        eventually_status(&control, &helper, |status| {
            matches!(status, AgentStatus::Completed(_))
        })
        .await;

        assert_eq!(scripts.temperatures("lead"), vec![Some(0.31), Some(0.31)]);
        assert_eq!(scripts.temperatures("other"), vec![None]);
    }
}

/// Runs the lead with `surface` prepared on it and a worker installed as an agent tool, which
/// inherits the lead's run configuration.
async fn ask_worker_tool_beside(surface: Vec<Arc<dyn Tool>>) -> Arc<Scripts> {
    let scripts = Arc::new(Scripts::default());
    let worker = worker_agent(Vec::new());
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(
        worker.as_tool().tool_name("ask_worker").build().unwrap(),
    )];
    tools.extend(surface.iter().cloned());
    let lead = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(tools)
        .build()
        .unwrap();
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-1",
            "l-1-call",
            "ask_worker",
            json!({"input": "check"}),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "done")));
    scripts.push("worker", Step::Respond(final_message("w-1", "checked")));

    let request = RunRequest::new(
        AgentBinding::direct(lead),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("do the work"))],
    )
    .with_prepared_capabilities(
        [Arc::new(LeadSurface(surface.clone())) as Arc<dyn Capability>],
        surface,
    );
    assert_eq!(Runner::run(request).await.unwrap().final_text(), "done");
    // The nested run answered the call rather than failing to bind the lead's surface.
    let lead = scripts.inputs("lead");
    assert_eq!(texts(&lead[1]).last().map(String::as_str), Some("checked"));
    assert_eq!(scripts.temperatures("lead"), vec![Some(0.31), Some(0.31)]);
    assert_eq!(scripts.temperatures("worker"), vec![None]);
    scripts
}

/// An agent tool inherits its parent's run configuration, as the reference's nested run reads the
/// tool context's `run_config`; the lead's prepared tool objects still belong to the lead, so the
/// nested worker runs without them instead of failing to bind them.
#[tokio::test]
async fn an_agent_tool_does_not_inherit_the_parents_prepared_tools() {
    let quick: Arc<dyn Tool> = Arc::new(QuickTool::new());
    let scripts = ask_worker_tool_beside(vec![quick]).await;

    for tools in scripts.tools("lead") {
        assert!(tools.contains(&"quick_look".to_owned()));
    }
    let worker = scripts.tools("worker");
    assert_eq!(worker.len(), 1);
    assert!(!worker[0].contains(&"quick_look".to_owned()));
}

/// Without tools the declaration alone decides, and the nested worker does not receive the lead's
/// runtime contributions.
#[tokio::test]
async fn an_agent_tool_does_not_inherit_a_tool_less_prepared_surface() {
    ask_worker_tool_beside(Vec::new()).await;
}

/// A child that runs the lead's own declaration holds the prepared tool objects, and keeps the
/// surface with its runtime contributions.
#[tokio::test]
async fn a_child_of_the_same_declaration_keeps_the_prepared_surface() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let quick: Arc<dyn Tool> = Arc::new(QuickTool::new());
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-1",
            "l-1-call",
            "spawn_agent",
            json!({"task_name": "helper", "message": "help", "fork_turns": "none"}),
        )),
    );
    // The lead's next turn and the helper's first both read this script; both conclude.
    scripts.push("lead", Step::Respond(final_message("l-2", "done")));
    scripts.push("lead", Step::Respond(final_message("h-1", "done")));

    let request = RunRequest::new(
        AgentBinding::direct(lead_with(std::slice::from_ref(&quick))),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("do the work"))],
    )
    .with_agent_control(control.root())
    .with_prepared_capabilities(
        [Arc::new(LeadSurface(vec![Arc::clone(&quick)])) as Arc<dyn Capability>],
        [quick],
    );
    assert_eq!(Runner::run(request).await.unwrap().final_text(), "done");
    let helper = AgentPath::root().join("helper").unwrap();
    eventually_status(&control, &helper, |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    let tools = scripts.tools("lead");
    assert_eq!(tools.len(), 3);
    for advertised in tools {
        assert!(advertised.contains(&"quick_look".to_owned()));
    }
    assert_eq!(
        scripts.temperatures("lead"),
        vec![Some(0.31), Some(0.31), Some(0.31)]
    );
}

/// A tool that runs only once the host approves it.
struct GuardedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    /// Told when a call starts, for a tool that then never finishes.
    hangs: Option<Arc<tokio::sync::Notify>>,
}

impl GuardedTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("deploy").unwrap(),
            schema: ToolSchema::new(
                "deploy",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            hangs: None,
        }
    }

    /// The same tool, whose approved call tells `started` and then runs until it is cancelled.
    fn hanging(started: Arc<tokio::sync::Notify>) -> Self {
        Self {
            hangs: Some(started),
            ..Self::new()
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
        if let Some(started) = &self.hangs {
            started.notify_one();
            return std::future::pending().await;
        }
        Ok(ToolOutput::text("deployed"))
    }
}

/// Spawns a worker whose first action needs approval, and returns once its run has paused.
async fn spawn_guarded_worker(scripts: &Arc<Scripts>, control: &AgentControl) -> PausedAgentRun {
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "ship it")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "deploy", json!({}))),
    );
    Runner::run(root_request(
        scripts,
        control,
        worker_agent(vec![Arc::new(GuardedTool::new())]),
    ))
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .expect("the worker pauses for approval")
}

#[tokio::test]
async fn a_background_run_paused_for_approval_continues_once_the_host_answers() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let paused = spawn_guarded_worker(&scripts, &control).await;
    assert_eq!(paused.path(), &worker_path());
    // The run is not over: the agent stays running and reports nothing to its parent yet.
    assert_eq!(control.status(&worker_path()), Some(AgentStatus::Running));
    assert!(!control.root().has_pending_mail());

    // Mail sent while paused reaches the run once it continues.
    control
        .root()
        .send(
            "worker",
            "also tag the release".to_owned(),
            MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap();

    let mut state = paused.state().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    assert_eq!(pending.len(), 1);
    state.approve(&pending[0], false).unwrap();
    scripts.push(
        "worker",
        Step::Respond(final_message("w-2", "deployed and tagged")),
    );
    control.resume(&worker_path(), state).unwrap();

    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("deployed and tagged".to_owned()))
    })
    .await;
    assert!(control.paused_runs().is_empty());
    // The same run continued: the approved call ran, and the waiting mail followed its output.
    let worker = scripts.inputs("worker");
    assert_eq!(worker.len(), 2);
    let resumed = texts(&worker[1]);
    assert_eq!(resumed[resumed.len() - 2], "deployed");
    assert_eq!(
        resumed.last().unwrap(),
        &format!("{NEW_TASK}also tag the release")
    );
    assert!(control.root().has_pending_mail());
}

#[tokio::test]
async fn a_paused_run_is_resumed_only_from_its_own_checkpoint_and_ends_on_interrupt() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let paused = spawn_guarded_worker(&scripts, &control).await;

    let foreign = RunState::start(RunId::new("someone-else"));
    let refused = control.resume(&worker_path(), foreign).unwrap_err();
    assert!(
        refused
            .to_string()
            .starts_with("the state does not belong to the paused run")
    );
    assert_eq!(
        control.paused_runs().len(),
        1,
        "a refused resume keeps the pause"
    );

    let previous = control.root().interrupt("worker").await.unwrap();
    assert_eq!(previous, AgentStatus::Running);
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Interrupted
    })
    .await;
    assert!(control.paused_runs().is_empty());
    let late = control
        .resume(&worker_path(), paused.state().clone())
        .unwrap_err();
    assert_eq!(late.to_string(), "agent `/root/worker` has no paused run");
}

#[tokio::test]
async fn spawned_approval_records_share_the_run_sequence_and_refuse_stale_batches() {
    use ra_core::event::{ApprovalEvent, ApprovalResolution};

    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "ship it")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "first", "deploy", json!({}))),
    );
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-2", "second", "deploy", json!({}))),
    );
    scripts.push("worker", Step::Respond(final_message("w-3", "deployed")));
    Runner::run(with_sink(
        root_request(
            &scripts,
            &control,
            worker_agent(vec![Arc::new(GuardedTool::new())]),
        ),
        &sink,
    ))
    .await
    .unwrap();
    let mut changes = control.subscribe_paused_runs();
    let first = control.wait_for_paused_run().await;
    let mut state = first.state().clone();
    let item = state.pending_interruption_items().next().unwrap().clone();
    control
        .record_approval(
            first.path(),
            &mut state,
            ApprovalEvent::resolved(CallId::new("first"), ApprovalResolution::Approved),
        )
        .unwrap();
    state.approve(&item, false).unwrap();
    control.resume(first.path(), state).unwrap();
    let second = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(paused) = control.paused_runs().into_iter().find(|paused| {
                paused.state().pending_interruption_items().any(|item| matches!(
                    item.kind(), RunItemKind::ToolApproval(approval) if approval.call_id().as_str() == "second"
                ))
            }) { break paused; }
            changes.changed().await.unwrap();
        }
    }).await.unwrap();
    assert_eq!(first.state().run_id(), second.state().run_id());
    assert!(
        control
            .record_approval(
                first.path(),
                &mut first.state().clone(),
                ApprovalEvent::resolved(CallId::new("first"), ApprovalResolution::Approved)
            )
            .is_err()
    );
    let mut state = second.state().clone();
    let item = state.pending_interruption_items().next().unwrap().clone();
    control
        .record_approval(
            second.path(),
            &mut state,
            ApprovalEvent::resolved(CallId::new("second"), ApprovalResolution::Approved),
        )
        .unwrap();
    state.approve(&item, false).unwrap();
    control.resume(second.path(), state).unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let approvals: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|event| matches!(event.body(), HostEventBody::Approval(_)))
        .collect();
    assert_eq!(approvals.len(), 2);
    assert!(
        approvals
            .iter()
            .all(|event| event.run_id() == first.state().run_id())
    );
    assert_unique_and_rising(&seqs_of(&sink, first.state().run_id().as_str()));
    control.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Closing and cancellation
// ---------------------------------------------------------------------------------------------

/// Kills and reaps a real process when dropped, as a tool that owns its process must.
struct OwnedProcess(std::process::Child);

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A tool that starts a long-running process, records its pid, and holds it until the call is
/// dropped.
struct HoldProcessTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    pids: Arc<Mutex<Vec<u32>>>,
}

impl HoldProcessTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("hold_process").unwrap(),
            schema: ToolSchema::new(
                "hold_process",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            pids: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Tool for HoldProcessTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::default()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .spawn()
            .map_err(|error| Error::caller(error.to_string()))?;
        self.pids.lock().unwrap().push(child.id());
        let _process = OwnedProcess(child);
        std::future::pending().await
    }
}

fn is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn eventually(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn helper_path() -> AgentPath {
    worker_path().join("helper").unwrap()
}

fn spawn_call(id: &str, task_name: &str, agent_type: &str, message: &str) -> ModelResponse {
    tool_call(
        id,
        &format!("{id}-call"),
        "spawn_agent",
        json!({"task_name": task_name, "message": message, "agent_type": agent_type}),
    )
}

fn helper_agent(extra: Vec<Arc<dyn Tool>>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("helper"))
        .name("Helper")
        .instructions("helper")
        .tools(extra)
        .build()
        .unwrap()
}

/// A root run whose registry declares a worker and the helper the worker spawns below itself.
fn tree_request(
    scripts: &Arc<Scripts>,
    control: &AgentControl,
    cancel: CancelScope,
    tools: &[Arc<dyn Tool>],
) -> RunRequest {
    let registry = AgentRegistry::builder()
        .register(worker_agent(tools.to_vec()))
        .register(helper_agent(tools.to_vec()))
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(parent_agent()),
        Arc::new(ScriptedResolver(Arc::clone(scripts))) as Arc<dyn ModelResolver>,
        RunId::generate(),
        cancel,
        vec![ModelInputItem::Message(Message::user("do the work"))],
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root())
}

fn paths(control: &AgentControl) -> Vec<String> {
    control
        .agents()
        .iter()
        .map(|agent| agent.agent_path().to_string())
        .collect()
}

/// Scripts a worker that spawns a helper, and both then hold a process each.
fn script_holding_tree(scripts: &Scripts) {
    scripts.push(
        "worker",
        Step::Respond(spawn_call("w-1", "helper", "helper", "help")),
    );
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-2", "w-2-call", "hold_process", json!({}))),
    );
    scripts.push(
        "helper",
        Step::Respond(tool_call("h-1", "h-1-call", "hold_process", json!({}))),
    );
}

#[tokio::test]
async fn close_shuts_down_an_agent_and_its_live_descendants() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let hold = HoldProcessTool::new();
    let pids = Arc::clone(&hold.pids);
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(hold)];
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "go")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    script_holding_tree(&scripts);
    Runner::run(tree_request(
        &scripts,
        &control,
        CancelScope::root(),
        &tools,
    ))
    .await
    .unwrap();
    eventually(|| pids.lock().unwrap().len() == 2, "both processes started").await;

    let previous = tokio::time::timeout(Duration::from_secs(10), control.close(&worker_path()))
        .await
        .expect("close drains in time")
        .unwrap();
    assert_eq!(previous, AgentStatus::Running);
    // Checked at once: close returns only after each agent's run has ended.
    let started = pids.lock().unwrap().clone();
    assert!(started.iter().all(|pid| !is_alive(*pid)), "{started:?}");
    assert_eq!(control.status(&worker_path()), None);
    assert_eq!(control.status(&helper_path()), None);
    assert_eq!(paths(&control), ["/root"]);
    let error = control
        .root()
        .send(
            "worker",
            "hello".to_owned(),
            MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "live agent path `/root/worker` not found"
    );
    // A closed agent reports nothing to its parent.
    assert!(!control.root().has_pending_mail());
}

#[tokio::test]
async fn closing_a_descendant_and_then_the_root_frees_the_paths_for_new_spawns() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "go")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(spawn_call("w-1", "helper", "helper", "help")),
    );
    scripts.push("worker", Step::Hang);
    scripts.push("helper", Step::Hang);
    Runner::run(tree_request(&scripts, &control, CancelScope::root(), &[]))
        .await
        .unwrap();
    eventually_status(&control, &helper_path(), |status| {
        *status == AgentStatus::Running
    })
    .await;

    control.close(&helper_path()).await.unwrap();
    assert_eq!(paths(&control), ["/root", "/root/worker"]);
    assert_eq!(control.status(&worker_path()), Some(AgentStatus::Running));

    control.close(&AgentPath::root()).await.unwrap();
    assert_eq!(paths(&control), ["/root"]);

    // The root stays and the tree keeps accepting spawns, at the path just freed.
    scripts.push("lead", Step::Respond(spawn_worker("l-3", "again")));
    scripts.push("lead", Step::Respond(final_message("l-4", "spawned again")));
    scripts.push("worker", Step::Respond(final_message("w-2", "second life")));
    Runner::run(tree_request(&scripts, &control, CancelScope::root(), &[]))
        .await
        .unwrap();
    let status = eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    assert_eq!(
        status,
        AgentStatus::Completed(Some("second life".to_owned()))
    );
}

#[tokio::test]
async fn close_reports_the_previous_status_and_refuses_an_unknown_agent() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "go")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", Step::Respond(final_message("w-1", "done")));
    Runner::run(tree_request(&scripts, &control, CancelScope::root(), &[]))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    assert_eq!(
        control.close(&worker_path()).await.unwrap(),
        AgentStatus::Completed(Some("done".to_owned()))
    );
    assert_eq!(
        control.close(&worker_path()).await.unwrap_err().to_string(),
        "live agent path `/root/worker` not found"
    );
}

#[tokio::test]
async fn a_close_the_caller_stops_waiting_for_still_stops_every_agent() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let hold = HoldProcessTool::new();
    let pids = Arc::clone(&hold.pids);
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(hold)];
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "go")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    script_holding_tree(&scripts);
    Runner::run(tree_request(
        &scripts,
        &control,
        CancelScope::root(),
        &tools,
    ))
    .await
    .unwrap();
    eventually(|| pids.lock().unwrap().len() == 2, "both processes started").await;

    // Polled once, then dropped while it waits for the first agent to stop.
    let abandoned = control.close(&worker_path()).now_or_never();
    assert!(abandoned.is_none(), "the close was still waiting");

    eventually(|| paths(&control) == ["/root"], "the close finished").await;
    let started = pids.lock().unwrap().clone();
    eventually(
        || started.iter().all(|pid| !is_alive(*pid)),
        "both processes are gone",
    )
    .await;
}

#[tokio::test]
async fn spawns_below_an_agent_are_refused_while_its_subtree_is_being_closed() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "go")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", Step::Hang);
    Runner::run(tree_request(&scripts, &control, CancelScope::root(), &[]))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Running
    })
    .await;

    let abandoned = control.close(&AgentPath::root()).now_or_never();
    assert!(abandoned.is_none(), "the close was still waiting");
    let error = control
        .root()
        .spawn(SpawnAgentRequest::new("new_worker", "late"))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the agents below `/root` are being closed; spawn again once that is done"
    );

    eventually(|| paths(&control) == ["/root"], "the close finished").await;
    // Admission is back once the close is done; outside a runner the spawn now fails for that
    // reason instead.
    let error = control
        .root()
        .spawn(SpawnAgentRequest::new("new_worker", "late"))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "agents can only be spawned by an agent running under a runner"
    );
}

#[tokio::test]
async fn a_cancelled_root_run_leaves_spawned_agents_running_by_default() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let hold = HoldProcessTool::new();
    let pids = Arc::clone(&hold.pids);
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(hold)];
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "go")));
    scripts.push("lead", Step::Hang);
    script_holding_tree(&scripts);
    let cancel = CancelScope::root();
    let run = tokio::spawn(Runner::run(tree_request(
        &scripts,
        &control,
        cancel.clone(),
        &tools,
    )));
    eventually(|| pids.lock().unwrap().len() == 2, "both processes started").await;

    cancel.cancel(ra_core::cancel::CancelReason::UserInterrupt);
    let error = run.await.unwrap().unwrap_err();
    assert!(error.is_cancelled(), "{error:?}");
    assert_eq!(
        paths(&control),
        ["/root", "/root/worker", "/root/worker/helper"]
    );
    assert_eq!(control.status(&worker_path()), Some(AgentStatus::Running));
    let started = pids.lock().unwrap().clone();
    assert!(started.iter().all(|pid| is_alive(*pid)));

    tokio::time::timeout(Duration::from_secs(10), control.shutdown())
        .await
        .expect("shutdown drains in time");
    assert!(started.iter().all(|pid| !is_alive(*pid)));
}

#[tokio::test]
async fn a_cancelled_root_run_closes_every_agent_and_its_processes_before_returning_when_asked() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new().with_close_descendants_on_cancel(true);
    let hold = HoldProcessTool::new();
    let pids = Arc::clone(&hold.pids);
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(hold)];
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "go")));
    scripts.push("lead", Step::Hang);
    script_holding_tree(&scripts);
    let cancel = CancelScope::root();
    let run = tokio::spawn(Runner::run(tree_request(
        &scripts,
        &control,
        cancel.clone(),
        &tools,
    )));
    eventually(|| pids.lock().unwrap().len() == 2, "both processes started").await;

    cancel.cancel(ra_core::cancel::CancelReason::UserInterrupt);
    let error = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("the run returns in time")
        .unwrap()
        .unwrap_err();
    assert!(error.is_cancelled(), "{error:?}");
    // Checked at once: the run returns only after the agents below it have stopped, down to the
    // grandchild and the processes their tools held.
    assert_eq!(paths(&control), ["/root"]);
    let started = pids.lock().unwrap().clone();
    assert!(started.iter().all(|pid| !is_alive(*pid)), "{started:?}");
    assert_eq!(
        control.status(&AgentPath::root()),
        Some(AgentStatus::Interrupted)
    );
}

#[tokio::test]
async fn an_interrupted_agent_closes_the_agents_below_it_and_stays_when_asked() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new().with_close_descendants_on_cancel(true);
    let hold = HoldProcessTool::new();
    let pids = Arc::clone(&hold.pids);
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(hold)];
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "go")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    script_holding_tree(&scripts);
    Runner::run(tree_request(
        &scripts,
        &control,
        CancelScope::root(),
        &tools,
    ))
    .await
    .unwrap();
    eventually(|| pids.lock().unwrap().len() == 2, "both processes started").await;

    control.root().interrupt("worker").await.unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Interrupted
    })
    .await;
    eventually(
        || control.status(&helper_path()).is_none(),
        "the helper is closed",
    )
    .await;
    assert_eq!(paths(&control), ["/root", "/root/worker"]);
    let started = pids.lock().unwrap().clone();
    eventually(
        || started.iter().all(|pid| !is_alive(*pid)),
        "both processes are gone",
    )
    .await;

    // The interrupted agent itself is still there and takes a follow-up.
    scripts.push("worker", Step::Respond(final_message("w-3", "resumed")));
    control
        .root()
        .send(
            "worker",
            "carry on".to_owned(),
            MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap();
    let status = eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    assert_eq!(status, AgentStatus::Completed(Some("resumed".to_owned())));
}

// ---------------------------------------------------------------------------------------------
// Rollout budget
// ---------------------------------------------------------------------------------------------

fn billed(response: ModelResponse, usage: RequestUsage) -> ModelResponse {
    response.with_usage(Usage::from_request(usage))
}

fn rollout_budget() -> RolloutBudgetConfig {
    RolloutBudgetConfig::new(100, vec![75, 50, 25])
}

fn rollout_budget_message(remaining_tokens: u64) -> String {
    format!(
        "<rollout_budget>\nYou have {remaining_tokens} weighted tokens left in the shared session token budget.\n</rollout_budget>"
    )
}

fn rollout_budget_texts(input: &[ModelInputItem]) -> Vec<String> {
    texts(input)
        .into_iter()
        .filter(|text| text.starts_with("<rollout_budget>"))
        .collect()
}

/// A request continuing `previous` with a new user message, as a host's next turn does.
fn next_turn(
    scripts: &Arc<Scripts>,
    control: &AgentControl,
    previous: &ra_runtime::runner::RunResult,
    message: &str,
) -> RunRequest {
    let mut input = previous.continuation_input(ContinuationInput::Normalized);
    input.push(ModelInputItem::Message(Message::user(message)));
    let registry = AgentRegistry::builder()
        .register(worker_agent(Vec::new()))
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(parent_agent()),
        Arc::new(ScriptedResolver(Arc::clone(scripts))) as Arc<dyn ModelResolver>,
        RunId::generate(),
        CancelScope::root(),
        input,
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root())
}

#[tokio::test]
async fn each_run_is_reminded_of_the_weighted_budget_left_at_its_start() {
    // Codex's `adds_weighted_initial_and_threshold_reminders`, weighted-usage case.
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_budget(
            rollout_budget()
                .with_sampling_token_weight(2.0)
                .with_prefill_token_weight(0.5),
        )
        .unwrap();
    scripts.push(
        "lead",
        Step::Respond(billed(
            final_message("l-1", "first answer"),
            RequestUsage::new(60, 15).with_cached_input_tokens(40),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "second answer")));

    let first = Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    Runner::run(next_turn(&scripts, &control, &first, "second turn"))
        .await
        .unwrap();

    let lead = scripts.inputs("lead");
    assert_eq!(
        rollout_budget_texts(&lead[0]),
        vec![rollout_budget_message(100)]
    );
    // 15 output tokens at 2 and 20 uncached input tokens at 0.5 leave 60, past the 75 threshold.
    // The first reminder stays in history; the new one follows the new input.
    assert_eq!(
        rollout_budget_texts(&lead[1]),
        vec![rollout_budget_message(100), rollout_budget_message(60)]
    );
    assert_eq!(texts(&lead[1]).last().unwrap(), &rollout_budget_message(60));
}

#[tokio::test]
async fn a_run_is_not_reminded_again_until_another_threshold_is_crossed() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_budget(rollout_budget())
        .unwrap();
    scripts.push(
        "lead",
        Step::Respond(billed(
            final_message("l-1", "first answer"),
            RequestUsage::new(10, 0),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "second answer")));

    let first = Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    Runner::run(next_turn(&scripts, &control, &first, "second turn"))
        .await
        .unwrap();

    // 90 left is above every threshold, so the second run brings nothing new.
    assert_eq!(
        rollout_budget_texts(&scripts.inputs("lead")[1]),
        vec![rollout_budget_message(100)]
    );
}

#[tokio::test]
async fn subagent_usage_draws_from_the_shared_budget() {
    // Codex's `subagent_usage_draws_from_the_shared_budget`.
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_budget(rollout_budget())
        .unwrap();
    scripts.push(
        "lead",
        Step::Respond(billed(
            spawn_worker("l-1", "consume child budget"),
            RequestUsage::new(10, 0),
        )),
    );
    scripts.push(
        "lead",
        Step::Respond(billed(
            final_message("l-2", "spawned"),
            RequestUsage::new(10, 0),
        )),
    );
    scripts.push(
        "worker",
        Step::Respond(billed(
            final_message("w-1", "consumed"),
            RequestUsage::new(30, 0),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-3", "reported")));

    let first = Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    Runner::run(next_turn(
        &scripts,
        &control,
        &first,
        "report the shared budget",
    ))
    .await
    .unwrap();

    // The child forks the root's reminder with the conversation, as Codex keeps it in a fork, and
    // is told what is left itself when its run starts.
    let worker = rollout_budget_texts(&scripts.inputs("worker")[0]);
    assert_eq!(worker.len(), 2);
    assert_eq!(worker[0], rollout_budget_message(100));
    let lead = scripts.inputs("lead");
    assert_eq!(
        rollout_budget_texts(&lead[2]).last(),
        Some(&rollout_budget_message(50))
    );
    // A spawned agent's spend is the tree's, not the spawning run's: the root's ledger holds only
    // its own calls, as Codex keeps each thread's usage to itself.
    assert_eq!(first.state().tokens_used(), 20);
}

#[tokio::test]
async fn a_final_answer_that_spends_the_budget_is_kept_but_not_delivered() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_budget(RolloutBudgetConfig::new(30, Vec::new()))
        .unwrap();
    scripts.push(
        "lead",
        Step::Respond(billed(
            final_message("l-1", "the answer"),
            RequestUsage::new(30, 0),
        )),
    );

    let result = Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();

    // Codex fails the request whose usage spent the budget: the answer is recorded, not delivered.
    assert_eq!(
        result.outcome().finish_reason(),
        Some(FinishReason::BudgetExhausted)
    );
    assert!(result.final_message().is_none());
    assert!(result.new_items().iter().any(|item| matches!(
        item.kind(),
        RunItemKind::Message(message) if message.text_content() == "the answer"
    )));
}

#[tokio::test]
async fn an_exhausted_budget_fails_the_current_run_and_every_later_one_after_its_request() {
    // Codex's `exhausted_budget_fails_current_and_later_turns`.
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_budget(RolloutBudgetConfig::new(30, vec![20, 10]))
        .unwrap();
    scripts.push(
        "lead",
        Step::Respond(billed(
            tool_call("l-1", "l-1-call", "list_agents", json!({})),
            RequestUsage::new(30, 0),
        )),
    );
    scripts.push(
        "lead",
        Step::Respond(billed(
            final_message("l-2", "too late"),
            RequestUsage::new(1, 0),
        )),
    );

    let first = Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    assert_eq!(
        first.outcome().finish_reason(),
        Some(FinishReason::BudgetExhausted)
    );
    // The call the spending response issued still ran, as Codex drains in-flight tool calls.
    assert!(
        first
            .new_items()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(_)))
    );
    assert_eq!(scripts.inputs("lead").len(), 1);

    // A later run still makes its request, and fails after it.
    let second = Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    assert_eq!(
        second.outcome().finish_reason(),
        Some(FinishReason::BudgetExhausted)
    );
    assert!(second.final_message().is_none());
    assert_eq!(scripts.inputs("lead").len(), 2);
}

/// Recovers a refusal and declines every other terminal condition.
struct RecoverRefusalOnly {
    history: bool,
}

#[async_trait]
impl RunErrorHandler for RecoverRefusalOnly {
    async fn handle(
        &self,
        input: RunErrorHandlerInput<'_>,
    ) -> Result<Option<RunErrorHandlerResult>> {
        Ok((input.error().code() == "provider.refusal").then(|| {
            RunErrorHandlerResult::new(Message::assistant("recovered", OutputPhase::Final))
                .with_write_to_history(self.history)
        }))
    }
}

#[tokio::test]
async fn a_recovered_answer_on_a_spent_budget_is_kept_but_not_delivered() {
    for streamed in [false, true] {
        for history in [false, true] {
            let scripts = Arc::new(Scripts::default());
            let control = AgentControl::new()
                .with_rollout_budget(rollout_budget())
                .unwrap();
            let refusal = RunItem::new(
                ItemId::new("l-1"),
                RunItemKind::Message(Message::new(
                    MessageRole::Assistant,
                    vec![ContentBlock::refusal("declined")],
                )),
            );
            scripts.push(
                "lead",
                Step::Respond(billed(
                    ModelResponse::new(vec![refusal]),
                    RequestUsage::new(200, 50),
                )),
            );
            let registry = AgentRegistry::builder()
                .register(worker_agent(Vec::new()))
                .build()
                .unwrap();
            let request = root_request(&scripts, &control, worker_agent(Vec::new())).with_config(
                RunConfig::new()
                    .with_agent_registry(registry)
                    .with_error_handler(Arc::new(RecoverRefusalOnly { history })),
            );

            let mut events = Vec::new();
            let result = if streamed {
                let mut stream = Runner::run_streamed(request);
                while let Some(event) = stream.next_event().await {
                    events.push(event);
                }
                stream.finish().await.unwrap()
            } else {
                Runner::run(request).await.unwrap()
            };

            assert_eq!(
                result.outcome().finish_reason(),
                Some(FinishReason::BudgetExhausted)
            );
            assert!(result.final_message().is_none());
            assert!(result.final_output().is_none());
            assert!(
                events
                    .iter()
                    .all(|event| !matches!(event, RunStreamEvent::FinalMessage(_)))
            );
            assert_eq!(
                result.new_items().iter().any(|item| matches!(
                    item.kind(),
                    RunItemKind::Message(message) if message.text_content() == "recovered"
                )),
                history
            );
        }
    }
}

#[tokio::test]
async fn a_turn_paused_on_a_spent_budget_ends_once_its_answers_are_settled() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_budget(RolloutBudgetConfig::new(30, Vec::new()))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "ship it")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(billed(
            tool_call("w-1", "w-1-call", "deploy", json!({})),
            RequestUsage::new(30, 0),
        )),
    );
    scripts.push("worker", Step::Respond(final_message("w-2", "unreachable")));
    Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(vec![Arc::new(GuardedTool::new())]),
    ))
    .await
    .unwrap();
    let paused = tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .expect("the turn still pauses for its approval");

    let mut state = paused.state().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    control.resume(&worker_path(), state).unwrap();
    let status = eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Errored(_))
    })
    .await;

    assert_eq!(
        status,
        AgentStatus::Errored("the agent stopped before concluding (budget_exhausted)".to_owned())
    );
    // The approved call ran; the model was not asked again.
    assert_eq!(scripts.inputs("worker").len(), 1);
}

#[tokio::test]
async fn an_agent_tool_run_inside_the_tree_is_charged_and_stopped_by_its_budget() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_budget(RolloutBudgetConfig::new(30, Vec::new()))
        .unwrap();
    let nested = AgentSpec::builder()
        .id(AgentId::new("nested"))
        .name("Nested")
        .instructions("nested")
        .tool(Arc::new(QuickTool::new()))
        .build()
        .unwrap();
    let lead = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tool(Arc::new(nested.as_tool().build().unwrap()))
        .build()
        .unwrap();
    scripts.push(
        "lead",
        Step::Respond(billed(
            tool_call("l-1", "l-1-call", "nested", json!({"input": "look"})),
            RequestUsage::new(10, 0),
        )),
    );
    scripts.push(
        "nested",
        Step::Respond(billed(
            tool_call("n-1", "n-1-call", "quick_look", json!({})),
            RequestUsage::new(25, 0),
        )),
    );
    scripts.push("nested", Step::Respond(final_message("n-2", "unreachable")));
    scripts.push("lead", Step::Respond(final_message("l-2", "unreachable")));

    let result = Runner::run(
        RunRequest::new(
            AgentBinding::direct(lead),
            Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("do the work"))],
        )
        .with_agent_control(control.root()),
    )
    .await
    .unwrap();

    // The nested call reached the tree's budget through the run that started it, and the nested
    // run stopped on it before calling again; so did the root.
    assert_eq!(scripts.inputs("nested").len(), 1);
    assert_eq!(scripts.inputs("lead").len(), 1);
    assert_eq!(
        result.outcome().finish_reason(),
        Some(FinishReason::BudgetExhausted)
    );
    // An agent-tool run is not an agent of the tree and is told nothing about its budget.
    assert!(rollout_budget_texts(&scripts.inputs("nested")[0]).is_empty());
}

#[tokio::test]
async fn a_run_continued_after_approval_is_not_reminded_again() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_budget(rollout_budget())
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "ship it")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    // Past the 75 threshold, which a run starting now would be told about.
    scripts.push(
        "worker",
        Step::Respond(billed(
            tool_call("w-1", "w-1-call", "deploy", json!({})),
            RequestUsage::new(30, 0),
        )),
    );
    scripts.push("worker", Step::Respond(final_message("w-2", "deployed")));
    Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(vec![Arc::new(GuardedTool::new())]),
    ))
    .await
    .unwrap();
    let paused = tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .expect("the worker pauses for approval");

    let mut state = paused.state().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    control.resume(&worker_path(), state).unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    // Answering an approval finishes the turn it stopped; Codex reminds at the start of a turn.
    let worker = scripts.inputs("worker");
    assert_eq!(worker.len(), 2);
    assert_eq!(
        rollout_budget_texts(&worker[1]),
        rollout_budget_texts(&worker[0])
    );
}

#[test]
fn rollout_budget_configuration_is_checked_like_codex() {
    for (config, message) in [
        (
            RolloutBudgetConfig::new(0, Vec::new()),
            "rollout budget `limit_tokens` must be positive",
        ),
        (
            RolloutBudgetConfig::new(100, vec![0]),
            "rollout budget `reminder_at_remaining_tokens` must contain only positive values below `limit_tokens`",
        ),
        (
            RolloutBudgetConfig::new(100, vec![100]),
            "rollout budget `reminder_at_remaining_tokens` must contain only positive values below `limit_tokens`",
        ),
        (
            RolloutBudgetConfig::new(100, Vec::new()).with_sampling_token_weight(-1.0),
            "rollout budget `sampling_token_weight` must be finite and non-negative",
        ),
        (
            RolloutBudgetConfig::new(100, Vec::new()).with_prefill_token_weight(f64::NAN),
            "rollout budget `prefill_token_weight` must be finite and non-negative",
        ),
    ] {
        let error = AgentControl::new().with_rollout_budget(config).unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
    }
    let error = AgentControl::new()
        .with_rollout_budget(rollout_budget())
        .unwrap()
        .with_rollout_budget(rollout_budget())
        .unwrap_err();
    assert!(
        error.to_string().contains("already has a rollout budget"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------------------------
// Depth limit
// ---------------------------------------------------------------------------------------------

const DEPTH_LIMIT_REACHED: &str = "Agent depth limit reached. Solve the task yourself.";

fn offers_spawn(tools: &[String]) -> bool {
    tools.iter().any(|tool| tool == "spawn_agent")
}

#[tokio::test]
async fn an_agent_at_the_depth_limit_is_not_offered_the_tools_and_cannot_spawn() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new().with_max_depth(1);
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "work")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", Step::Respond(final_message("w-1", "worked")));

    Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(vec![Arc::new(QuickTool::new())]),
    ))
    .await
    .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    assert!(offers_spawn(&scripts.tools("lead")[0]));
    // Codex hides every collaboration tool from an agent that may spawn nothing; the rest stay.
    assert_eq!(scripts.tools("worker")[0], vec!["quick_look".to_owned()]);
    let worker = control.handle(&worker_path()).unwrap();
    assert!(worker.spawn_depth_exceeded());
    assert!(!control.root().spawn_depth_exceeded());
    let refusal = worker
        .spawn(SpawnAgentRequest::new("helper", "help"))
        .await
        .unwrap_err();
    assert_eq!(refusal.to_string(), DEPTH_LIMIT_REACHED);
}

#[tokio::test]
async fn a_zero_depth_limit_keeps_the_root_from_spawning() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new().with_max_depth(0);
    scripts.push("lead", Step::Respond(final_message("l-1", "alone")));

    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();

    assert!(scripts.tools("lead")[0].is_empty());
    let refusal = control
        .root()
        .spawn(SpawnAgentRequest::new("worker", "work"))
        .await
        .unwrap_err();
    assert_eq!(refusal.to_string(), DEPTH_LIMIT_REACHED);
}

#[tokio::test]
async fn without_a_depth_limit_every_agent_is_offered_the_tools() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "work")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", Step::Respond(final_message("w-1", "worked")));

    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    assert!(offers_spawn(&scripts.tools("worker")[0]));
    assert!(
        !control
            .handle(&worker_path())
            .unwrap()
            .spawn_depth_exceeded()
    );
}

// ---------------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------------

#[test]
fn agent_paths_resolve_like_codex() {
    let root = AgentPath::root();
    let worker = root.join("worker").unwrap();
    assert_eq!(worker.as_str(), "/root/worker");
    assert_eq!(worker.name(), "worker");
    assert_eq!(worker.parent(), Some(root.clone()));
    assert_eq!(root.parent(), None);
    assert_eq!(worker.resolve("sub").unwrap().as_str(), "/root/worker/sub");
    assert_eq!(worker.resolve("/root").unwrap(), root);
    assert_eq!(
        worker.resolve("/root/other").unwrap().as_str(),
        "/root/other"
    );
    assert!(root.join("root").is_err());
    assert!(root.join("..").is_err());
    assert!(root.join("UPPER").is_err());
    assert!(AgentPath::try_from("/other").is_err());
    assert!(AgentPath::try_from("/root/").is_err());
    assert!(worker.starts_with(&root));
    assert!(
        !AgentPath::try_from("/root/workers")
            .unwrap()
            .starts_with(&worker)
    );
}

#[test]
fn statuses_serialize_in_codex_wire_form() {
    for (status, wire) in [
        (AgentStatus::PendingInit, json!("pending_init")),
        (AgentStatus::Running, json!("running")),
        (AgentStatus::Interrupted, json!("interrupted")),
        (AgentStatus::Completed(None), json!({"completed": null})),
        (
            AgentStatus::Completed(Some("x".to_owned())),
            json!({"completed": "x"}),
        ),
        (
            AgentStatus::Errored("boom".to_owned()),
            json!({"errored": "boom"}),
        ),
        (AgentStatus::Shutdown, json!("shutdown")),
        (AgentStatus::NotFound, json!("not_found")),
    ] {
        assert_eq!(serde_json::to_value(&status).unwrap(), wire);
        assert_eq!(serde_json::from_value::<AgentStatus>(wire).unwrap(), status);
    }
}

#[test]
fn completion_messages_report_terminal_statuses_only() {
    let root = AgentPath::root();
    let worker = worker_path();
    let errored =
        completion_message(&root, &worker, &AgentStatus::Errored("boom".to_owned())).unwrap();
    assert_eq!(
        errored,
        InterAgentCommunication::new(
            worker.clone(),
            root.clone(),
            format!(
                "{FINAL_ANSWER}Agent errored: boom\n\nThis agent's turn failed. If you still need this agent, use the available collaboration tools to give it another task."
            ),
            false,
        )
    );
    assert!(completion_message(&root, &worker, &AgentStatus::Interrupted).is_none());
    assert!(completion_message(&root, &worker, &AgentStatus::Running).is_none());
    assert_eq!(
        completion_message(&root, &worker, &AgentStatus::Completed(None))
            .unwrap()
            .content(),
        FINAL_ANSWER
    );
}

// ---------------------------------------------------------------------------------------------
// Activity on the host timeline
// ---------------------------------------------------------------------------------------------

/// One recorded activity: the run and agent whose timeline holds it, its id, the agent it
/// happened to, and what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Activity {
    run: String,
    agent: String,
    id: String,
    path: String,
    kind: SubAgentActivityKind,
}

fn activities(sink: &InMemoryHostEventSink) -> Vec<Activity> {
    sink.events()
        .iter()
        .filter_map(|event| match event.body() {
            HostEventBody::Agent(AgentEvent::SubAgentActivity(activity)) => Some(Activity {
                run: event.run_id().to_string(),
                agent: event.agent_id().to_string(),
                id: activity.id().to_owned(),
                path: activity.agent_path().to_string(),
                kind: activity.activity().clone(),
            }),
            _ => None,
        })
        .collect()
}

fn activity(run: &str, agent: &str, id: &str, path: &str, kind: SubAgentActivityKind) -> Activity {
    Activity {
        run: run.to_owned(),
        agent: agent.to_owned(),
        id: id.to_owned(),
        path: path.to_owned(),
        kind,
    }
}

/// A parent's `wait_agent` that lets a child answer only once the parent's request is built, so
/// the answer arrives during the wait instead of being delivered at that model call.
fn wait_releasing(id: &str, child: &Arc<Notify>) -> Step {
    let child = Arc::clone(child);
    Step::After(
        Box::pin(async move { child.notify_one() }),
        tool_call(
            id,
            &format!("{id}-call"),
            "wait_agent",
            json!({"timeout_ms": 10_000}),
        ),
    )
}

/// A child's final answer, given once `gate` is released.
fn final_after(gate: &Arc<Notify>, id: &str, text: &str) -> Step {
    let gate = Arc::clone(gate);
    Step::After(
        Box::pin(async move { gate.notified().await }),
        final_message(id, text),
    )
}

fn with_sink(request: RunRequest, sink: &InMemoryHostEventSink) -> RunRequest {
    request.with_services(ToolServices::new().with_event_sink(Arc::new(sink.clone())))
}

/// Replaces the generated run id in a completion's id, which a test cannot know in advance.
fn completion_ids_checked(mut recorded: Vec<Activity>) -> Vec<Activity> {
    for activity in &mut recorded {
        if activity.kind == SubAgentActivityKind::Completed {
            let run = activity
                .id
                .strip_prefix("subagent-completed-")
                .expect("a completion is keyed by the run that completed");
            assert!(!run.is_empty());
            activity.id = "subagent-completed-*".to_owned();
        }
    }
    recorded
}

#[tokio::test]
async fn spawns_messages_and_completed_runs_are_recorded_on_the_callers_timeline() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    let worker_turn = Arc::new(Notify::new());

    scripts.push(
        "lead",
        Step::Respond(spawn_worker("l-1", "count the files")),
    );
    scripts.push("lead", wait_releasing("l-2", &worker_turn));
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-3",
            "l-3-call",
            "send_message",
            json!({"target": "worker", "message": "Include hidden files."}),
        )),
    );
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-4",
            "l-4-call",
            "followup_task",
            json!({"target": "worker", "message": "Count again."}),
        )),
    );
    scripts.push("lead", wait_releasing("l-5", &worker_turn));
    scripts.push("lead", Step::Respond(final_message("l-6", "done")));
    scripts.push("worker", final_after(&worker_turn, "w-1", "42 files"));
    scripts.push("worker", final_after(&worker_turn, "w-2", "45 files"));

    let request = with_sink(
        root_request(&scripts, &control, worker_agent(Vec::new())),
        &sink,
    );
    assert_eq!(Runner::run(request).await.unwrap().final_text(), "done");

    let recorded = activities(&sink);
    let completions: Vec<&str> = recorded
        .iter()
        .filter(|activity| activity.kind == SubAgentActivityKind::Completed)
        .map(|activity| activity.id.as_str())
        .collect();
    assert_eq!(completions.len(), 2);
    assert_ne!(
        completions[0], completions[1],
        "each run is its own completion"
    );
    let worker = "/root/worker";
    assert_eq!(
        completion_ids_checked(recorded),
        vec![
            activity(
                "run-root",
                "lead",
                "l-1-call",
                worker,
                SubAgentActivityKind::Started
            ),
            activity(
                "run-root",
                "lead",
                "subagent-completed-*",
                worker,
                SubAgentActivityKind::Completed
            ),
            activity(
                "run-root",
                "lead",
                "l-3-call",
                worker,
                SubAgentActivityKind::Interacted
            ),
            activity(
                "run-root",
                "lead",
                "l-4-call",
                worker,
                SubAgentActivityKind::Interacted
            ),
            activity(
                "run-root",
                "lead",
                "subagent-completed-*",
                worker,
                SubAgentActivityKind::Completed
            ),
        ]
    );
    // One sequence for the run, whoever recorded the event: the tools or the tree.
    let seqs: Vec<u64> = sink
        .events()
        .iter()
        .filter(|event| event.run_id().as_str() == "run-root")
        .map(ra_core::event::HostEvent::seq)
        .collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
}

#[tokio::test]
async fn an_interrupt_is_recorded_and_runs_that_do_not_complete_record_no_completion() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    let slow = GateTool::new();
    let tool_started = Arc::clone(&slow.started);

    scripts.push("lead", Step::Respond(spawn_worker("l-1", "index the repo")));
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move { tool_started.notified().await }),
            tool_call(
                "l-2",
                "l-2-call",
                "interrupt_agent",
                json!({"target": "worker"}),
            ),
        ),
    );
    let watch = control.clone();
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move {
                drop(
                    watch
                        .wait_for_status(&worker_path(), |status| {
                            *status == AgentStatus::Interrupted
                        })
                        .await,
                );
            }),
            tool_call(
                "l-3",
                "l-3-call",
                "followup_task",
                json!({"target": "worker", "message": "Try again."}),
            ),
        ),
    );
    scripts.push("lead", Step::Respond(final_message("l-4", "gave up")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "slow_task", json!({}))),
    );
    // The follow-up's run finds no response scripted and fails.

    let request = with_sink(
        root_request(&scripts, &control, worker_agent(vec![Arc::new(slow)])),
        &sink,
    );
    assert_eq!(Runner::run(request).await.unwrap().final_text(), "gave up");
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Errored(_))
    })
    .await;

    let worker = "/root/worker";
    assert_eq!(
        activities(&sink),
        vec![
            activity(
                "run-root",
                "lead",
                "l-1-call",
                worker,
                SubAgentActivityKind::Started
            ),
            activity(
                "run-root",
                "lead",
                "l-2-call",
                worker,
                SubAgentActivityKind::Interrupted
            ),
            activity(
                "run-root",
                "lead",
                "l-3-call",
                worker,
                SubAgentActivityKind::Interacted
            ),
        ]
    );
}

#[tokio::test]
async fn a_run_started_by_another_agent_records_its_completion_on_that_agents_timeline() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    let worker_turn = Arc::new(Notify::new());
    let helper = AgentSpec::builder()
        .id(AgentId::new("helper"))
        .name("Helper")
        .instructions("helper")
        .tools(collaboration())
        .build()
        .unwrap();
    let registry = AgentRegistry::builder()
        .register(worker_agent(Vec::new()))
        .register(helper)
        .build()
        .unwrap();

    scripts.push(
        "lead",
        Step::Respond(spawn_worker("l-1", "draft the notes")),
    );
    scripts.push("lead", wait_releasing("l-2", &worker_turn));
    scripts.push(
        "lead",
        Step::Respond(spawn_call(
            "l-3",
            "helper",
            "helper",
            "Have the worker add a summary.",
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-4", "delegated")));
    scripts.push("worker", final_after(&worker_turn, "w-1", "notes drafted"));
    scripts.push(
        "worker",
        Step::Respond(final_message("w-2", "summary added")),
    );
    scripts.push(
        "helper",
        Step::Respond(tool_call(
            "h-1",
            "h-1-call",
            "followup_task",
            json!({"target": "/root/worker", "message": "Add a summary."}),
        )),
    );
    scripts.push("helper", Step::Respond(final_message("h-2", "asked")));

    let request = RunRequest::new(
        AgentBinding::direct(parent_agent()),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("do the work"))],
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root());
    let request = with_sink(request, &sink);
    assert_eq!(
        Runner::run(request).await.unwrap().final_text(),
        "delegated"
    );
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("summary added".to_owned()))
    })
    .await;
    let helper_path = AgentPath::root().join("helper").unwrap();
    eventually_status(&control, &helper_path, |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    let recorded = completion_ids_checked(activities(&sink));
    let (lead, helper): (Vec<_>, Vec<_>) = recorded
        .into_iter()
        .partition(|activity| activity.agent == "lead");
    let worker = "/root/worker";
    assert_eq!(
        lead,
        vec![
            activity(
                "run-root",
                "lead",
                "l-1-call",
                worker,
                SubAgentActivityKind::Started
            ),
            activity(
                "run-root",
                "lead",
                "subagent-completed-*",
                worker,
                SubAgentActivityKind::Completed
            ),
            activity(
                "run-root",
                "lead",
                "l-3-call",
                "/root/helper",
                SubAgentActivityKind::Started
            ),
            activity(
                "run-root",
                "lead",
                "subagent-completed-*",
                "/root/helper",
                SubAgentActivityKind::Completed
            ),
        ]
    );
    // The worker's second run was asked for by the helper, so the helper's run records it, though
    // the worker's final answer still goes to its parent.
    assert_eq!(helper.len(), 2, "{helper:?}");
    assert!(helper.iter().all(|activity| activity.agent == "helper"
        && activity.run == helper[0].run
        && activity.run != "run-root"
        && activity.path == worker));
    assert_eq!(
        helper
            .iter()
            .map(|activity| (activity.id.as_str(), activity.kind.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("h-1-call", SubAgentActivityKind::Interacted),
            ("subagent-completed-*", SubAgentActivityKind::Completed),
        ]
    );
}

#[tokio::test]
async fn a_run_started_by_follow_ups_from_two_runs_records_no_completion() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    let slow = GateTool::new();
    let tool_started = Arc::clone(&slow.started);
    let worker_turn = Arc::new(Notify::new());
    let worker = worker_agent(vec![Arc::new(slow)]);

    // The first root run spawns the worker and, while the worker sits in a tool call, asks it for
    // more; the follow-up waits in the mailbox for the worker's next model call.
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "index the repo")));
    scripts.push(
        "lead",
        Step::After(
            Box::pin(async move { tool_started.notified().await }),
            tool_call(
                "l-2",
                "l-2-call",
                "followup_task",
                json!({"target": "worker", "message": "List the crates."}),
            ),
        ),
    );
    scripts.push(
        "lead",
        Step::Respond(final_message("l-3", "first run done")),
    );
    // A second root run asks again and interrupts the worker, which then starts a new run on both
    // follow-ups — mail from two different runs.
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-4",
            "l-4-call",
            "followup_task",
            json!({"target": "worker", "message": "List the tests."}),
        )),
    );
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-5",
            "l-5-call",
            "interrupt_agent",
            json!({"target": "worker"}),
        )),
    );
    scripts.push("lead", wait_releasing("l-6", &worker_turn));
    scripts.push(
        "lead",
        Step::Respond(final_message("l-7", "second run done")),
    );
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "slow_task", json!({}))),
    );
    scripts.push(
        "worker",
        final_after(&worker_turn, "w-2", "crates and tests"),
    );

    let first = with_sink(root_request(&scripts, &control, Arc::clone(&worker)), &sink);
    assert_eq!(
        Runner::run(first).await.unwrap().final_text(),
        "first run done"
    );
    let registry = AgentRegistry::builder().register(worker).build().unwrap();
    let second = RunRequest::new(
        AgentBinding::direct(parent_agent()),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root-2"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("keep going"))],
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root());
    let second = with_sink(second, &sink);
    assert_eq!(
        Runner::run(second).await.unwrap().final_text(),
        "second run done"
    );
    assert_eq!(
        control.status(&worker_path()),
        Some(AgentStatus::Completed(Some("crates and tests".to_owned())))
    );
    let last_worker_input = texts(scripts.inputs("worker").last().unwrap());
    assert!(
        last_worker_input
            .iter()
            .any(|text| text.ends_with("List the crates."))
    );
    assert!(
        last_worker_input
            .iter()
            .any(|text| text.ends_with("List the tests."))
    );

    let worker = "/root/worker";
    assert_eq!(
        activities(&sink),
        vec![
            activity(
                "run-root",
                "lead",
                "l-1-call",
                worker,
                SubAgentActivityKind::Started
            ),
            activity(
                "run-root",
                "lead",
                "l-2-call",
                worker,
                SubAgentActivityKind::Interacted
            ),
            activity(
                "run-root-2",
                "lead",
                "l-4-call",
                worker,
                SubAgentActivityKind::Interacted
            ),
            activity(
                "run-root-2",
                "lead",
                "l-5-call",
                worker,
                SubAgentActivityKind::Interrupted
            ),
        ]
    );
}

#[tokio::test]
async fn a_run_paused_for_approval_records_its_completion_only_once_it_completes() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "ship it")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "deploy", json!({}))),
    );
    let request = root_request(
        &scripts,
        &control,
        worker_agent(vec![Arc::new(GuardedTool::new())]),
    );
    Runner::run(with_sink(request, &sink)).await.unwrap();
    let paused = tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .expect("the worker pauses for approval");

    // Waiting for an approval is not the end of the run.
    let worker = "/root/worker";
    let started = activity(
        "run-root",
        "lead",
        "l-1-call",
        worker,
        SubAgentActivityKind::Started,
    );
    assert_eq!(activities(&sink), vec![started.clone()]);

    let mut state = paused.state().clone();
    let run_id = state.run_id().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    scripts.push("worker", Step::Respond(final_message("w-2", "deployed")));
    control.resume(&worker_path(), state).unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    // The run that paused is the run that completed, recorded once on the spawning run.
    assert_eq!(
        activities(&sink),
        vec![
            started,
            activity(
                "run-root",
                "lead",
                &format!("subagent-completed-{run_id}"),
                worker,
                SubAgentActivityKind::Completed,
            ),
        ]
    );
}

/// The sequence numbers of the events recorded for `run`, in the order the sink received them.
fn seqs_of(sink: &InMemoryHostEventSink, run: &str) -> Vec<u64> {
    sink.events()
        .iter()
        .filter(|event| event.run_id().as_str() == run)
        .map(ra_core::event::HostEvent::seq)
        .collect()
}

fn assert_unique_and_rising(seqs: &[u64]) {
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "sequence numbers must be unique and rising: {seqs:?}"
    );
}

/// Continues the root run `state` belongs to, bound to `control`'s root, with `input`.
fn continue_root(
    scripts: &Arc<Scripts>,
    control: &AgentControl,
    state: RunState,
    input: &str,
    sink: &InMemoryHostEventSink,
) -> RunRequest {
    let input = if input.is_empty() {
        Vec::new()
    } else {
        vec![ModelInputItem::Message(Message::user(input))]
    };
    let request = RunRequest::new(
        AgentBinding::direct(parent_agent()),
        Arc::new(ScriptedResolver(Arc::clone(scripts))) as Arc<dyn ModelResolver>,
        state.run_id().clone(),
        CancelScope::root(),
        input,
    )
    .with_state(state)
    .with_config(
        RunConfig::new().with_agent_registry(
            AgentRegistry::builder()
                .register(worker_agent(Vec::new()))
                .build()
                .unwrap(),
        ),
    )
    .with_agent_control(control.root());
    with_sink(request, sink)
}

#[tokio::test]
async fn a_run_continued_from_its_checkpoint_shares_a_sequence_with_a_completion_recorded_after_it_returned()
 {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    let worker_turn = Arc::new(Notify::new());

    scripts.push(
        "lead",
        Step::Respond(spawn_worker("l-1", "count the files")),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", final_after(&worker_turn, "w-1", "42 files"));
    let first = Runner::run(with_sink(
        root_request(&scripts, &control, worker_agent(Vec::new())),
        &sink,
    ))
    .await
    .unwrap();
    // The worker completes after the root run returned; the completion is still the root run's.
    worker_turn.notify_one();
    eventually(
        || {
            activities(&sink)
                .iter()
                .any(|activity| activity.kind == SubAgentActivityKind::Completed)
        },
        "the worker's completion is recorded",
    )
    .await;

    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-3",
            "l-3-call",
            "send_message",
            json!({"target": "worker", "message": "Thanks."}),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-4", "noted")));
    let second = continue_root(&scripts, &control, first.state().clone(), "thank it", &sink);
    assert_eq!(Runner::run(second).await.unwrap().final_text(), "noted");

    assert_eq!(
        completion_ids_checked(activities(&sink))
            .into_iter()
            .map(|activity| (activity.run, activity.kind))
            .collect::<Vec<_>>(),
        vec![
            ("run-root".to_owned(), SubAgentActivityKind::Started),
            ("run-root".to_owned(), SubAgentActivityKind::Completed),
            ("run-root".to_owned(), SubAgentActivityKind::Interacted),
        ]
    );
    assert_unique_and_rising(&seqs_of(&sink, "run-root"));
}

#[tokio::test]
async fn a_completion_recorded_during_a_continued_run_is_covered_by_its_checkpoint() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    let worker_turn = Arc::new(Notify::new());

    scripts.push(
        "lead",
        Step::Respond(spawn_worker("l-1", "count the files")),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", final_after(&worker_turn, "w-1", "42 files"));
    let first = Runner::run(with_sink(
        root_request(&scripts, &control, worker_agent(Vec::new())),
        &sink,
    ))
    .await
    .unwrap();

    // The continued run lets the worker finish and waits for its answer, so the completion is
    // recorded while both the continuation and the earlier segment's origin hold the sequence.
    scripts.push("lead", wait_releasing("l-3", &worker_turn));
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-4",
            "l-4-call",
            "send_message",
            json!({"target": "worker", "message": "Thanks."}),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-5", "noted")));
    let second = Runner::run(continue_root(
        &scripts,
        &control,
        first.state().clone(),
        "wait for it",
        &sink,
    ))
    .await
    .unwrap();
    let seqs = seqs_of(&sink, "run-root");
    assert_eq!(seqs.len(), 3, "started, completed, interacted");
    assert_unique_and_rising(&seqs);

    // Continued once more under a tree that never saw the run, the checkpoint alone has to keep
    // the run clear of every number already issued — including the completion's.
    scripts.push(
        "lead",
        Step::Respond(spawn_call("l-6", "auditor", "worker", "audit")),
    );
    scripts.push("lead", Step::Respond(final_message("l-7", "audited")));
    scripts.push("worker", Step::Respond(final_message("w-2", "clean")));
    let elsewhere = AgentControl::new();
    let third = continue_root(&scripts, &elsewhere, second.state().clone(), "audit", &sink);
    assert_eq!(Runner::run(third).await.unwrap().final_text(), "audited");
    let seqs = seqs_of(&sink, "run-root");
    assert!(seqs.len() > 3);
    assert_unique_and_rising(&seqs);
    elsewhere.shutdown().await;
}

#[tokio::test]
async fn a_nested_agent_tool_run_continued_after_approval_shares_a_sequence_with_its_completions() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    let worker_turn = Arc::new(Notify::new());
    let mut nested_tools = collaboration();
    nested_tools.push(Arc::new(GuardedTool::new()));
    let nested = AgentSpec::builder()
        .id(AgentId::new("nested"))
        .name("Nested")
        .instructions("nested")
        .tools(nested_tools)
        .build()
        .unwrap();
    let lead = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tool(Arc::new(nested.as_tool().build().unwrap()))
        .build()
        .unwrap();
    let config = RunConfig::new().with_agent_registry(
        AgentRegistry::builder()
            .register(worker_agent(Vec::new()))
            .build()
            .unwrap(),
    );
    let request = |run_id: RunId, input: Vec<ModelInputItem>| {
        with_sink(
            RunRequest::new(
                AgentBinding::direct(Arc::clone(&lead)),
                Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
                run_id,
                CancelScope::root(),
                input,
            )
            .with_config(config.clone())
            .with_agent_control(control.root()),
            &sink,
        )
    };

    // The nested run spawns a worker, then stops to ask for approval; the worker completes once
    // the paused run has returned, on the nested run's timeline.
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-1",
            "l-1-call",
            "nested",
            json!({"input": "ship"}),
        )),
    );
    scripts.push(
        "nested",
        Step::Respond(spawn_call("n-1", "worker", "worker", "prepare the release")),
    );
    scripts.push(
        "nested",
        Step::Respond(tool_call("n-2", "n-2-call", "deploy", json!({}))),
    );
    scripts.push("worker", final_after(&worker_turn, "w-1", "prepared"));
    let first = Runner::run(request(
        RunId::new("run-root"),
        vec![ModelInputItem::Message(Message::user("release"))],
    ))
    .await
    .unwrap();
    assert!(!first.outcome().interruptions().is_empty());
    worker_turn.notify_one();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let nested_run = activities(&sink)
        .into_iter()
        .find(|activity| activity.kind == SubAgentActivityKind::Completed)
        .expect("the completion is recorded")
        .run;
    assert_ne!(nested_run, "run-root");

    // Approved and continued, the nested run messages the worker from its new segment.
    let mut state = first.state().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    scripts.push(
        "nested",
        Step::Respond(tool_call(
            "n-3",
            "n-3-call",
            "send_message",
            json!({"target": "worker", "message": "Deployed."}),
        )),
    );
    scripts.push("nested", Step::Respond(final_message("n-4", "shipped")));
    scripts.push("lead", Step::Respond(final_message("l-2", "released")));
    let second = Runner::run(request(RunId::new("run-root"), Vec::new()).with_state(state))
        .await
        .unwrap();
    assert_eq!(second.final_text(), "released");

    assert_eq!(
        activities(&sink)
            .into_iter()
            .filter(|activity| activity.run == nested_run)
            .map(|activity| activity.kind)
            .collect::<Vec<_>>(),
        vec![
            SubAgentActivityKind::Started,
            SubAgentActivityKind::Completed,
            SubAgentActivityKind::Interacted,
        ]
    );
    assert_unique_and_rising(&seqs_of(&sink, &nested_run));
}

#[tokio::test]
async fn a_continuation_resumed_past_a_persisted_log_raises_the_shared_sequence() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let sink = InMemoryHostEventSink::new();
    let worker_turn = Arc::new(Notify::new());

    scripts.push(
        "lead",
        Step::Respond(spawn_worker("l-1", "count the files")),
    );
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push("worker", final_after(&worker_turn, "w-1", "42 files"));
    let first = Runner::run(with_sink(
        root_request(&scripts, &control, worker_agent(Vec::new())),
        &sink,
    ))
    .await
    .unwrap();
    worker_turn.notify_one();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;

    // The host's log says the run already wrote number 40; the continuation and the sequence the
    // tree holds for the run both have to move past it.
    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-3",
            "l-3-call",
            "send_message",
            json!({"target": "worker", "message": "Thanks."}),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-4", "noted")));
    let second = RunRequest::new(
        AgentBinding::direct(parent_agent()),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("thank it"))],
    )
    .with_state_and_persisted_max_seq(first.state().clone(), Some(40))
    .with_agent_control(control.root());
    assert_eq!(
        Runner::run(with_sink(second, &sink))
            .await
            .unwrap()
            .final_text(),
        "noted"
    );
    assert_eq!(seqs_of(&sink, "run-root"), vec![0, 1, 41]);
}

struct GatedFactory {
    inner: PreparingFactory,
    entered: Notify,
    proceed: Notify,
}

#[async_trait]
impl SpawnedAgentFactory for GatedFactory {
    async fn prepare(&self, request: SpawnedAgentRequest<'_>) -> Result<PreparedSpawnedAgent> {
        self.entered.notify_one();
        self.proceed.notified().await;
        self.inner.prepare(request).await
    }
}

#[tokio::test]
async fn shutdown_during_preparation_refuses_spawn_and_releases_resources() {
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(GatedFactory {
        inner: PreparingFactory::new(prepared_worker(), false),
        entered: Notify::new(),
        proceed: Notify::new(),
    });
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("r-parent-1", "work")));
    scripts.push(
        "lead",
        Step::Respond(final_message("r-parent-2", "finished")),
    );
    let request = root_request(&scripts, &control, worker_agent(Vec::new()));
    let run = tokio::spawn(Runner::run(request));
    tokio::time::timeout(Duration::from_secs(5), factory.entered.notified())
        .await
        .unwrap();
    control.shutdown().await;
    factory.proceed.notify_one();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let remaining = paths(&control);
    let releases = factory.inner.released.load(Ordering::SeqCst);
    control.shutdown().await;
    assert_eq!(
        remaining,
        ["/root"],
        "late spawn was admitted after shutdown, releases={releases}"
    );
    assert_eq!(
        releases, 1,
        "prepared resources were not released on refusal"
    );
}

struct BlockingRelease {
    entered: Notify,
    proceed: Notify,
    released: AtomicBool,
}

#[async_trait]
impl SpawnedAgentResources for BlockingRelease {
    async fn shutdown(&self) {
        self.entered.notify_one();
        self.proceed.notified().await;
        self.released.store(true, Ordering::SeqCst);
    }
}
struct BlockingReleaseFactory(Arc<BlockingRelease>);
#[async_trait]
impl SpawnedAgentFactory for BlockingReleaseFactory {
    async fn prepare(&self, _: SpawnedAgentRequest<'_>) -> Result<PreparedSpawnedAgent> {
        Ok(PreparedSpawnedAgent::new(prepared_worker()).with_resources(self.0.clone()))
    }
}

#[tokio::test]
async fn a_cancelled_shutdown_waiter_does_not_abandon_resource_cleanup() {
    let scripts = Arc::new(Scripts::default());
    let resource = Arc::new(BlockingRelease {
        entered: Notify::new(),
        proceed: Notify::new(),
        released: AtomicBool::new(false),
    });
    let control = AgentControl::new()
        .with_spawned_agent_factory(Arc::new(BlockingReleaseFactory(resource.clone())))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("r-parent-1", "work")));
    scripts.push(
        "lead",
        Step::Respond(final_message("r-parent-2", "finished")),
    );
    scripts.push(
        "prepared",
        Step::Respond(final_message("r-child-1", "finished")),
    );
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let closing = tokio::spawn({
        let control = control.clone();
        async move { control.shutdown().await }
    });
    tokio::time::timeout(Duration::from_secs(5), resource.entered.notified())
        .await
        .unwrap();
    closing.abort();
    let _ = closing.await;
    resource.proceed.notify_one();
    tokio::time::timeout(Duration::from_secs(5), control.shutdown())
        .await
        .unwrap();
    let released = resource.released.load(Ordering::SeqCst);
    if !released {
        resource.shutdown().await;
    }
    assert!(
        released,
        "shutdown returned while the resource cleanup was permanently skipped"
    );
}

#[tokio::test]
async fn every_shutdown_waiter_waits_for_resource_cleanup() {
    let scripts = Arc::new(Scripts::default());
    let resource = Arc::new(BlockingRelease {
        entered: Notify::new(),
        proceed: Notify::new(),
        released: AtomicBool::new(false),
    });
    let control = AgentControl::new()
        .with_spawned_agent_factory(Arc::new(BlockingReleaseFactory(resource.clone())))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("r-parent-1", "work")));
    scripts.push(
        "lead",
        Step::Respond(final_message("r-parent-2", "finished")),
    );
    scripts.push(
        "prepared",
        Step::Respond(final_message("r-child-1", "finished")),
    );
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let first = tokio::spawn({
        let control = control.clone();
        async move { control.shutdown().await }
    });
    tokio::time::timeout(Duration::from_secs(5), resource.entered.notified())
        .await
        .unwrap();
    let second_returned_early = tokio::time::timeout(Duration::from_millis(50), control.shutdown())
        .await
        .is_ok();
    let was_released = resource.released.load(Ordering::SeqCst);
    resource.proceed.notify_one();
    first.await.unwrap();
    assert!(
        !second_returned_early || was_released,
        "second shutdown returned before the first cleanup completed"
    );
}

#[tokio::test]
async fn children_and_grandchildren_inherit_the_root_cache_override() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push("lead", Step::Respond(spawn_worker("l1", "go")));
    scripts.push("lead", Step::Respond(final_message("l2", "started")));
    scripts.push(
        "worker",
        Step::Respond(spawn_call("w1", "helper", "helper", "help")),
    );
    scripts.push(
        "worker",
        Step::Respond(final_message("w2", "started helper")),
    );
    scripts.push("helper", Step::Respond(final_message("h1", "done")));
    let registry = AgentRegistry::builder()
        .register(worker_agent(vec![]))
        .register(helper_agent(vec![]))
        .build()
        .unwrap();
    let request = RunRequest::new(
        AgentBinding::direct(parent_agent()),
        Arc::new(ScriptedResolver(scripts.clone())),
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("go"))],
    )
    .with_config(
        RunConfig::new()
            .with_agent_registry(registry)
            .with_prompt_cache_key_override("root-thread"),
    )
    .with_agent_control(control.root());

    let result = Runner::run(request).await.unwrap();
    assert_eq!(result.state().generated_prompt_cache_key(), None);
    eventually_status(&control, &helper_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let keys = scripts.cache_keys.lock().unwrap();
    for name in ["lead", "worker", "helper"] {
        let found: Vec<_> = keys.iter().filter(|(agent, _)| agent == name).collect();
        assert!(!found.is_empty(), "{name} must have run");
        assert!(
            found
                .iter()
                .all(|(_, key)| key.as_deref() == Some("root-thread"))
        );
    }
}

#[test]
fn skill_context_is_retained_without_becoming_a_fork_turn_boundary() {
    let skill = ModelInputItem::Message(Message::user("<skill>private instructions</skill>"));
    let history = vec![
        ModelInputItem::Message(Message::user("first")),
        ModelInputItem::Message(Message::assistant("first answer", OutputPhase::Final)),
        ModelInputItem::Message(Message::user("second")),
        skill.clone(),
        ModelInputItem::Message(Message::assistant("second answer", OutputPhase::Final)),
    ];
    assert_eq!(
        fork_history(&history, SpawnAgentForkMode::LastNTurns(1)),
        history[2..]
    );
    assert_eq!(
        fork_history(&history, SpawnAgentForkMode::FullHistory),
        history
    );
}

// ---------------------------------------------------------------------------------------------
// Spawn reservations
// ---------------------------------------------------------------------------------------------

/// A store whose rollout creation waits until the test lets it go on.
struct GatedStore {
    inner: Arc<dyn ra_core::session::rollout::RolloutThreadStore>,
    entered: Notify,
    proceed: Notify,
}

impl GatedStore {
    fn new(inner: Arc<dyn ra_core::session::rollout::RolloutThreadStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            entered: Notify::new(),
            proceed: Notify::new(),
        })
    }
}

#[async_trait]
impl ra_core::session::rollout::RolloutThreadStore for GatedStore {
    async fn create_thread(
        &self,
        session_id: &ra_core::session::SessionId,
        spawn: &ra_core::session::rollout::RolloutThreadSpawn,
    ) -> Result<Arc<dyn ra_core::session::rollout::RolloutRecorder>> {
        self.entered.notify_one();
        self.proceed.notified().await;
        self.inner.create_thread(session_id, spawn).await
    }
}

async fn until(what: &str, done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

/// As Codex's `SpawnReservation` and `PendingSpawn`: a spawn dropped after the factory handed over
/// the agent's resources, while its rollout is still being created, releases them once and frees
/// its path, though nobody waits for it.
#[tokio::test]
async fn a_spawn_cancelled_while_creating_its_rollout_releases_what_it_was_given() {
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), false));
    let store = GatedStore::new(Arc::new(ra_session::InMemoryThreadStore::new()));
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("r-parent-1", "work")));
    let run = tokio::spawn(Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(Vec::new()),
    )));
    tokio::time::timeout(Duration::from_secs(5), store.entered.notified())
        .await
        .unwrap();
    assert_eq!(control.live_instances().len(), 1, "the path is reserved");
    run.abort();
    let _ = run.await;
    until("the cancelled spawn released its resources", || {
        factory.released.load(Ordering::SeqCst) == 1
    })
    .await;
    until("the reservation was freed", || {
        control.live_instances().is_empty()
    })
    .await;
    store.proceed.notify_one();
    assert_eq!(paths(&control), ["/root"]);
    assert_eq!(factory.released.load(Ordering::SeqCst), 1);
}

/// A close that reaches a spawn still preparing its agent freezes it: the agent is admitted
/// closed, stopped by that close — its rollout written out — and the spawn is refused.
#[tokio::test]
async fn a_close_reaching_a_preparing_spawn_admits_and_stops_its_agent() {
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(GatedFactory {
        inner: PreparingFactory::new(prepared_worker(), false),
        entered: Notify::new(),
        proceed: Notify::new(),
    });
    let store = Arc::new(ra_session::InMemoryThreadStore::new());
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("r-parent-1", "work")));
    scripts.push(
        "lead",
        Step::Respond(final_message("r-parent-2", "finished")),
    );
    let run = tokio::spawn(Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(Vec::new()),
    )));
    tokio::time::timeout(Duration::from_secs(5), factory.entered.notified())
        .await
        .unwrap();
    let frozen = control.live_instances();
    assert_eq!(frozen.len(), 1);
    let closing = {
        let control = control.clone();
        tokio::spawn(async move { control.close(&AgentPath::root()).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    factory.proceed.notify_one();
    closing.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(paths(&control), ["/root"]);
    assert_eq!(factory.inner.released.load(Ordering::SeqCst), 1);
    let session = frozen[0].session_id();
    assert!(
        store.thread_metadata(session).is_some(),
        "the agent's rollout was materialized when it was stopped"
    );
}

/// A spawn refused after its rollout was created, whose rollout then cannot be discarded, keeps its
/// path and holds the rollout; closing its session again discards it and frees the path.
#[tokio::test]
async fn a_spawn_whose_rollout_cannot_be_discarded_holds_its_path_until_closed_again() {
    use ra_session::rollout::faults::{RolloutFaultPoint, RolloutFaults};
    let directory = tempfile::tempdir().unwrap();
    let faults = RolloutFaults::new();
    let store = GatedStore::new(Arc::new(
        ra_session::RolloutThreadDirectory::new(directory.path()).with_faults(Arc::clone(&faults)),
    ));
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), false));
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("r-parent-1", "work")));
    scripts.push(
        "lead",
        Step::Respond(final_message("r-parent-2", "finished")),
    );
    let run = tokio::spawn(Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(Vec::new()),
    )));
    tokio::time::timeout(Duration::from_secs(5), store.entered.notified())
        .await
        .unwrap();
    let session = control.live_instances()[0].session_id().clone();
    // Shutting down refuses the agent once its rollout exists; its discard fails once.
    faults.fail(RolloutFaultPoint::Discard, 1);
    control.shutdown().await;
    store.proceed.notify_one();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    until("the spawn tried to discard its rollout", || {
        faults.calls(RolloutFaultPoint::Discard) == 1
    })
    .await;
    assert_eq!(factory.released.load(Ordering::SeqCst), 1);
    assert_eq!(
        control
            .live_instances()
            .iter()
            .map(|agent| agent.session_id().clone())
            .collect::<Vec<_>>(),
        std::slice::from_ref(&session),
        "the reservation still holds the rollout"
    );
    let report = control.close_session(&session).unwrap().finished().await;
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(faults.calls(RolloutFaultPoint::Discard), 2);
    assert!(control.live_instances().is_empty());
    assert_eq!(factory.released.load(Ordering::SeqCst), 1);
}

/// Removal waits for the spawns a shutdown did not wait for: a spawn still creating its rollout
/// when the tree shut down settles later, and a cleanup that cannot discard its rollout is
/// reported, then retried by the next settle.
#[tokio::test]
async fn settling_spawns_reports_a_cleanup_that_failed_after_shutdown() {
    use ra_session::rollout::faults::{RolloutFaultPoint, RolloutFaults};
    let directory = tempfile::tempdir().unwrap();
    let faults = RolloutFaults::new();
    let store = GatedStore::new(Arc::new(
        ra_session::RolloutThreadDirectory::new(directory.path()).with_faults(Arc::clone(&faults)),
    ));
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), false));
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("r-parent-1", "work")));
    scripts.push(
        "lead",
        Step::Respond(final_message("r-parent-2", "finished")),
    );
    let run = tokio::spawn(Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(Vec::new()),
    )));
    tokio::time::timeout(Duration::from_secs(5), store.entered.notified())
        .await
        .unwrap();
    faults.fail(RolloutFaultPoint::Discard, 1);
    let shutdown = control.shutdown_close().finished().await;
    assert!(shutdown.is_complete(), "the shutdown does not wait for it");
    let settling = {
        let control = control.clone();
        tokio::spawn(async move { control.settle_spawns().await })
    };
    store.proceed.notify_one();
    let first = settling.await.unwrap();
    assert_eq!(first.failed().len(), 1, "{first:?}");
    let _ = run.await;
    let second = control.settle_spawns().await;
    assert!(second.is_complete(), "{second:?}");
    assert!(control.live_instances().is_empty());
    assert_eq!(faults.calls(RolloutFaultPoint::Discard), 2);
}

/// A close by an agent's session reaches only that instance: once it has gone and the path was
/// spawned again, closing the old session closes nothing, and the new agent goes on.
#[tokio::test]
async fn closing_an_old_session_leaves_the_agent_spawned_again_at_its_path() {
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), false));
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap()
        .with_root_session_id(ra_core::session::SessionId::new("root"))
        .unwrap();
    for round in ["1", "2"] {
        scripts.push(
            "lead",
            Step::Respond(spawn_worker(&format!("l-{round}"), "work")),
        );
        scripts.push(
            "lead",
            Step::Respond(final_message(&format!("l-{round}-end"), "spawned")),
        );
        scripts.push(
            "prepared",
            Step::Respond(final_message(&format!("p-{round}"), "prepared done")),
        );
    }
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let old = control.live_instances()[0].session_id().clone();
    control.close(&worker_path()).await.unwrap();
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let new = control.live_instances()[0].session_id().clone();
    assert_ne!(new, old);

    assert!(control.close_session(&old).is_none());
    assert_eq!(paths(&control), ["/root", "/root/worker"]);
    assert_eq!(
        control
            .live_instances()
            .iter()
            .map(|agent| agent.session_id().clone())
            .collect::<Vec<_>>(),
        [new]
    );
}

/// Context injected into an idle spawned agent — a network rule it saved, for instance — reaches
/// the input of its next run, as Codex's `inject_no_new_turn` adds it to the session.
#[tokio::test]
async fn context_injected_into_an_idle_agent_reaches_its_next_run() {
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), false));
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap()
        .with_root_session_id(ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "work")));
    scripts.push("lead", Step::Respond(final_message("l-1-end", "spawned")));
    scripts.push(
        "prepared",
        Step::Respond(final_message("p-1", "prepared done")),
    );
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let session = control.live_instances()[0].session_id().clone();
    assert!(control.inject_context(&session, vec![Message::developer("network rule saved")]));

    scripts.push(
        "lead",
        Step::Respond(tool_call(
            "l-2",
            "l-2-call",
            "followup_task",
            json!({"target": "worker", "message": "again"}),
        )),
    );
    scripts.push("lead", Step::Respond(final_message("l-2-end", "asked")));
    scripts.push(
        "prepared",
        Step::Respond(final_message("p-2", "prepared again")),
    );
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    until("the worker ran again", || {
        scripts.inputs("prepared").len() == 2
    })
    .await;
    let input = texts(&scripts.inputs("prepared")[1]);
    assert!(
        input.iter().any(|text| text == "network rule saved"),
        "{input:?}"
    );
}

/// Closes that meet a spawn's cleanup retry in progress share its outcome: none of them reports the
/// path free while the rollout is still held, even when the retry fails again.
#[tokio::test]
async fn concurrent_closes_share_a_spawns_cleanup_retry() {
    use ra_session::rollout::faults::{RolloutFaultPoint, RolloutFaults};
    let directory = tempfile::tempdir().unwrap();
    let faults = RolloutFaults::new();
    let store = GatedStore::new(Arc::new(
        ra_session::RolloutThreadDirectory::new(directory.path()).with_faults(Arc::clone(&faults)),
    ));
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), false));
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("r-parent-1", "work")));
    scripts.push(
        "lead",
        Step::Respond(final_message("r-parent-2", "finished")),
    );
    let run = tokio::spawn(Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(Vec::new()),
    )));
    tokio::time::timeout(Duration::from_secs(5), store.entered.notified())
        .await
        .unwrap();
    let session = control.live_instances()[0].session_id().clone();
    faults.fail(RolloutFaultPoint::Discard, 1);
    control.shutdown().await;
    store.proceed.notify_one();
    let _ = run.await;
    until("the first cleanup failed", || {
        faults.calls(RolloutFaultPoint::Discard) == 1
    })
    .await;

    // The retry is held and fails once more; a second close arrives while it is held.
    faults.fail(RolloutFaultPoint::Discard, 1);
    let hold = faults.hold(RolloutFaultPoint::Discard);
    let first = control.close_session(&session).unwrap();
    until("the retry reached the writer", || {
        faults.calls(RolloutFaultPoint::Discard) == 2
    })
    .await;
    let second = control.close_session(&session).unwrap();
    let second = tokio::spawn(second.finished());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !second.is_finished(),
        "the second close waits for the retry"
    );
    hold.release();
    assert!(!first.finished().await.is_complete());
    assert!(!second.await.unwrap().is_complete());
    assert_eq!(control.live_instances().len(), 1, "the path is still held");
    assert_eq!(faults.calls(RolloutFaultPoint::Discard), 2);
}

/// Context injected into an idle spawned agent is recorded into its rollout at once, as Codex
/// records into an idle session, so it is there even if the agent never runs again.
#[tokio::test]
async fn context_injected_into_an_idle_agent_is_recorded_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(directory.path()));
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(PreparingFactory::new(prepared_worker(), false));
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "work")));
    scripts.push("lead", Step::Respond(final_message("l-1-end", "spawned")));
    scripts.push(
        "prepared",
        Step::Respond(final_message("p-1", "prepared done")),
    );
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let session = control.live_instances()[0].session_id().clone();
    assert!(control.inject_context(&session, vec![Message::developer("saved network rule")]));
    let path = store.rollout_path(&session).unwrap();
    until("the injected context reached the rollout", || {
        std::fs::read_to_string(&path).is_ok_and(|text| text.contains("saved network rule"))
    })
    .await;
    control.shutdown().await;
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("saved network rule")
    );
}

fn worker_compaction_capability() -> ra_context::compaction::CompactionCapability {
    ra_context::compaction::CompactionCapability::default().with_context_windows(
        ra_context::window::ContextWindowConfig::new(
            std::collections::BTreeMap::from([("canonical-model".to_owned(), 1_000)]),
            ra_context::window::DEFAULT_COMPACTION_THRESHOLD_RATIO,
        )
        .unwrap(),
    )
}

fn worker_compaction_config(worker: Arc<AgentSpec>) -> RunConfig {
    let registry = AgentRegistry::builder().register(worker).build().unwrap();
    RunConfig::new()
        .with_agent_registry(registry)
        .with_capability(Arc::new(worker_compaction_capability()))
}

async fn spawn_compacting_worker(
    scripts: &Arc<Scripts>,
    control: &AgentControl,
    worker: Arc<AgentSpec>,
    hooks: UserHooks,
) {
    let config = worker_compaction_config(worker.clone()).with_user_hooks(hooks);
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "first task")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    Runner::run(root_request(scripts, control, worker).with_config(config))
        .await
        .unwrap();
}

#[derive(Default)]
struct WorkerCompactionHooks {
    stop_at: Option<HookEventName>,
    events: Mutex<Vec<HookEventName>>,
}

impl WorkerCompactionHooks {
    fn hooks(self: &Arc<Self>) -> UserHooks {
        [
            HookEventName::PreCompact,
            HookEventName::PostCompact,
            HookEventName::SessionStart,
        ]
        .into_iter()
        .fold(
            UserHooks::default().with_session_id(ra_core::session::SessionId::new("worker")),
            |hooks, event| hooks.with_hook(UserHookRegistration::new(event, self.clone())),
        )
    }
}

#[async_trait]
impl UserHook for WorkerCompactionHooks {
    fn name(&self) -> &str {
        "observe worker compaction"
    }

    async fn call(
        &self,
        context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        if context.run().agent_id() != &AgentId::new("worker") {
            return Ok(HookDecision::Continue);
        }
        if matches!(event, HookEvent::SessionStart { source, .. } if *source != SessionStartSource::Compact)
        {
            return Ok(HookDecision::Continue);
        }
        self.events.lock().unwrap().push(event.name());
        if self.stop_at == Some(event.name()) {
            return Ok(HookDecision::Deny {
                message: "stop compaction turn".into(),
            });
        }
        Ok(HookDecision::Continue)
    }
}

fn worker_summary() -> ModelResponse {
    final_message(
        "summary",
        r#"{"primary_request_and_intent":"intent","key_technical_concepts":"concepts","files_and_code_sections":"files","errors_and_fixes":"errors","problem_solving":"solving","pending_tasks":"tasks","current_work":"worker summary","optional_next_step":"next"}"#,
    )
    .with_usage(Usage::from_request(RequestUsage::new(3_000, 80)))
}

fn summaries(input: &[ModelInputItem]) -> Vec<&str> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Compaction(compaction) => Some(compaction.summary()),
            _ => None,
        })
        .collect()
}

async fn follow_up_worker(control: &AgentControl, message: &str) {
    control
        .root()
        .send(
            "worker",
            message.to_owned(),
            MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn spawned_agent_compacts_its_session_before_recording_follow_up_mail() {
    use ra_session::rollout::{RolloutPayload, RolloutReader, reconstruct_history};

    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(directory.path()));
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push(
        "worker",
        Step::Respond(
            final_message("w-1", "previous answer")
                .with_usage(Usage::from_request(RequestUsage::new(1_100, 5))),
        ),
    );
    spawn_compacting_worker(
        &scripts,
        &control,
        worker_agent(Vec::new()),
        UserHooks::default(),
    )
    .await;
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("previous answer".into()))
    })
    .await;

    scripts.push("worker", Step::Respond(worker_summary()));
    scripts.push(
        "worker",
        Step::Respond(final_message("w-2", "follow-up answer")),
    );
    follow_up_worker(&control, "second task").await;
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("follow-up answer".into()))
    })
    .await;
    let inputs = scripts.inputs("worker");
    assert_eq!(inputs.len(), 3);
    let summary_input = texts(&inputs[1]).join("\n");
    assert!(summary_input.contains("previous answer"));
    assert!(!summary_input.contains("second task"));
    let sampling_input = texts(&inputs[2]).join("\n");
    assert!(summaries(&inputs[2])[0].contains("worker summary"));
    assert!(sampling_input.contains("second task"));
    assert!(!sampling_input.contains("previous answer"));

    let session = control.live_instances()[0].session_id().clone();
    let path = store.rollout_path(&session).unwrap();
    control.shutdown().await;
    let records = RolloutReader::open(path).read_all().await.unwrap();
    let payloads: Vec<_> = records
        .iter()
        .map(|record| record.payload().unwrap())
        .collect();
    let (index, compacted) = payloads
        .iter()
        .enumerate()
        .find_map(|(index, payload)| match payload {
            RolloutPayload::Compacted(compacted) => Some((index, compacted)),
            _ => None,
        })
        .expect("the child owns a persisted replacement");
    assert!(payloads[..index].iter().any(|payload| matches!(payload,
        RolloutPayload::RunStarted(start) if start.run_id() == compacted.run_id()
    )));
    assert!(payloads[index + 1..].iter().any(|payload| matches!(payload,
        RolloutPayload::RunStarted(start) if start.run_id() == compacted.run_id()
            && texts(start.input()).join("\n").contains("second task")
    )));
    let reconstructed = reconstruct_history(&records).unwrap();
    let recovered = texts(reconstructed.history()).join("\n");
    assert_eq!(summaries(reconstructed.history()), summaries(&inputs[2]));
    assert!(!recovered.contains("previous answer"));
    assert_eq!(recovered.matches("second task").count(), 1);
    assert!(recovered.contains("follow-up answer"));
    assert_eq!(reconstructed.reported_tokens(), None);
}

#[tokio::test]
async fn spawned_agent_keeps_a_mid_turn_replacement_when_its_next_sampling_is_interrupted() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push(
        "worker",
        Step::Respond(
            tool_call("w-1", "w-1-call", "deploy", json!({}))
                .with_usage(Usage::from_request(RequestUsage::new(1_100, 5))),
        ),
    );
    let hooks = Arc::new(WorkerCompactionHooks::default());
    spawn_compacting_worker(
        &scripts,
        &control,
        worker_agent(vec![Arc::new(GuardedTool::new())]),
        hooks.hooks(),
    )
    .await;
    let paused = tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .expect("the worker pauses for approval");
    let mut state = paused.state().clone();
    let pending: Vec<_> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    scripts.push("worker", Step::Respond(worker_summary()));
    scripts.push("worker", Step::Hang);
    control.resume(&worker_path(), state).unwrap();
    until("the worker sampled from its replacement", || {
        scripts.inputs("worker").len() == 3
    })
    .await;
    let inputs = scripts.inputs("worker");
    assert!(summaries(&inputs[2])[0].contains("worker summary"));
    assert_eq!(
        *hooks.events.lock().unwrap(),
        [HookEventName::PreCompact, HookEventName::PostCompact],
    );
    control.root().interrupt("worker").await.unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Interrupted
    })
    .await;
    scripts.push("worker", Step::Respond(final_message("w-done", "finished")));
    follow_up_worker(&control, "continue after interrupt").await;
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("finished".into()))
    })
    .await;
    let next = &scripts.inputs("worker")[3];
    assert_eq!(summaries(next), summaries(&inputs[2]));
    assert!(texts(next).join("\n").contains("continue after interrupt"));
    assert!(!next.iter().any(|item| {
        matches!(
            item,
            ModelInputItem::ToolCall(_) | ModelInputItem::ToolCallOutput(_)
        )
    }));
}

#[rstest::rstest]
#[case::pre(HookEventName::PreCompact, &[HookEventName::PreCompact], false)]
#[case::post(HookEventName::PostCompact, &[HookEventName::PreCompact, HookEventName::PostCompact], true)]
#[tokio::test]
async fn spawned_agent_compaction_hook_deny_cancels_and_preserves_follow_up_mail(
    #[case] stop_at: HookEventName,
    #[case] expected_hooks: &[HookEventName],
    #[case] committed: bool,
) {
    use ra_core::session::rollout::RolloutRunEnd;
    use ra_session::rollout::{RolloutPayload, RolloutReader, reconstruct_history};

    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(directory.path()));
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    let hooks = Arc::new(WorkerCompactionHooks {
        stop_at: Some(stop_at),
        ..WorkerCompactionHooks::default()
    });
    scripts.push(
        "worker",
        Step::Respond(
            final_message("w-1", "before stop")
                .with_usage(Usage::from_request(RequestUsage::new(1_100, 5))),
        ),
    );
    spawn_compacting_worker(&scripts, &control, worker_agent(Vec::new()), hooks.hooks()).await;
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("before stop".into()))
    })
    .await;

    if committed {
        scripts.push("worker", Step::Respond(worker_summary()));
    }
    let preserved = "mail preserved after a compaction hook stops";
    follow_up_worker(&control, preserved).await;
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Interrupted
    })
    .await;
    assert_eq!(scripts.inputs("worker").len(), 1 + usize::from(committed));
    assert_eq!(*hooks.events.lock().unwrap(), expected_hooks);

    let session = control.live_instances()[0].session_id().clone();
    let path = store.rollout_path(&session).unwrap();
    control.shutdown().await;
    let records = RolloutReader::open(path).read_all().await.unwrap();
    let payloads: Vec<_> = records
        .iter()
        .map(|record| record.payload().unwrap())
        .collect();
    let run_id = payloads
        .iter()
        .find_map(|payload| match payload {
            RolloutPayload::RunStarted(start)
                if texts(start.input())
                    .iter()
                    .any(|text| text.contains(preserved)) =>
            {
                Some(start.run_id())
            }
            _ => None,
        })
        .expect("the cancelled run records its mail");
    assert!(payloads.iter().any(|payload| matches!(payload,
        RolloutPayload::RunEnded(end) if end.run_id() == run_id && end.end() == RolloutRunEnd::Cancelled
    )));
    assert_eq!(
        payloads
            .iter()
            .any(|payload| matches!(payload, RolloutPayload::Compacted(_))),
        committed,
    );
    let recovered = reconstruct_history(&records).unwrap();
    assert!(
        texts(recovered.history())
            .iter()
            .any(|text| text.contains(preserved))
    );
}

struct WorkerCompactionFactory;

#[async_trait]
impl SpawnedAgentFactory for WorkerCompactionFactory {
    async fn prepare(&self, request: SpawnedAgentRequest<'_>) -> Result<PreparedSpawnedAgent> {
        Ok(
            PreparedSpawnedAgent::new(Arc::clone(request.agent())).with_prepared_capabilities(
                [Arc::new(worker_compaction_capability())
                    as Arc<dyn ra_core::capability::Capability>],
                [],
            ),
        )
    }
}

#[rstest::rstest]
#[case::inherited(false)]
#[case::spawned_factory(true)]
#[tokio::test]
async fn full_fork_inherits_the_parents_latest_sampling_total_for_its_first_compaction(
    #[case] factory_only: bool,
) {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let control = if factory_only {
        control
            .with_spawned_agent_factory(Arc::new(WorkerCompactionFactory))
            .unwrap()
    } else {
        control
    };
    let worker = worker_agent(Vec::new());
    scripts.push(
        "lead",
        Step::Respond(
            ModelResponse::new(vec![spawn_with_fork("l-spawn", "worker", Some("all"))])
                .with_usage(Usage::from_request(RequestUsage::new(1_100, 5))),
        ),
    );
    scripts.push("lead", Step::Respond(final_message("l-done", "spawned")));
    scripts.push("worker", Step::Respond(worker_summary()));
    scripts.push(
        "worker",
        Step::Respond(final_message("w-1", "after fork compaction")),
    );
    let request = root_request(&scripts, &control, worker.clone());
    let request = if factory_only {
        request
    } else {
        request.with_config(worker_compaction_config(worker))
    };
    Runner::run(request).await.unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("after fork compaction".into()))
    })
    .await;

    let inputs = scripts.inputs("worker");
    assert_eq!(inputs.len(), 2);
    assert_eq!(scripts.inputs("lead").len(), 2);
    let summary = texts(&inputs[0]).join("\n");
    assert!(summary.contains("do the work"));
    assert!(!summary.contains(NEW_TASK));
    assert!(summaries(&inputs[1])[0].contains("worker summary"));
    assert!(texts(&inputs[1]).join("\n").contains(NEW_TASK));
}

#[tokio::test]
async fn failed_spawned_compaction_keeps_follow_up_mail_for_the_next_turn() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    scripts.push(
        "worker",
        Step::Respond(
            final_message("w-1", "previous answer")
                .with_usage(Usage::from_request(RequestUsage::new(1_100, 5))),
        ),
    );
    spawn_compacting_worker(
        &scripts,
        &control,
        worker_agent(Vec::new()),
        UserHooks::default(),
    )
    .await;
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("previous answer".into()))
    })
    .await;

    scripts.push(
        "worker",
        Step::Respond(final_message("bad-summary", "invalid JSON")),
    );
    follow_up_worker(&control, "keep this mail").await;
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Errored(_))
    })
    .await;
    assert_eq!(scripts.inputs("worker").len(), 2);

    scripts.push("worker", Step::Respond(worker_summary()));
    scripts.push("worker", Step::Respond(final_message("w-2", "recovered")));
    follow_up_worker(&control, "try again").await;
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("recovered".into()))
    })
    .await;
    let calls = scripts.inputs("worker");
    assert_eq!(calls.len(), 4);
    let prior = texts(&calls[2]).join("\n");
    assert!(prior.contains("previous answer"));
    assert!(prior.contains("keep this mail"));
    assert!(!prior.contains("try again"));
    assert_eq!(summaries(&calls[3]).len(), 1);
}

#[tokio::test]
async fn spawned_agent_mid_turn_compaction_leaves_new_mail_for_sampling() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let gate = Arc::new(GateTool::new());
    scripts.push(
        "worker",
        Step::Respond(
            tool_call("w-1", "w-1-call", "slow_task", json!({}))
                .with_usage(Usage::from_request(RequestUsage::new(1_100, 5))),
        ),
    );
    scripts.push("worker", Step::Respond(worker_summary()));
    scripts.push("worker", Step::Respond(final_message("w-2", "done")));
    spawn_compacting_worker(
        &scripts,
        &control,
        worker_agent(vec![gate.clone()]),
        UserHooks::default(),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), gate.started.notified())
        .await
        .expect("the worker is blocked in its tool");
    let mail = "Read this message verbatim.";
    control
        .root()
        .send("worker", mail.into(), MessageDeliveryMode::QueueOnly)
        .await
        .unwrap();
    gate.release.notify_one();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("done".into()))
    })
    .await;

    let inputs = scripts.inputs("worker");
    assert_eq!(inputs.len(), 3);
    assert!(!texts(&inputs[1]).join("\n").contains(mail));
    assert_eq!(summaries(&inputs[2]).len(), 1);
    assert_eq!(texts(&inputs[2]).join("\n").matches(mail).count(), 1);
}

#[rstest::rstest]
#[case::deadline(true)]
#[case::shared_tokens(false)]
#[tokio::test]
async fn spawned_agent_pre_turn_compaction_exhaustion_stops_before_sampling(
    #[case] deadline: bool,
) {
    use ra_core::{finish::FinishReason, session::rollout::RolloutRunEnd};
    use ra_session::rollout::{RolloutPayload, RolloutReader, reconstruct_history};

    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(directory.path()));
    let scripts = Arc::new(Scripts::default());
    let worker = worker_agent(Vec::new());
    let control = AgentControl::new()
        .with_close_descendants_on_cancel(true)
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    let control = if deadline {
        control
            .with_spawned_agent_factory(Arc::new(DeadlineFactory {
                worker: worker.clone(),
                deadline_after: Duration::from_secs(2),
            }))
            .unwrap()
    } else {
        control
            .with_rollout_budget(RolloutBudgetConfig::new(3_000, Vec::new()))
            .unwrap()
    };
    if deadline {
        scripts.push(
            "worker",
            Step::Respond(spawn_call("w-spawn", "helper", "helper", "Stay available")),
        );
        scripts.push("helper", Step::Hang);
    }
    scripts.push(
        "worker",
        Step::Respond(
            final_message("w-1", "previous answer")
                .with_usage(Usage::from_request(RequestUsage::new(1_100, 5))),
        ),
    );
    let registry = AgentRegistry::builder()
        .register(worker.clone())
        .register(helper_agent(Vec::new()))
        .build()
        .unwrap();
    let config = RunConfig::new()
        .with_agent_registry(registry)
        .with_capability(Arc::new(worker_compaction_capability()));
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "first task")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    Runner::run(root_request(&scripts, &control, worker).with_config(config))
        .await
        .unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Completed(Some("previous answer".into()))
    })
    .await;
    if deadline {
        until("the helper started sampling", || {
            scripts.inputs("helper").len() == 1
        })
        .await;
    }

    scripts.push(
        "worker",
        if deadline {
            Step::Hang
        } else {
            Step::Respond(worker_summary())
        },
    );
    scripts.push(
        "worker",
        Step::Respond(final_message("w-2", "extra sample")),
    );
    let mail = "Retain this follow-up even when the budget ends.";
    follow_up_worker(&control, mail).await;
    let terminal = eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Interrupted | AgentStatus::Errored(_))
    })
    .await;
    assert!(
        matches!(&terminal, AgentStatus::Errored(message) if message.contains("budget_exhausted")),
        "{terminal:?}"
    );
    assert_eq!(scripts.inputs("worker").len(), 2 + usize::from(deadline));
    if deadline {
        assert_eq!(control.status(&helper_path()), Some(AgentStatus::Running));
    }

    let session = control
        .agents()
        .into_iter()
        .find(|agent| agent.agent_path() == &worker_path())
        .unwrap()
        .session_id()
        .unwrap()
        .clone();
    let path = store.rollout_path(&session).unwrap();
    control.shutdown().await;
    let records = RolloutReader::open(path).read_all().await.unwrap();
    assert!(records.iter().any(|record| matches!(
        record.payload().unwrap(),
        RolloutPayload::RunEnded(end)
            if end.end() == RolloutRunEnd::Completed
                && end.finish_reason() == Some(FinishReason::BudgetExhausted)
    )));
    let recovered = reconstruct_history(&records).unwrap();
    assert_eq!(
        texts(recovered.history()).join("\n").matches(mail).count(),
        1
    );
    assert_eq!(summaries(recovered.history()).len(), usize::from(!deadline));
}

struct InvalidWorkerConcurrency(Arc<AgentSpec>);

#[async_trait]
impl SpawnedAgentFactory for InvalidWorkerConcurrency {
    async fn prepare(&self, _: SpawnedAgentRequest<'_>) -> Result<PreparedSpawnedAgent> {
        Ok(PreparedSpawnedAgent::new(Arc::clone(&self.0))
            .with_run_config(|config| config.with_max_function_tool_concurrency(0)))
    }
}

#[tokio::test]
async fn spawned_agent_validates_configuration_before_compacting_forked_history() {
    let scripts = Arc::new(Scripts::default());
    let worker = worker_agent(Vec::new());
    let control = AgentControl::new()
        .with_spawned_agent_factory(Arc::new(InvalidWorkerConcurrency(worker.clone())))
        .unwrap();
    scripts.push(
        "lead",
        Step::Respond(
            ModelResponse::new(vec![spawn_with_fork("l-spawn", "worker", Some("all"))])
                .with_usage(Usage::from_request(RequestUsage::new(1_100, 5))),
        ),
    );
    scripts.push("lead", Step::Respond(final_message("l-done", "spawned")));
    scripts.push("worker", Step::Respond(worker_summary()));
    Runner::run(
        root_request(&scripts, &control, worker.clone())
            .with_config(worker_compaction_config(worker)),
    )
    .await
    .unwrap();

    let terminal = eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Errored(_))
    })
    .await;
    assert!(
        matches!(&terminal, AgentStatus::Errored(message) if message.contains("max_function_tool_concurrency")),
        "{terminal:?}"
    );
    assert!(scripts.inputs("worker").is_empty());
}

/// Context injected while a spawned run waits for approval reaches the run once the host answers
/// the checkpoint it took before the injection, as the thread hands a paused turn's context to the
/// continued run rather than to the checkpoint it handed out.
#[tokio::test]
async fn context_injected_while_a_run_waits_for_approval_reaches_the_continued_run() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_root_session_id(ra_core::session::SessionId::new("root"))
        .unwrap();
    let paused = spawn_guarded_worker(&scripts, &control).await;
    let mut state = paused.state().clone();
    let session = control.live_instances()[0].session_id().clone();
    assert!(control.inject_context(
        &session,
        vec![Message::developer("rule saved while paused")]
    ));

    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    scripts.push("worker", Step::Respond(final_message("w-2", "deployed")));
    control.resume(&worker_path(), state).unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let worker = scripts.inputs("worker");
    assert_eq!(worker.len(), 2);
    let resumed = texts(&worker[1]);
    assert!(
        resumed.iter().any(|text| text == "rule saved while paused"),
        "{resumed:?}"
    );
}

/// Context injected while a spawned run waits for approval stays in the agent's history when the
/// run is cancelled instead, and reaches its next run.
#[tokio::test]
async fn context_injected_while_a_run_waits_for_approval_survives_its_cancellation() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_root_session_id(ra_core::session::SessionId::new("root"))
        .unwrap();
    let _paused = spawn_guarded_worker(&scripts, &control).await;
    let session = control.live_instances()[0].session_id().clone();
    assert!(control.inject_context(
        &session,
        vec![Message::developer("rule saved while paused")]
    ));

    control.root().interrupt("worker").await.unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Interrupted
    })
    .await;
    scripts.push("worker", Step::Respond(final_message("w-2", "done")));
    control
        .root()
        .send(
            "worker",
            "carry on".to_owned(),
            MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap();
    until("the worker ran again", || {
        scripts.inputs("worker").len() == 2
    })
    .await;
    let next = texts(&scripts.inputs("worker")[1]);
    assert!(
        next.iter().any(|text| text == "rule saved while paused"),
        "{next:?}"
    );
}

/// Context injected after the runner has closed a run's context lane for an approval stop, but
/// before the tree has taken the pause, reaches the run once the host answers: it is handed to the
/// paused run's own lane rather than kept until the whole run ends.
#[tokio::test]
async fn context_injected_as_a_run_stops_for_approval_reaches_the_continued_run() {
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_root_session_id(ra_core::session::SessionId::new("root"))
        .unwrap();
    let injected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_control = control.clone();
    let hook_injected = Arc::clone(&injected);
    control.set_before_pause_hook(Arc::new(move || {
        if hook_injected.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let session = hook_control.live_instances()[0].session_id().clone();
        assert!(hook_control.inject_context(
            &session,
            vec![Message::developer("rule saved as the run stopped")]
        ));
    }));
    let paused = spawn_guarded_worker(&scripts, &control).await;
    assert!(injected.load(std::sync::atomic::Ordering::SeqCst));

    let mut state = paused.state().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    scripts.push("worker", Step::Respond(final_message("w-2", "deployed")));
    control.resume(&worker_path(), state).unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let resumed = texts(&scripts.inputs("worker")[1]);
    assert!(
        resumed
            .iter()
            .any(|text| text == "rule saved as the run stopped"),
        "{resumed:?}"
    );
}

/// Context a spawned run took in while it waited for approval, and had not delivered when the
/// continued run failed — here interrupted while the approved call runs — stays in the agent's
/// history and reaches its next run, as the thread keeps a failed turn's context.
#[tokio::test]
async fn context_a_continued_run_had_not_delivered_when_it_failed_reaches_the_next_run() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(directory.path()));
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "ship it")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "deploy", json!({}))),
    );
    Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(vec![Arc::new(GuardedTool::hanging(Arc::clone(&started)))]),
    ))
    .await
    .unwrap();
    let paused = tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .expect("the worker pauses for approval");
    let session = control.live_instances()[0].session_id().clone();
    assert!(control.inject_context(
        &session,
        vec![Message::developer("rule saved while paused")]
    ));

    let mut state = paused.state().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    control.resume(&worker_path(), state).unwrap();
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("the approved call runs");
    control.root().interrupt("worker").await.unwrap();
    eventually_status(&control, &worker_path(), |status| {
        *status == AgentStatus::Interrupted
    })
    .await;
    assert_eq!(
        scripts.inputs("worker").len(),
        1,
        "the run sampled nothing more"
    );

    scripts.push("worker", Step::Respond(final_message("w-2", "done")));
    control
        .root()
        .send(
            "worker",
            "carry on".to_owned(),
            MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap();
    until("the worker ran again", || {
        scripts.inputs("worker").len() == 2
    })
    .await;
    let next = texts(&scripts.inputs("worker")[1]);
    assert!(
        next.iter().any(|text| text == "rule saved while paused"),
        "{next:?}"
    );
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let path = store.rollout_path(&session).unwrap();
    control.shutdown().await;
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("rule saved while paused"),
        "the agent's rollout keeps the context"
    );
}

/// A spawn a host froze while it was still preparing, and that cleaned up once the tree shut
/// down, is reported closed by a later settle even though nothing of it is left in the tree: the
/// host holding it as frozen learns it may let go of it.
#[tokio::test]
async fn a_frozen_spawn_that_cleaned_up_during_shutdown_is_reported_closed() {
    let scripts = Arc::new(Scripts::default());
    let factory = Arc::new(GatedFactory {
        inner: PreparingFactory::new(prepared_worker(), false),
        entered: Notify::new(),
        proceed: Notify::new(),
    });
    let store = Arc::new(ra_session::InMemoryThreadStore::new());
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory.clone())
        .unwrap()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("r-parent-1", "work")));
    scripts.push(
        "lead",
        Step::Respond(final_message("r-parent-2", "finished")),
    );
    let run = tokio::spawn(Runner::run(root_request(
        &scripts,
        &control,
        worker_agent(Vec::new()),
    )));
    tokio::time::timeout(Duration::from_secs(5), factory.entered.notified())
        .await
        .unwrap();
    let frozen = control.stop_spawning();
    assert_eq!(frozen.len(), 1, "the preparing spawn is frozen");

    let shutdown = control.shutdown_close().finished().await;
    assert!(shutdown.is_complete(), "{shutdown:?}");
    factory.proceed.notify_one();
    let _ = run.await;
    until("the refused spawn cleaned up", || {
        control.live_instances().is_empty()
    })
    .await;
    assert_eq!(factory.inner.released.load(Ordering::SeqCst), 1);

    let settled = control.settle_spawns().await;
    assert!(settled.failed().is_empty(), "{settled:?}");
    assert_eq!(settled.closed(), frozen.as_slice());
}

/// Fails the step it captures once armed, with an error that carries no checkpoint.
#[derive(Default)]
struct FailingStepObserver(std::sync::atomic::AtomicBool);

#[async_trait]
impl ra_runtime::runner::StepExecutionObserver for FailingStepObserver {
    async fn capture(
        &self,
        _run: &ra_core::context::RunContext,
        _cancel: &CancelScope,
    ) -> Result<()> {
        if self.0.load(Ordering::SeqCst) {
            return Err(Error::caller("the step could not be captured"));
        }
        Ok(())
    }
}

/// Context a spawned run took in while it waited for approval stays in the agent's history and
/// rollout when the continued run fails, after the approved call returned and before the context
/// was delivered, with an error that carries no checkpoint to find it in.
#[tokio::test]
async fn context_a_continued_run_had_not_delivered_survives_an_error_without_a_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(directory.path()));
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    let observer = Arc::new(FailingStepObserver::default());
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "ship it")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "deploy", json!({}))),
    );
    let worker = worker_agent(vec![Arc::new(GuardedTool::new())]);
    let registry = AgentRegistry::builder()
        .register(Arc::clone(&worker))
        .build()
        .unwrap();
    let config = RunConfig::new()
        .with_max_turns(32)
        .with_agent_registry(registry)
        .with_step_execution_observer(Arc::clone(&observer) as _);
    Runner::run(root_request(&scripts, &control, worker).with_config(config))
        .await
        .unwrap();
    let paused = tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .expect("the worker pauses for approval");
    let session = control.live_instances()[0].session_id().clone();
    assert!(control.inject_context(
        &session,
        vec![Message::developer("rule saved while paused")]
    ));

    observer.0.store(true, Ordering::SeqCst);
    let mut state = paused.state().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    control.resume(&worker_path(), state).unwrap();
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Errored(_))
    })
    .await;
    assert_eq!(
        scripts.inputs("worker").len(),
        1,
        "the run sampled nothing more"
    );

    observer.0.store(false, Ordering::SeqCst);
    scripts.push("worker", Step::Respond(final_message("w-2", "done")));
    control
        .root()
        .send(
            "worker",
            "carry on".to_owned(),
            MessageDeliveryMode::TriggerTurn,
        )
        .await
        .unwrap();
    until("the worker ran again", || {
        scripts.inputs("worker").len() == 2
    })
    .await;
    let next = texts(&scripts.inputs("worker")[1]);
    assert!(
        next.iter().any(|text| text == "rule saved while paused"),
        "{next:?}"
    );
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Completed(_))
    })
    .await;
    let path = store.rollout_path(&session).unwrap();
    control.shutdown().await;
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("rule saved while paused"),
        "the agent's rollout keeps the context"
    );
}

/// Prepares the worker with its own wall-clock deadline and leaves its descendants unbounded.
struct DeadlineFactory {
    worker: Arc<AgentSpec>,
    deadline_after: Duration,
}

#[async_trait]
impl SpawnedAgentFactory for DeadlineFactory {
    async fn prepare(&self, request: SpawnedAgentRequest<'_>) -> Result<PreparedSpawnedAgent> {
        if request.path() != &worker_path() {
            return Ok(
                PreparedSpawnedAgent::new(Arc::clone(request.agent())).with_run_config(|config| {
                    config.with_budget(ra_core::budget::BudgetLimit::default())
                }),
            );
        }
        let deadline = ra_core::cancel::Deadline::after(self.deadline_after);
        Ok(PreparedSpawnedAgent::new(Arc::clone(&self.worker))
            .with_run_config(move |config| config.with_deadline(deadline)))
    }
}

/// Context a spawned run took in while it waited for approval is delivered, and recorded in the
/// agent's rollout, when the continued run stops softly on its wall clock while the approved call
/// runs: the runner ends it as a completed run, so nothing reads its context as a failed run's.
#[tokio::test]
async fn context_injected_while_paused_is_recorded_when_the_continued_run_runs_out_of_time() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ra_session::RolloutThreadDirectory::new(directory.path()));
    let scripts = Arc::new(Scripts::default());
    let started = Arc::new(Notify::new());
    let factory = Arc::new(DeadlineFactory {
        worker: worker_agent(vec![Arc::new(GuardedTool::hanging(Arc::clone(&started)))]),
        deadline_after: Duration::from_millis(500),
    });
    let control = AgentControl::new()
        .with_spawned_agent_factory(factory)
        .unwrap()
        .with_rollout_store(store.clone(), ra_core::session::SessionId::new("root"))
        .unwrap();
    scripts.push("lead", Step::Respond(spawn_worker("l-1", "ship it")));
    scripts.push("lead", Step::Respond(final_message("l-2", "spawned")));
    scripts.push(
        "worker",
        Step::Respond(tool_call("w-1", "w-1-call", "deploy", json!({}))),
    );
    Runner::run(root_request(&scripts, &control, worker_agent(Vec::new())))
        .await
        .unwrap();
    let paused = tokio::time::timeout(Duration::from_secs(10), control.wait_for_paused_run())
        .await
        .expect("the worker pauses for approval");
    let session = control.live_instances()[0].session_id().clone();
    assert!(control.inject_context(
        &session,
        vec![Message::developer("rule saved while paused")]
    ));

    let mut state = paused.state().clone();
    let pending: Vec<RunItem> = state.pending_interruption_items().cloned().collect();
    state.approve(&pending[0], false).unwrap();
    control.resume(&worker_path(), state).unwrap();
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("the approved call runs");
    // The run ends on its budget without concluding, which the tree reports as such.
    eventually_status(&control, &worker_path(), |status| {
        matches!(status, AgentStatus::Errored(message) if message.contains("budget_exhausted"))
    })
    .await;
    assert_eq!(
        scripts.inputs("worker").len(),
        1,
        "the run sampled nothing more"
    );

    let path = store.rollout_path(&session).unwrap();
    control.shutdown().await;
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("rule saved while paused"),
        "the agent's rollout keeps the context"
    );
}
