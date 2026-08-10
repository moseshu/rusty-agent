//! R3-6b contracts for feeding the tool-use tracker from turn settlement.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ra_core::{
    cancel::CancelScope,
    error::{Error, Result, ToolErrorKind},
    item::{
        AgentId, CallId, ItemId, McpApprovalRequest, Message, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::ModelHandoffDefinition,
    state::{ToolUse, ToolUseTracker},
    tool::{
        Tool, ToolApprovalPolicy, ToolInvocation, ToolLookupKey, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_runtime::turn::{TurnSettlementRequest, prepare::TurnActionSurface, settle_turn};
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

    async fn call(&self, _invocation: ToolInvocation<'_>) -> Result<ToolOutput> {
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

    async fn needs_approval(&self, _invocation: &ToolInvocation<'_>) -> Result<bool> {
        Ok(!matches!(
            self.options.approval(),
            ToolApprovalPolicy::Never
        ))
    }
}

struct Host;

fn agent() -> AgentId {
    AgentId::new("main")
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
        &agent(),
        response,
        surface,
        &Host,
        &cancel,
        tracker,
    ))
    .await
    .map(|_| ())
}

#[tokio::test]
async fn 结算把本轮四类动作按响应原序全部记下() {
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

    // 顺序跟着响应走，不跟着 `ProcessedResponse` 的字段排列走。
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
    // 纯消息不是一次动作，不该在里面。
    assert_eq!(tracker.agent(&agent()).unwrap().turn_calls(), 3);
}

#[tokio::test]
async fn 连续两轮点同一个调用连续段累加参数一变归零() {
    let tool = Arc::new(ScriptedTool::new("read_file", Behavior::Succeed));
    let surface = surface(vec![tool]);
    let read = tool_identity("read_file");
    let mut tracker = ToolUseTracker::new();

    for turn in 0..3 {
        let response = ModelResponse::new(vec![call(
            &format!("c-{turn}"),
            &format!("call-{turn}"),
            "read_file",
            // 键序每轮都换：provider 不承诺顺序，指纹要认得出这是同一个调用。
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
async fn 记的是模型要了什么而不是执行成没成() {
    let failing = Arc::new(ScriptedTool::new("write_file", Behavior::Fail));
    let calls = Arc::clone(&failing.calls);
    let surface = surface(vec![failing]);
    let response = ModelResponse::new(vec![
        call("c-1", "call-1", "write_file", json!({ "path": "a" })),
        call("c-2", "call-2", "vanished", json!({ "path": "a" })),
    ]);
    let mut tracker = ToolUseTracker::new();

    settle(&response, &surface, &mut tracker).await.unwrap();

    // 失败的、解析不到的，都是模型又要了一次同样的东西——`reset_tool_choice`
    // 和熔断器反应的正是这个，只统计跑成功的会让它们都失明。
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
async fn 停下来要审批的那一轮也算模型要过工具() {
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

    // 工具一次都没跑，但模型确实点了它。记账在执行之前，正是为了让 R3-6 的熔断器
    // 在 `dispatch_tool` 里看得到本轮的这一次。
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        tracker.repeat_streak(&agent(), &tool_identity("write_file")),
        1
    );
}

#[tokio::test]
async fn 这一轮报错也不会让已经要过的东西消失() {
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

    // 交接要等 R17 才跑得起来，这一轮明确报错。
    let error = settle(&response, &surface, &mut tracker).await.unwrap_err();
    assert!(error.to_string().contains("reviewer"));

    // 记账在任何东西动手之前。一轮的结局改变不了「模型要过什么」这个事实，
    // 审计与熔断器读的都是后者。
    assert_eq!(
        tracker.repeat_streak(&agent(), &ToolUse::Handoff(AgentId::new("reviewer"))),
        1
    );
}

#[tokio::test]
async fn 归属跟着传进来的公共_agent_身份走() {
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
            &AgentId::new(id),
            &response,
            &surface,
            &Host,
            &cancel,
            &mut tracker,
        ))
        .await
        .unwrap();
    }

    // 同一条响应、同一个 call_id，两个 agent 各记各的。去重是每个 agent 各自的窗口，
    // 不是全局的——否则第二个 agent 的第一次调用会被当成第一个 agent 的重放吞掉。
    let read = tool_identity("read_file");
    assert_eq!(tracker.repeat_streak(&AgentId::new("planner"), &read), 1);
    assert_eq!(tracker.repeat_streak(&AgentId::new("executor"), &read), 1);
    assert_eq!(tracker.agents().count(), 2);
}
