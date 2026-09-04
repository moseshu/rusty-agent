//! R3-6b contracts for feeding the tool-use tracker from turn settlement.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result, ToolErrorKind},
    item::{
        AgentId, CallId, ItemId, McpApprovalRequest, Message, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::ModelHandoffDefinition,
    state::{RunId, ToolFailureTracker, ToolUse, ToolUseTracker},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolLookupKey, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    turn::{TurnSettlementRequest, prepare::TurnActionSurface, settle_turn},
};
use serde_json::{Value, json};

/// What the tool does when the settlement finally reaches it.
enum Behavior {
    Succeed,
    Fail,
}

struct ScriptedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    behavior: Behavior,
    calls: Arc<AtomicUsize>,
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
        match self.behavior {
            Behavior::Succeed => Ok(ToolOutput::text("ok")),
            Behavior::Fail => Err(Error::tool(
                ToolErrorKind::ExecutionFailed,
                self.origin.qualified_name(),
                "工具内部失败",
            )),
        }
    }

    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        Ok(!matches!(
            self.options.approval(),
            ToolApprovalPolicy::Never
        ))
    }
}

fn agent() -> AgentId {
    AgentId::new("main")
}

fn spec(id: &str) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name(id)
        .build()
        .unwrap()
}

/// The binding a settlement runs under. `direct` because these tests are about what gets recorded,
/// not about the public/execution split — that has its own file.
fn binding(id: &str) -> AgentBinding {
    AgentBinding::direct(spec(id))
}

/// The live context of the run a settlement belongs to, naming the same agent as its binding.
fn run(id: &str) -> Arc<RunContext> {
    let spec = spec(id);
    Arc::new(RunContext::new(RunId::new("run-tool-use"), spec.as_ref()))
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn call(id: &str, call_id: &str, name: &str, arguments: Value) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, arguments)),
    )
}

fn surface(tools: Vec<Arc<dyn Tool>>) -> TurnActionSurface {
    TurnActionSurface::new(tools, Vec::new()).unwrap()
}

fn tool_identity(name: &str) -> ToolUse {
    ToolUse::Tool(ToolLookupKey::bare(name).unwrap())
}

/// Settles one response, recording into the caller's tracker.
async fn settle(
    response: &ModelResponse,
    surface: &TurnActionSurface,
    tracker: &mut ToolUseTracker,
) -> Result<()> {
    let cancel = CancelScope::root();
    settle_turn(TurnSettlementRequest::new(
        &binding("main"),
        response,
        surface,
        run("main"),
        &cancel,
        tracker,
        &mut ToolFailureTracker::new(),
        Default::default(),
    ))
    .await
    .map(|_| ())
}

#[tokio::test]
async fn test_tool_use_tracking_01() {
    let surface = surface(vec![Arc::new(ScriptedTool::new(
        "write_file",
        Behavior::Succeed,
    ))]);
    let response = ModelResponse::new(vec![
        item(
            "msg-1",
            RunItemKind::Message(Message::assistant("先说一句", OutputPhase::Commentary)),
        ),
        call("c-2", "call-2", "vanished", json!({ "path": "a" })),
        item(
            "c-3",
            RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                "req-1",
                "docs",
                "search",
                json!({ "q": "x" }),
            )),
        ),
        call("c-4", "call-4", "write_file", json!({ "path": "a" })),
    ]);
    let mut tracker = ToolUseTracker::new();

    settle(&response, &surface, &mut tracker).await.unwrap();

    // Order follows the response, not the order the fields sit in on `ProcessedResponse`.
    let recorded = tracker
        .agent(&agent())
        .unwrap()
        .entries()
        .iter()
        .map(|entry| entry.identity().clone())
        .collect::<Vec<_>>();
    assert_eq!(
        recorded,
        [
            ToolUse::Unresolved("vanished".to_owned()),
            ToolUse::Mcp {
                server: "docs".to_owned(),
                tool_name: "search".to_owned(),
            },
            tool_identity("write_file"),
        ]
    );
    // A plain message is not an action and does not belong in there.
    assert_eq!(tracker.agent(&agent()).unwrap().turn_calls(), 3);
}

#[tokio::test]
async fn test_tool_use_tracking_02() {
    let tool = Arc::new(ScriptedTool::new("read_file", Behavior::Succeed));
    let surface = surface(vec![tool]);
    let read = tool_identity("read_file");
    let mut tracker = ToolUseTracker::new();

    for turn in 0..3 {
        let response = ModelResponse::new(vec![call(
            &format!("c-{turn}"),
            &format!("call-{turn}"),
            "read_file",
            // Key order changes every turn: a provider promises none, and the fingerprint has to
            // recognize this as the same call.
            if turn % 2 == 0 {
                json!({ "path": "a.txt", "limit": 10 })
            } else {
                json!({ "limit": 10, "path": "a.txt" })
            },
        )]);
        settle(&response, &surface, &mut tracker).await.unwrap();
    }
    assert_eq!(tracker.repeat_streak(&agent(), &read), 3);

    let changed = ModelResponse::new(vec![call(
        "c-9",
        "call-9",
        "read_file",
        json!({ "path": "b.txt", "limit": 10 }),
    )]);
    settle(&changed, &surface, &mut tracker).await.unwrap();
    assert_eq!(tracker.repeat_streak(&agent(), &read), 1);
}

#[tokio::test]
async fn test_tool_use_tracking_03() {
    let failing = Arc::new(ScriptedTool::new("write_file", Behavior::Fail));
    let calls = Arc::clone(&failing.calls);
    let surface = surface(vec![failing]);
    let response = ModelResponse::new(vec![
        call("c-1", "call-1", "write_file", json!({ "path": "a" })),
        call("c-2", "call-2", "vanished", json!({ "path": "a" })),
    ]);
    let mut tracker = ToolUseTracker::new();

    settle(&response, &surface, &mut tracker).await.unwrap();

    // A failure and an unresolvable name are both the model asking for the same thing again, which is
    // what `reset_tool_choice` and the breaker react to; counting only what succeeded blinds both.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        tracker
            .agent(&agent())
            .unwrap()
            .entry(&tool_identity("write_file"))
            .unwrap()
            .run_calls(),
        1
    );
    assert!(tracker.used_any_this_turn(&agent()));
}

#[tokio::test]
async fn test_tool_use_tracking_04() {
    let gated = Arc::new(
        ScriptedTool::new("write_file", Behavior::Succeed)
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let calls = Arc::clone(&gated.calls);
    let surface = surface(vec![gated]);
    let response = ModelResponse::new(vec![call(
        "c-1",
        "call-1",
        "write_file",
        json!({ "path": "a" }),
    )]);
    let mut tracker = ToolUseTracker::new();

    settle(&response, &surface, &mut tracker).await.unwrap();

    // The tool never ran and the model did ask for it. Filing happens before execution precisely so
    // that the breaker inside `dispatch_tool` can see this turn's attempt.
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        tracker.repeat_streak(&agent(), &tool_identity("write_file")),
        1
    );
}

#[tokio::test]
async fn test_tool_use_tracking_05() {
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
    let response = ModelResponse::new(vec![call(
        "c-1",
        "call-1",
        "transfer_to_reviewer",
        json!({}),
    )]);
    let mut tracker = ToolUseTracker::new();

    // Handoff execution is not implemented yet, so this turn fails explicitly.
    let error = settle(&response, &surface, &mut tracker).await.unwrap_err();
    assert!(error.to_string().contains("reviewer"));

    // Filing happens before anything acts. How a turn ended changes nothing about what the model asked
    // for, and that is what the audit trail and the breaker read.
    assert_eq!(
        tracker.repeat_streak(&agent(), &ToolUse::Handoff(AgentId::new("reviewer"))),
        1
    );
}

#[tokio::test]
async fn test_tool_use_tracking_06() {
    let surface = surface(vec![Arc::new(ScriptedTool::new(
        "read_file",
        Behavior::Succeed,
    ))]);
    let response = ModelResponse::new(vec![call(
        "c-1",
        "call-1",
        "read_file",
        json!({ "path": "a" }),
    )]);
    let cancel = CancelScope::root();
    let mut tracker = ToolUseTracker::new();

    for id in ["planner", "executor"] {
        settle_turn(TurnSettlementRequest::new(
            &binding(id),
            &response,
            &surface,
            run(id),
            &cancel,
            &mut tracker,
            &mut ToolFailureTracker::new(),
            Default::default(),
        ))
        .await
        .unwrap();
    }

    // One response and one `call_id`, filed separately per agent. De-duplication is each agent's own
    // window rather than a global one; otherwise the second agent's first call would be swallowed as
    // a replay of the first agent's.
    let read = tool_identity("read_file");
    assert_eq!(tracker.repeat_streak(&AgentId::new("planner"), &read), 1);
    assert_eq!(tracker.repeat_streak(&AgentId::new("executor"), &read), 1);
    assert_eq!(tracker.agents().count(), 2);
}
