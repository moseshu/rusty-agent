//! Deferred discovery contracts ported from Codex's handler and registry cache tests.

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind, ToolCall,
        ToolCallKind,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings,
        ModelToolDefinition, ProviderKey, ResolvedModel,
    },
    permission::{PermissionDecision, PermissionRule},
    state::{RunId, RunState, ToolUseTracker},
    tool::{
        Tool, ToolAvailability, ToolContext, ToolExposure, ToolNamespace, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
    turn::prepare::{PreparedTurn, TurnPreparationRequest, prepare_turn},
    turn::process::process_model_response,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct CatalogTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    description: Mutex<String>,
    enabled: AtomicBool,
}

impl CatalogTool {
    fn new(name: &str, namespace: Option<&str>, exposure: ToolExposure) -> Arc<Self> {
        Arc::new(Self {
            origin: match namespace {
                Some(namespace) => {
                    ToolOrigin::namespaced(ToolNamespace::new(namespace).unwrap(), name).unwrap()
                }
                None if exposure == ToolExposure::Deferred => {
                    ToolOrigin::deferred_top_level(name).unwrap()
                }
                None => ToolOrigin::new(name).unwrap(),
            },
            schema: ToolSchema::loose(
                name,
                json!({"type": "object", "properties": {
                    "title": {"type": "string", "description": "Event appointment"},
                    "attachments": {"type": "array", "items": {
                        "anyOf": [{"type": "string", "description": "lunar"}]
                    }}
                }, "anyOf": [{"properties": {
                    "orbit_code": {"type": "string", "description": "astronomy"}
                }}]}),
            )
            .unwrap(),
            options: ToolOptions::new()
                .with_exposure(exposure)
                .with_availability(ToolAvailability::Dynamic),
            description: Mutex::new(format!("{name} calendar events")),
            enabled: AtomicBool::new(true),
        })
    }
}

#[async_trait]
impl Tool for CatalogTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
    async fn is_enabled(&self, _: &RunContext) -> Result<bool> {
        Ok(self.enabled.load(Ordering::SeqCst))
    }
    fn model_definition(&self) -> ModelToolDefinition {
        let definition = self
            .schema
            .to_model_definition()
            .with_description(self.description.lock().unwrap().clone())
            .with_defer_loading(self.options.is_discoverable());
        match self.origin.namespace() {
            Some(namespace) => {
                definition.with_namespace(namespace.as_str(), Some("Calendar tools".to_owned()))
            }
            None => definition,
        }
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("called"))
    }
}

struct ScriptedModel {
    script: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            requests: Mutex::new(vec![]),
        })
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("script exhausted"))
    }
}

struct Resolver(Arc<ScriptedModel>);
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(ProviderKey::new("mock"), None, ApiProtocol::OpenAiResponses),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn binding(tools: Vec<Arc<dyn Tool>>) -> AgentBinding {
    AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("agent"))
            .name("Agent")
            .tools(tools)
            .build()
            .unwrap(),
    )
}

async fn prepare(binding: &AgentBinding) -> PreparedTurn {
    let resolver = Resolver(ScriptedModel::new(vec![]));
    let run = RunContext::new(RunId::new("run"), binding.public());
    prepare_turn(TurnPreparationRequest::new(
        binding,
        &resolver,
        &run,
        &CancelScope::root(),
        &ToolUseTracker::new(),
        vec![],
    ))
    .await
    .unwrap()
}

async fn search(
    prepared: &PreparedTurn,
    binding: &AgentBinding,
    arguments: Value,
) -> Result<ToolOutput> {
    let tool = prepared.action_surface().find_tool_search().unwrap();
    let run = RunContext::new(RunId::new("run"), binding.public());
    tool.call(ToolContext::new(
        &run,
        tool.as_ref(),
        &CallId::new("search"),
        &arguments,
    ))
    .await
}

fn call(id: &str, name: &str, kind: ToolCallKind, namespace: Option<&str>) -> ModelResponse {
    let mut call =
        ToolCall::new(CallId::new(id), name, json!({"query": "calendar"})).with_kind(kind);
    if let Some(namespace) = namespace {
        call = call.with_namespace(namespace);
    }
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(call),
    )])
}

fn final_response() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("answer"),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )])
}

#[tokio::test]
async fn search_indexes_only_enabled_deferred_metadata_and_schema() {
    let deferred = CatalogTool::new("create_event", Some("calendar"), ToolExposure::Deferred);
    let hidden = CatalogTool::new("host_only", None, ToolExposure::Hidden);
    let disabled = CatalogTool::new("disabled", None, ToolExposure::Deferred);
    disabled.enabled.store(false, Ordering::SeqCst);
    let binding = binding(vec![deferred, hidden, disabled]);
    let prepared = prepare(&binding).await;
    assert_eq!(prepared.request().tools().len(), 1);
    assert!(prepared.request().tools()[0].kind().is_tool_search());
    assert_eq!(prepared.action_surface().advertised_count(), 1);
    for query in [
        "calendar",
        "create event",
        "appointment",
        "title",
        "orbit_code",
        "astronomy",
        "lunar",
    ] {
        let output = search(&prepared, &binding, json!({"query": query}))
            .await
            .unwrap();
        assert_eq!(output.discovered_tools().unwrap().len(), 1);
        assert_eq!(
            output.discovered_tools().unwrap()[0].qualified_name(),
            "calendar.create_event"
        );
    }
    for query in ["host_only", "disabled", "unrelatedxyz"] {
        assert!(
            search(&prepared, &binding, json!({"query": query}))
                .await
                .unwrap()
                .discovered_tools()
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn cache_reuses_registry_entries_and_rebuilds_for_metadata_or_availability_changes() {
    let tool = CatalogTool::new("lookup", None, ToolExposure::Deferred);
    let binding = binding(vec![tool.clone()]);
    let first = prepare(&binding).await;
    let second = prepare(&binding.clone()).await;
    assert!(Arc::ptr_eq(
        first.action_surface().find_tool_search().unwrap(),
        second.action_surface().find_tool_search().unwrap()
    ));
    *tool.description.lock().unwrap() = "Refresh inventory".to_owned();
    let refreshed = prepare(&binding).await;
    assert!(!Arc::ptr_eq(
        first.action_surface().find_tool_search().unwrap(),
        refreshed.action_surface().find_tool_search().unwrap()
    ));
    assert_eq!(
        search(&refreshed, &binding, json!({"query": "inventory"}))
            .await
            .unwrap()
            .discovered_tools()
            .unwrap()
            .len(),
        1
    );
    tool.enabled.store(false, Ordering::SeqCst);
    let disabled = prepare(&binding).await;
    assert!(disabled.action_surface().find_tool_search().is_none());
    tool.enabled.store(true, Ordering::SeqCst);
    let enabled = prepare(&binding).await;
    assert!(!Arc::ptr_eq(
        refreshed.action_surface().find_tool_search().unwrap(),
        enabled.action_surface().find_tool_search().unwrap()
    ));
}

#[tokio::test]
async fn replacement_registry_entries_invalidate_the_binding_cache() {
    let first_tool = CatalogTool::new("lookup", None, ToolExposure::Deferred);
    let binding = binding(vec![first_tool]);
    let first = prepare(&binding).await;
    let replacement = binding
        .with_external_tools([
            CatalogTool::new("new_lookup", None, ToolExposure::Deferred) as Arc<dyn Tool>
        ])
        .unwrap();
    let second = prepare(&replacement).await;
    assert!(!Arc::ptr_eq(
        first.action_surface().find_tool_search().unwrap(),
        second.action_surface().find_tool_search().unwrap()
    ));
    assert_eq!(
        search(&second, &replacement, json!({"query": "calendar"}))
            .await
            .unwrap()
            .discovered_tools()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn search_limits_results_and_preserves_codex_argument_errors() {
    let binding = binding(
        (0..12)
            .map(|i| {
                CatalogTool::new(&format!("event_{i}"), None, ToolExposure::Deferred)
                    as Arc<dyn Tool>
            })
            .collect(),
    );
    let prepared = prepare(&binding).await;
    assert_eq!(
        search(&prepared, &binding, json!({"query": "calendar"}))
            .await
            .unwrap()
            .discovered_tools()
            .unwrap()
            .len(),
        8
    );
    assert_eq!(
        search(
            &prepared,
            &binding,
            json!({"query": "  calendar  ", "limit": 1})
        )
        .await
        .unwrap()
        .discovered_tools()
        .unwrap()
        .len(),
        1
    );
    for (args, message) in [
        (json!({"query": "  "}), "query must not be empty"),
        (
            json!({"query": "calendar", "limit": 0}),
            "limit must be greater than zero",
        ),
    ] {
        assert!(
            search(&prepared, &binding, args)
                .await
                .unwrap_err()
                .to_string()
                .contains(message)
        );
    }
    for args in [
        json!({}),
        json!({"query": "calendar", "limit": -1}),
        json!({"query": "calendar", "limit": 1.5}),
    ] {
        assert!(search(&prepared, &binding, args).await.is_err());
    }
}

#[tokio::test]
async fn native_search_is_distinct_from_a_function_named_tool_search() {
    let binding = binding(vec![
        CatalogTool::new("lookup", None, ToolExposure::Deferred),
        CatalogTool::new("tool_search", None, ToolExposure::Advertised),
    ]);
    let prepared = prepare(&binding).await;
    for kind in [ToolCallKind::Function, ToolCallKind::ToolSearch] {
        let processed = process_model_response(
            &call("c", "tool_search", kind, None),
            prepared.action_surface(),
        )
        .unwrap();
        assert_eq!(
            processed.functions()[0]
                .tool()
                .model_definition()
                .kind()
                .is_tool_search(),
            kind == ToolCallKind::ToolSearch
        );
    }
}

#[tokio::test]
async fn deferred_top_level_synthetic_namespace_binds_only_its_own_tool() {
    let binding = binding(vec![CatalogTool::new(
        "lookup",
        None,
        ToolExposure::Deferred,
    )]);
    let prepared = prepare(&binding).await;
    let bare_call = call("bare", "lookup", ToolCallKind::Function, None);
    assert!(
        ra_core::step::ProcessedResponse::builder()
            .function(
                bare_call.output()[0].clone(),
                prepared.action_surface().tools()[0].clone(),
            )
            .is_err()
    );
    let processed = process_model_response(
        &call("c", "lookup", ToolCallKind::Function, Some("lookup")),
        prepared.action_surface(),
    )
    .unwrap();
    assert_eq!(processed.functions().len(), 1);
    let processed = process_model_response(
        &call("c", "lookup", ToolCallKind::Function, Some("other")),
        prepared.action_surface(),
    )
    .unwrap();
    assert_eq!(processed.tools_not_found().len(), 1);
}

#[tokio::test]
async fn a_serialized_search_approval_resumes_with_native_output_kind() {
    let binding = binding(vec![CatalogTool::new(
        "lookup",
        None,
        ToolExposure::Deferred,
    )]);
    let model = ScriptedModel::new(vec![call(
        "search",
        "tool_search",
        ToolCallKind::ToolSearch,
        None,
    )]);
    let request = |binding: AgentBinding, model: Arc<ScriptedModel>| {
        RunRequest::new(
            binding,
            Arc::new(Resolver(model)),
            RunId::new("run"),
            CancelScope::root(),
            vec![],
        )
        .with_config(RunConfig::new().with_permission_rules([
            PermissionRule::new(PermissionDecision::Ask).with_tool_name("tool_search"),
        ]))
    };
    let result = Runner::run(request(binding.clone(), model)).await.unwrap();
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("search should ask for approval");
    };
    let mut state: RunState =
        serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
    state.approve(&items[0], false).unwrap();
    let restored_model = ScriptedModel::new(vec![final_response()]);
    let resumed = Runner::run(
        request(
            AgentBinding::direct(binding.public().clone()),
            restored_model.clone(),
        )
        .with_state(state),
    )
    .await
    .unwrap();
    assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
    let requests = restored_model.requests.lock().unwrap();
    let output = requests[0]
        .input()
        .iter()
        .find_map(|item| match item {
            ra_core::item::ModelInputItem::ToolCallOutput(output) => Some(output),
            _ => None,
        })
        .unwrap();
    assert_eq!(output.kind(), ToolCallKind::ToolSearch);
    assert_eq!(
        ToolOutput::from_stored(output.output())
            .unwrap()
            .unwrap()
            .discovered_tools()
            .unwrap()[0]
            .name(),
        "lookup"
    );
}

/// The framework's stop policies apply to function-tool results; discovery still needs the model.
#[tokio::test]
async fn native_discovery_does_not_trigger_stop_on_first_function_tool() {
    let base = binding(vec![CatalogTool::new(
        "lookup",
        None,
        ToolExposure::Deferred,
    )]);
    let binding = AgentBinding::direct(
        base.public()
            .to_builder()
            .tool_use_behavior(ra_core::agent::ToolUseBehavior::StopOnFirstTool)
            .build()
            .unwrap(),
    );
    let model = ScriptedModel::new(vec![
        call("search", "tool_search", ToolCallKind::ToolSearch, None),
        final_response(),
    ]);
    let result = Runner::run(RunRequest::new(
        binding,
        Arc::new(Resolver(model.clone())),
        RunId::new("run"),
        CancelScope::root(),
        vec![],
    ))
    .await
    .unwrap();
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert!(result.final_message().is_some());
}

/// Ported lookup semantics: visible and deferred top-level peers with the same name are distinct.
#[tokio::test]
async fn bare_and_deferred_same_name_tools_coexist_and_dispatch_independently() {
    let deferred = CatalogTool::new("lookup", None, ToolExposure::Deferred);
    let visible = CatalogTool::new("lookup", None, ToolExposure::Advertised);
    let registry = ra_runtime::tool::registry::ToolRegistry::builder()
        .register(visible.clone() as Arc<dyn Tool>)
        .register(deferred.clone() as Arc<dyn Tool>)
        .build()
        .unwrap();
    let profile = ra_runtime::tool::profile::ToolProfile::builder(
        ra_runtime::tool::profile::ToolProfileId::new("all").unwrap(),
    )
    .all_registered()
    .budget(ra_runtime::tool::profile::ToolSurfaceBudget::new(0, usize::MAX).unwrap())
    .build()
    .unwrap();
    assert_eq!(registry.assemble(&profile).unwrap().advertised_count(), 1);
    let base = binding(vec![visible.clone()]);
    let external = base
        .with_external_tools([deferred.clone() as Arc<dyn Tool>])
        .unwrap();
    let declared = binding(vec![visible.clone(), deferred.clone()]);
    for binding in [external, declared] {
        let prepared = prepare(&binding).await;
        assert_eq!(prepared.request().tools().len(), 2);
        for (namespace, expected) in [(None, visible.clone()), (Some("lookup"), deferred.clone())] {
            let processed = process_model_response(
                &call("c", "lookup", ToolCallKind::Function, namespace),
                prepared.action_surface(),
            )
            .unwrap();
            assert!(Arc::ptr_eq(
                processed.functions()[0].tool(),
                &(expected as Arc<dyn Tool>)
            ));
        }
    }
    let only_deferred = binding(vec![deferred]);
    let prepared = prepare(&only_deferred).await;
    assert!(prepared.action_surface().find_tool("lookup").is_none());
}

#[tokio::test]
async fn native_search_then_synthetic_function_runs_to_final_output() {
    let binding = binding(vec![CatalogTool::new(
        "lookup",
        None,
        ToolExposure::Deferred,
    )]);
    let model = ScriptedModel::new(vec![
        call("search", "tool_search", ToolCallKind::ToolSearch, None),
        call("function", "lookup", ToolCallKind::Function, Some("lookup")),
        final_response(),
    ]);
    let result = Runner::run(RunRequest::new(
        binding,
        Arc::new(Resolver(model.clone())),
        RunId::new("run"),
        CancelScope::root(),
        vec![],
    ))
    .await
    .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(model.requests.lock().unwrap().len(), 3);
    assert!(result.new_items().iter().any(|item| match item.kind() {
        RunItemKind::ToolCallOutput(output) =>
            output.call_id().as_str() == "function" && !output.is_error(),
        _ => false,
    }));
}

/// A sticky answer is pinned to the identity it was given for. Native discovery and a function
/// named `tool_search` share a name, so an `always` approval of one must not approve the other.
#[tokio::test]
async fn sticky_search_approval_does_not_approve_a_function_named_tool_search() {
    let binding = binding(vec![
        CatalogTool::new("lookup", None, ToolExposure::Deferred),
        CatalogTool::new("tool_search", None, ToolExposure::Advertised),
    ]);
    let request = |binding: AgentBinding, model: Arc<ScriptedModel>| {
        RunRequest::new(
            binding,
            Arc::new(Resolver(model)),
            RunId::new("run"),
            CancelScope::root(),
            vec![],
        )
        .with_config(RunConfig::new().with_permission_rules([
            PermissionRule::new(PermissionDecision::Ask).with_tool_name("tool_search"),
        ]))
    };
    let first = ScriptedModel::new(vec![call(
        "native",
        "tool_search",
        ToolCallKind::ToolSearch,
        None,
    )]);
    let result = Runner::run(request(binding.clone(), first)).await.unwrap();
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("native search should ask for approval");
    };
    let mut state = result.state().clone();
    state.approve(&items[0], true).unwrap();

    let resumed_model = ScriptedModel::new(vec![
        call("function", "tool_search", ToolCallKind::Function, None),
        final_response(),
    ]);
    let resumed = Runner::run(request(binding, resumed_model).with_state(state))
        .await
        .unwrap();
    let RunOutcome::Interrupted { items } = resumed.outcome() else {
        panic!("the sticky search approval must not approve the function");
    };
    assert_eq!(items.len(), 1);
    let RunItemKind::ToolApproval(approval) = items[0].kind() else {
        panic!("expected a tool approval, got {:?}", items[0].kind());
    };
    assert_eq!(approval.call_id().as_str(), "function");
    assert_eq!(approval.kind(), ToolCallKind::Function);
}
