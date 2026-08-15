//! R3-2 contracts for resolving a model response against the surface the turn advertised.

use std::sync::Arc;

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    item::{
        CallId, HandoffCall, ItemId, McpApprovalRequest, Message, ModelResponse, OutputPhase,
        RunItem, RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelHandoffDefinition, ModelRequest, ModelResolver, ModelSelector,
        ModelSettings, ModelStream, ProviderKey, ResolvedModel,
    },
    state::{RunId, ToolUseTracker},
    step::ToolUse,
    tool::{
        Tool, ToolAvailability, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    turn::{
        prepare::{TurnActionSurface, TurnPreparationRequest, prepare_turn},
        process::process_model_response,
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

struct FixedResolver;

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
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
    options: ToolOptions,
    enabled: bool,
}

impl StubTool {
    fn new(name: &str, availability: ToolAvailability, enabled: bool) -> Self {
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
            options: ToolOptions::new().with_availability(availability),
            enabled,
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

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("unused"))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn is_enabled(&self, _context: &RunContext) -> Result<bool> {
        Ok(self.enabled)
    }
}

fn tool(name: &str) -> Arc<dyn Tool> {
    Arc::new(StubTool::new(name, ToolAvailability::Enabled, true))
}

fn dynamic_tool(name: &str, enabled: bool) -> Arc<dyn Tool> {
    Arc::new(StubTool::new(name, ToolAvailability::Dynamic, enabled))
}

fn handoff(name: &str, target: &str) -> ModelHandoffDefinition {
    ModelHandoffDefinition::new(
        AgentId::new(target),
        name,
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    )
}

/// Wraps a plain agent as the binding preparation and settlement take (R3-12). These tests are not
/// about a prepared instance, so both identities are the same object.
fn direct(agent: &Arc<AgentSpec>) -> AgentBinding {
    AgentBinding::direct(Arc::clone(agent))
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
fn test_response_classification_01() {
    let surface = TurnActionSurface::new(
        vec![tool("write_file"), tool("read_file")],
        vec![handoff("transfer_to_reviewer", "reviewer")],
    )
    .unwrap();
    let response = ModelResponse::new(vec![
        item(
            "msg-1",
            RunItemKind::Message(Message::assistant("我先说一句", OutputPhase::Commentary)),
        ),
        tool_call("call-item-1", "call-1", "write_file"),
        tool_call("call-item-2", "call-2", "transfer_to_reviewer"),
        tool_call("call-item-3", "call-3", "vanished"),
        item(
            "approval-1",
            RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                "req-1",
                "docs",
                "search",
                json!({ "query": "x" }),
            )),
        ),
    ]);

    let processed = process_model_response(&response, &surface).unwrap();

    assert_eq!(processed.new_items().len(), 5);
    assert_eq!(processed.functions().len(), 1);
    assert_eq!(
        processed.functions()[0].tool().origin().name(),
        "write_file"
    );
    assert_eq!(processed.handoffs().len(), 1);
    assert_eq!(processed.handoffs()[0].target_agent().as_str(), "reviewer");
    // 交接在线上就是普通函数调用，重放需要模型当时用的那个名字。
    assert_eq!(
        processed.handoffs()[0].call().tool_name(),
        Some("transfer_to_reviewer")
    );
    assert_eq!(processed.tools_not_found().len(), 1);
    assert_eq!(processed.tools_not_found()[0].name(), "vanished");
    assert_eq!(processed.mcp_approval_requests().len(), 1);

    assert!(processed.has_tools_or_approvals_to_run());
    assert!(processed.has_interruptions());
    assert_eq!(processed.tools_used().len(), 4);
}

#[test]
fn test_response_classification_02() {
    let surface = TurnActionSurface::new(vec![tool("write_file")], Vec::new()).unwrap();
    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "wrte_file")]);

    // 模型打错一个名字是常事，答案是一条它读得懂的失败观察，而不是一个死掉的 run。
    let processed = process_model_response(&response, &surface).unwrap();
    assert!(processed.functions().is_empty());
    assert_eq!(processed.tools_not_found()[0].call_id().as_str(), "call-1");
    assert!(processed.has_tools_or_approvals_to_run());
    assert_eq!(
        processed.tools_used(),
        [ToolUse::Unresolved("wrte_file".to_owned())]
    );
}

#[tokio::test]
async fn test_response_classification_03() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([tool("write_file"), dynamic_tool("gone_this_turn", false)])
        .build()
        .unwrap();
    let resolver = FixedResolver;
    let context = RunContext::new(RunId::new("run-classification"), Arc::clone(&agent));
    let cancel = CancelScope::root();

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap();

    // 动作面在准备阶段就建好了，结算走 `into_call` 把它和请求一起接走——
    // 从 agent 重新推一份就会用「声明的」工具而不是本轮启用快照。
    assert_eq!(prepared.action_surface().tools().len(), 1);
    let (surface, _request) = prepared.into_call();

    let response = ModelResponse::new(vec![
        tool_call("call-item-1", "call-1", "write_file"),
        tool_call("call-item-2", "call-2", "gone_this_turn"),
    ]);
    let processed = process_model_response(&response, &surface).unwrap();

    // 这一轮被 `is_enabled` 关掉的工具，结算阶段不能把它重新解析出来再跑一遍。
    assert_eq!(processed.functions().len(), 1);
    assert_eq!(
        processed.functions()[0].tool().origin().name(),
        "write_file"
    );
    assert_eq!(processed.tools_not_found()[0].name(), "gone_this_turn");
}

#[tokio::test]
async fn test_response_classification_04() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([tool("write_file"), dynamic_tool("gone_this_turn", false)])
        .build()
        .unwrap();
    let resolver = FixedResolver;
    let context = RunContext::new(RunId::new("run-classification"), Arc::clone(&agent));
    let cancel = CancelScope::root();

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap();

    // 校验发生在准备阶段而不是调用方主动取快照时——先 `into_request()` 的 runner
    // 不会因此把一个有歧义的动作面发给模型。
    let surface = prepared.action_surface();
    assert_eq!(surface.tools().len(), 1);
    assert!(surface.find_tool("write_file").is_some());
    assert!(surface.find_tool("gone_this_turn").is_none());
    assert_eq!(
        surface.advertised_names().collect::<Vec<_>>(),
        ["write_file"]
    );
    // 请求面与可执行面出自同一份快照。
    assert_eq!(prepared.request().tools().len(), 1);
    assert_eq!(prepared.tools().len(), 1);
}

#[test]
fn test_response_classification_05() {
    // handoff 与 tool 共用线上命名空间；留着它就得在结算里随便挑一边，那是静默的错。
    let error = TurnActionSurface::new(
        vec![tool("transfer_to_reviewer")],
        vec![handoff("transfer_to_reviewer", "reviewer")],
    )
    .unwrap_err();
    assert!(error.to_string().contains("transfer_to_reviewer"));

    let duplicate_tools =
        TurnActionSurface::new(vec![tool("search"), tool("search")], Vec::new()).unwrap_err();
    assert!(duplicate_tools.to_string().contains("search"));
}

#[test]
fn test_response_classification_06() {
    let surface = TurnActionSurface::new(
        vec![tool("write_file")],
        vec![handoff("transfer_to_reviewer", "reviewer")],
    )
    .unwrap();
    let response = ModelResponse::new(vec![item(
        "call-item-1",
        RunItemKind::HandoffCall(HandoffCall::new(
            CallId::new("call-1"),
            AgentId::new("root_admin"),
            json!({}),
        )),
    )]);

    // 没人授权过的控制权转移，跑起来比拒绝要糟得多。
    let error = process_model_response(&response, &surface).unwrap_err();
    assert!(error.to_string().contains("root_admin"));

    let allowed = ModelResponse::new(vec![item(
        "call-item-1",
        RunItemKind::HandoffCall(HandoffCall::new(
            CallId::new("call-1"),
            AgentId::new("reviewer"),
            json!({}),
        )),
    )]);
    let processed = process_model_response(&allowed, &surface).unwrap();
    assert_eq!(processed.handoffs()[0].target_agent().as_str(), "reviewer");
}

#[test]
fn test_response_classification_07() {
    let surface = TurnActionSurface::new(
        Vec::new(),
        vec![handoff("transfer_to_reviewer", "reviewer")],
    )
    .unwrap();
    let response = ModelResponse::new(vec![tool_call(
        "call-item-1",
        "call-1",
        "transfer_to_reviewer",
    )]);

    let processed = process_model_response(&response, &surface).unwrap();
    assert!(matches!(
        processed.new_items()[0].kind(),
        RunItemKind::ToolCall(_)
    ));
    assert_eq!(processed.handoffs()[0].item_id().as_str(), "call-item-1");
}

#[test]
fn test_response_classification_08() {
    let surface = TurnActionSurface::new(vec![tool("write_file")], Vec::new()).unwrap();
    let response = ModelResponse::new(vec![item(
        "msg-1",
        RunItemKind::Message(Message::assistant("完事了", OutputPhase::Final)),
    )]);

    let processed = process_model_response(&response, &surface).unwrap();
    assert!(!processed.has_tools_or_approvals_to_run());
    assert!(!processed.has_interruptions());
    assert!(processed.tools_used().is_empty());
}
