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
        ModelSettings, ModelStream, ModelToolDefinition, ProviderKey, ResolvedModel,
    },
    state::{RunId, ToolUseTracker},
    step::ToolUse,
    tool::{Tool, ToolAvailability, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::{
    agent::AgentBinding,
    turn::{
        prepare::{
            ToolNameCollisionPolicy, TurnActionSurface, TurnPreparationRequest, prepare_turn,
        },
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
    advertised_as: Option<String>,
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
            advertised_as: None,
        }
    }

    fn advertised_as(mut self, name: &str) -> Self {
        self.advertised_as = Some(name.to_owned());
        self
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

    fn model_definition(&self) -> ModelToolDefinition {
        match &self.advertised_as {
            Some(name) => {
                ModelToolDefinition::new(name.clone(), self.schema.input_schema().clone())
            }
            None => self.schema.to_model_definition(),
        }
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
    // A handoff is an ordinary function call on the wire, and a replay needs the name the model used.
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

    // A model mistyping a name is routine, and the answer is a failure observation it can read rather
    // than a dead run.
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
    let context = RunContext::new(RunId::new("run-classification"), agent.as_ref());
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

    // The action surface was built during preparation, and settlement takes it along with the request
    // through `into_call` — deriving a fresh one from the agent would use the *declared* tools rather
    // than this turn's enabled snapshot.
    assert_eq!(prepared.action_surface().tools().len(), 1);
    let (surface, _request) = prepared.into_call();

    let response = ModelResponse::new(vec![
        tool_call("call-item-1", "call-1", "write_file"),
        tool_call("call-item-2", "call-2", "gone_this_turn"),
    ]);
    let processed = process_model_response(&response, &surface).unwrap();

    // A tool `is_enabled` switched off this turn may not be resolved back into existence and run by
    // settlement.
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
    let context = RunContext::new(RunId::new("run-classification"), agent.as_ref());
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

    // Validation happens during preparation rather than when a caller asks for the snapshot, so a
    // runner that calls `into_request()` first cannot send the model an ambiguous action surface.
    let surface = prepared.action_surface();
    assert_eq!(surface.tools().len(), 1);
    assert!(surface.find_tool("write_file").is_some());
    assert!(surface.find_tool("gone_this_turn").is_none());
    assert_eq!(
        surface.advertised_names().collect::<Vec<_>>(),
        ["write_file"]
    );
    // The advertised surface and the executable one come from one snapshot.
    assert_eq!(prepared.request().tools().len(), 1);
    assert_eq!(prepared.tools().len(), 1);
}

#[test]
fn test_response_classification_05() {
    // Handoffs and tools share one wire namespace; allowing the collision would leave settlement to
    // pick a side arbitrarily, which is the silent kind of wrong.
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

    // A transfer of control nobody authorized is far worse to run than to refuse.
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

#[test]
fn test_response_classification_09() {
    let surface = TurnActionSurface::new(
        vec![
            Arc::new(StubTool::new("search", ToolAvailability::Enabled, true)),
            Arc::new(
                StubTool::new("search", ToolAvailability::Enabled, true)
                    .advertised_as("jira_search"),
            ),
        ],
        Vec::new(),
    )
    .expect("distinct model-facing names are an unambiguous surface");

    assert_eq!(
        surface.advertised_names().collect::<Vec<_>>(),
        ["search", "jira_search"]
    );
    assert!(surface.find_tool("jira_search").is_some());
    assert!(surface.find_tool("missing").is_none());

    let response = ModelResponse::new(vec![tool_call("call-item-1", "call-1", "jira_search")]);
    let processed = process_model_response(&response, &surface)
        .expect("the advertised projection resolves to its executable tool");

    assert_eq!(processed.functions().len(), 1);
    assert_eq!(processed.functions()[0].tool().origin().name(), "search");
}

#[test]
fn test_response_classification_10() {
    let surface = TurnActionSurface::new_with_collision_policy(
        vec![tool("transfer_to_reviewer")],
        vec![handoff("transfer_to_reviewer", "reviewer")],
        ToolNameCollisionPolicy::Warn,
    )
    .expect("the warning policy keeps one deterministic dispatch winner");

    assert!(surface.find_tool("transfer_to_reviewer").is_none());
    assert_eq!(surface.handoffs().len(), 1);
    assert_eq!(
        surface.advertised_names().collect::<Vec<_>>(),
        ["transfer_to_reviewer"]
    );
    assert!(surface.tool_definitions().is_empty());

    let response = ModelResponse::new(vec![tool_call(
        "call-item-1",
        "call-1",
        "transfer_to_reviewer",
    )]);
    let processed = process_model_response(&response, &surface)
        .expect("the retained handoff is the only resolution for the advertised name");
    assert_eq!(processed.handoffs()[0].target_agent().as_str(), "reviewer");
}

#[test]
fn test_response_classification_11() {
    let surface = TurnActionSurface::new_with_collision_policy(
        vec![tool("search"), tool("search")],
        Vec::new(),
        ToolNameCollisionPolicy::Warn,
    )
    .expect("the warning policy keeps one deterministic dispatch winner");

    assert_eq!(surface.tools().len(), 1);
    assert_eq!(
        surface
            .tool_definitions()
            .iter()
            .map(ModelToolDefinition::name)
            .collect::<Vec<_>>(),
        ["search"]
    );
}
