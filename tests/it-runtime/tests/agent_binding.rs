//! R3-12 contracts for the public agent / execution agent split.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    item::{
        CallId, ItemId, ItemProvenance, Message, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ProviderKey, ResolvedModel,
    },
    state::{RunId, ToolFailureTracker, ToolUseTracker},
    tool::{Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::{
    agent::AgentBinding,
    turn::{
        TurnSettlementRequest,
        prepare::{TurnActionSurface, TurnPreparationRequest, prepare_turn},
        settle_turn,
    },
};
use serde_json::json;

struct FakeModel;

#[async_trait]
impl Model for FakeModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("fake model is not invoked here"))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::empty().boxed()
    }
}

/// Records the model selector it was asked to resolve, so a test can assert *whose* selection won.
struct RecordingResolver {
    seen: Mutex<Vec<Option<String>>>,
}

impl RecordingResolver {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<Option<String>> {
        self.seen.lock().unwrap().clone()
    }
}

impl ModelResolver for RecordingResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        self.seen
            .lock()
            .unwrap()
            .push(model_name.map(str::to_owned));
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(FakeModel),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct StubTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    /// Public IDs of the agents this tool was told it was running for.
    seen_agents: Arc<Mutex<Vec<String>>>,
}

impl StubTool {
    fn new(name: &str) -> Self {
        Self {
            seen_agents: Arc::new(Mutex::new(Vec::new())),
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
        }
    }
}

#[async_trait]
impl Tool for StubTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.seen_agents
            .lock()
            .unwrap()
            .push(context.run().agent_id().as_str().to_owned());
        Ok(ToolOutput::text("ok"))
    }
}

fn tool(name: &str) -> Arc<dyn Tool> {
    Arc::new(StubTool::new(name))
}

/// The live context of the run a binding is executing.
///
/// It names the **public** agent, for the reason the binding itself exists: a prepared instance is
/// what runs, and what a tool, a hook or a dynamic instruction is told is who the user configured.
fn run(binding: &AgentBinding) -> RunContext {
    RunContext::new(RunId::new("run-binding"), Arc::clone(binding.public()))
}

/// The agent as the user wrote it down.
fn public_agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .model("public/model")
        .tool(tool("read_file"))
        .tool(tool("write_file"))
        .build()
        .unwrap()
}

/// What a sandbox or capability step might hand back: a different ID, a narrower tool set, and a
/// model of its own.
fn prepared_agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("coder#sandbox-3f2a"))
        .name("Coder (sandboxed)")
        .model("execution/model")
        .tool(tool("read_file"))
        .build()
        .unwrap()
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
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

#[test]
fn test_agent_binding_01() {
    let agent = public_agent();
    let binding = AgentBinding::direct(Arc::clone(&agent));

    assert!(!binding.is_prepared());
    assert_eq!(binding.public_id(), agent.id());
    assert_eq!(binding.execution().id(), agent.id());

    let prepared = AgentBinding::prepared(agent, prepared_agent());
    assert!(prepared.is_prepared());
}

#[test]
fn test_agent_binding_02() {
    // 一个 sandbox clone 完全可以沿用公共 agent 的 ID——它还是同一个 agent，只是装配方式不同。
    // 「两个 ID 不一样吗」这个问法会对这种情形答 false，而被换掉的恰恰是要跑的那套工具。
    let public = public_agent();
    let same_id_clone = AgentSpec::builder()
        .id(public.id().clone())
        .name(public.name())
        .tool(tool("read_file"))
        .build()
        .unwrap();

    let binding = AgentBinding::prepared(Arc::clone(&public), same_id_clone);
    assert_eq!(binding.public_id(), binding.execution().id());
    assert!(
        binding.is_prepared(),
        "ID 相同但工具面已被换掉，这依然是一个准备过的实例"
    );
}

#[tokio::test]
async fn test_agent_binding_03() {
    let binding = AgentBinding::prepared(public_agent(), prepared_agent());
    let resolver = RecordingResolver::new();
    let cancel = CancelScope::root();

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &binding,
        &resolver,
        &run(&binding),
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap();

    // 广播的是执行实例的工具面。按公共 agent 解析会把 sandbox 步骤刚拿掉的 `write_file`
    // 重新递给模型，而错要到模型真的调它的时候才现形。
    let advertised = prepared
        .action_surface()
        .advertised_names()
        .collect::<Vec<_>>();
    assert_eq!(advertised, ["read_file"]);

    // 模型选择器同理：跑起来的是执行实例选的那个。
    assert_eq!(resolver.seen(), [Some("execution/model".to_owned())]);
}

#[tokio::test]
async fn test_agent_binding_04() {
    let binding = AgentBinding::prepared(public_agent(), prepared_agent());
    let surface = TurnActionSurface::new(vec![tool("read_file")], Vec::new()).unwrap();
    let response = ModelResponse::new(vec![tool_call("c-1", "call-1", "read_file")]);
    let cancel = CancelScope::root();
    let mut tracker = ToolUseTracker::new();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding,
        &response,
        &surface,
        Arc::new(run(&binding)),
        &cancel,
        &mut tracker,
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    // 工具轨迹记在用户配置的那个 agent 名下。记到执行期 clone 头上，用户配的是 `coder`，
    // 审计里却出现一个他从没写过的 `coder#sandbox-3f2a`。
    let agents = tracker
        .agents()
        .map(|(id, _)| id.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(agents, ["coder"]);
    assert!(tracker.used_any_this_turn(&AgentId::new("coder")));

    // 本轮生成的每一条记录也一样：会话回读时要用用户认得的说法说清是谁产出的。
    assert!(!settled.session_step_items().is_empty());
    for stored in settled.session_step_items() {
        let provenance = stored
            .provenance()
            .unwrap_or_else(|| panic!("记录 `{}` 没有归属", stored.id()));
        assert_eq!(provenance.agent_id().as_str(), "coder");
        assert_eq!(provenance.agent_name(), Some("Coder"));
    }
}

#[tokio::test]
async fn test_agent_binding_05() {
    let binding = AgentBinding::direct(public_agent());
    let surface = TurnActionSurface::new(Vec::new(), Vec::new()).unwrap();
    // 一条已经声明了产出者的记录——R12 的嵌套子 run 就会这么标它自己的项。
    let nested = item(
        "msg-1",
        RunItemKind::Message(Message::assistant("子 agent 说的", OutputPhase::Final)),
    )
    .with_provenance(ItemProvenance::new(AgentId::new("researcher")));
    let response = ModelResponse::new(vec![nested]);
    let cancel = CancelScope::root();
    let mut tracker = ToolUseTracker::new();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding,
        &response,
        &surface,
        Arc::new(run(&binding)),
        &cancel,
        &mut tracker,
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    // 只填空的，不覆盖。已经报了产出者的记录，是从更清楚的地方来的。
    assert_eq!(
        settled.session_step_items()[0]
            .provenance()
            .unwrap()
            .agent_id()
            .as_str(),
        "researcher"
    );
}

#[tokio::test]
async fn test_agent_binding_06() {
    let binding = AgentBinding::direct(public_agent());
    let surface = TurnActionSurface::new(Vec::new(), Vec::new()).unwrap();
    let response = ModelResponse::new(vec![item(
        "msg-1",
        RunItemKind::Message(Message::assistant("完事了", OutputPhase::Final)),
    )]);
    let cancel = CancelScope::root();
    let mut tracker = ToolUseTracker::new();

    let settled = settle_turn(TurnSettlementRequest::new(
        &binding,
        &response,
        &surface,
        Arc::new(run(&binding)),
        &cancel,
        &mut tracker,
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    assert_eq!(
        settled.session_step_items()[0]
            .provenance()
            .unwrap()
            .agent_id()
            .as_str(),
        "coder"
    );
    // 一轮什么都没要，agent 依然在册：它确实跑了一轮。
    assert!(tracker.agent(&AgentId::new("coder")).is_some());
    assert!(!tracker.used_any_this_turn(&AgentId::new("coder")));
}

#[tokio::test]
async fn test_agent_binding_07() {
    // What a running tool is told about the run follows the same rule as records and counts: the
    // public agent, even though a prepared instance is what advertised the surface and ran. A tool
    // that saw `coder#sandbox-3f2a` here would report, log and branch on an identity the user never
    // wrote down — and it would do it from inside the one object every host-facing stage reads.
    let binding = AgentBinding::prepared(public_agent(), prepared_agent());
    let stub = Arc::new(StubTool::new("read_file"));
    let seen = Arc::clone(&stub.seen_agents);
    let surface = TurnActionSurface::new(vec![stub], Vec::new()).unwrap();
    let response = ModelResponse::new(vec![tool_call("c-1", "call-1", "read_file")]);
    let cancel = CancelScope::root();

    let context = run(&binding);
    assert_eq!(context.agent_id().as_str(), "coder");
    assert_eq!(context.run_id().as_str(), "run-binding");

    settle_turn(TurnSettlementRequest::new(
        &binding,
        &response,
        &surface,
        Arc::new(context),
        &cancel,
        &mut ToolUseTracker::new(),
        &mut ToolFailureTracker::new(),
    ))
    .await
    .unwrap();

    assert_eq!(*seen.lock().unwrap(), ["coder"]);
}
