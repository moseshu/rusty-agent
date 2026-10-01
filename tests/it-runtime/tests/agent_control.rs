//! The multi-agent control plane and its collaboration tools, run end to end. Ported from Codex's
//! `MultiAgentV2` behavior: background agents, mailbox delivery at model-call boundaries, final
//! answers reported to the parent, and the wait / follow-up / interrupt loop a parent uses to stop
//! waiting on a stalled child.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{FutureExt, future::BoxFuture};
use ra_core::{
    agent::{
        AgentId, AgentSpec, HandoffSpec,
        control::{
            AgentControlPort, AgentPath, AgentStatus, InterAgentCommunication, MessageDeliveryMode,
            SpawnAgentForkMode, SpawnAgentRequest, completion_message, fork_history,
        },
    },
    cancel::CancelScope,
    error::{Error, Result},
    finish::FinishReason,
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
    usage::{RequestUsage, Usage},
};
use ra_runtime::{
    agent::{
        AgentBinding, AgentRegistry,
        control::{AgentControl, PausedAgentRun, RolloutBudgetConfig},
        tool::AgentAsTool,
    },
    runner::{ContinuationInput, RunConfig, RunRequest, Runner},
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
    .with_config(RunConfig::new().with_agent_registry(registry))
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

    // The second run carries the first one's settled turn and the follow-up. The turn that was in
    // flight at the interrupt was never recorded, so its call does not reach the next request.
    let worker = scripts.inputs("worker");
    assert_eq!(worker.len(), 3);
    assert_eq!(
        texts(&worker[2]),
        vec![
            "do the work".to_owned(),
            format!("{NEW_TASK}index the repo"),
            "Looking around first.".to_owned(),
            "12 modules".to_owned(),
            format!("{NEW_TASK}Summarize what you saw."),
        ]
    );
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
            "fork_turns must be `none`, `all`, or a positive integer string".to_owned(),
            "fork_turns must be `none`, `all`, or a positive integer string".to_owned(),
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
async fn fork_turns_selects_how_much_of_the_conversation_a_child_inherits() {
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
            spawn_with_fork("l-2b", "last_turn", Some("1")),
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
    for name in ["none_fork", "last_turn", "everything"] {
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
        child("last_turn"),
        vec!["second ask".to_owned(), task("last_turn")]
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
    let scripts = Arc::new(Scripts::default());
    let control = AgentControl::new();
    let specialist = AgentSpec::builder()
        .id(AgentId::new("specialist"))
        .name("Specialist")
        .instructions("specialist")
        .tools(collaboration())
        .build()
        .unwrap();
    let lead = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(collaboration())
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
    let request = RunRequest::new(
        AgentBinding::direct(lead),
        Arc::new(ScriptedResolver(Arc::clone(&scripts))) as Arc<dyn ModelResolver>,
        RunId::new("run-root"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("route this"))],
    )
    .with_config(RunConfig::new().with_agent_registry(registry))
    .with_agent_control(control.root());
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
}

/// A tool that runs only once the host approves it.
struct GuardedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
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
