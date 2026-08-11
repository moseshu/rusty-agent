//! R3-0 contracts for the fixed preparation order before a model call.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope, ScopeKind},
    error::{Error, Result},
    item::{Message, ModelInputItem, ModelResponse},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelTracing, ProviderKey, ResolvedModel, ToolChoice,
    },
    tool::{
        Tool, ToolAvailability, ToolExposure, ToolInvocation, ToolOptions, ToolOrigin, ToolOutput,
        ToolRuntimeContext, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    turn::prepare::{TurnPreparationRequest, prepare_turn},
};
use serde_json::json;

type Events = Arc<Mutex<Vec<String>>>;

struct FakeModel;

#[async_trait]
impl Model for FakeModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller(
            "fake model is not invoked during preparation",
        ))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::empty().boxed()
    }
}

struct RecordingResolver {
    events: Events,
    resolved_names: Mutex<Vec<Option<String>>>,
}

impl RecordingResolver {
    fn new(events: Events) -> Self {
        Self {
            events,
            resolved_names: Mutex::new(Vec::new()),
        }
    }

    fn resolved_names(&self) -> Vec<Option<String>> {
        self.resolved_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ModelResolver for RecordingResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("resolve_model".to_owned());
        self.resolved_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(model_name.map(str::to_owned));

        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(FakeModel),
            ModelSettings::new()
                .with_temperature(0.1)
                .with_metadata("provider", "yes"),
            ModelSettings::new()
                .with_max_tokens(1_000)
                .with_metadata("model", "yes"),
        ))
    }
}

struct HostContext {
    dynamic_tools_enabled: bool,
}

struct RecordingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    events: Events,
    dynamic_result: Result<bool>,
}

impl RecordingTool {
    fn new(
        name: &str,
        availability: ToolAvailability,
        events: Events,
        dynamic_result: Result<bool>,
    ) -> Self {
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
            events,
            dynamic_result,
        }
    }

    fn with_options(mut self, options: ToolOptions) -> Self {
        self.options = options;
        self
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _invocation: ToolInvocation<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("unused"))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn is_enabled(&self, context: &dyn ToolRuntimeContext) -> Result<bool> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("is_enabled:{}", self.origin.name()));
        let host = context
            .as_any()
            .downcast_ref::<HostContext>()
            .ok_or_else(|| Error::caller("HostContext is required"))?;
        if !host.dynamic_tools_enabled {
            return Ok(false);
        }
        match &self.dynamic_result {
            Ok(enabled) => Ok(*enabled),
            Err(error) => Err(Error::caller(error.to_string())),
        }
    }
}

fn events(events: &Events) -> Vec<String> {
    events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn dynamic_tool(name: &str, events: &Events, enabled: bool) -> Arc<dyn Tool> {
    Arc::new(RecordingTool::new(
        name,
        ToolAvailability::Dynamic,
        Arc::clone(events),
        Ok(enabled),
    ))
}

/// Wraps a plain agent as the binding preparation and settlement take (R3-12). These tests are not
/// about a prepared instance, so both identities are the same object.
fn direct(agent: &Arc<AgentSpec>) -> AgentBinding {
    AgentBinding::direct(Arc::clone(agent))
}

fn host() -> HostContext {
    HostContext {
        dynamic_tools_enabled: true,
    }
}

#[tokio::test]
async fn 动态工具先于模型解析且最终快照同时驱动请求与执行绑定() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let enabled = dynamic_tool("dynamic_enabled", &event_log, true);
    let static_enabled: Arc<dyn Tool> = Arc::new(RecordingTool::new(
        "static_enabled",
        ToolAvailability::Enabled,
        Arc::clone(&event_log),
        Ok(true),
    ));
    let static_disabled: Arc<dyn Tool> = Arc::new(RecordingTool::new(
        "static_disabled",
        ToolAvailability::Disabled,
        Arc::clone(&event_log),
        Ok(true),
    ));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .instructions("Use only the enabled tool snapshot.")
        .model("agent/model")
        .tools([enabled, static_enabled, static_disabled])
        .model_settings(
            ModelSettings::new()
                .with_temperature(0.2)
                .with_metadata("agent", "yes"),
        )
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(Arc::clone(&event_log));
    let context = host();
    let cancel = CancelScope::root();
    let input = vec![ModelInputItem::Message(Message::user("hello"))];

    let prepared = prepare_turn(
        TurnPreparationRequest::new(&direct(&agent), &resolver, &context, &cancel, input.clone())
            .with_model("run/model")
            .with_model_settings(
                ModelSettings::new()
                    .with_temperature(0.3)
                    .with_max_tokens(2_000)
                    .with_tool_choice(ToolChoice::Required)
                    .with_metadata("run", "yes"),
            ),
    )
    .await
    .unwrap();

    assert_eq!(
        events(&event_log),
        ["is_enabled:dynamic_enabled", "resolve_model"]
    );
    assert_eq!(resolver.resolved_names(), [Some("run/model".to_owned())]);
    assert_eq!(prepared.selector().provider().as_str(), "test-provider");
    assert_eq!(prepared.selector().model(), Some("canonical-model"));

    let executable_names = prepared
        .tools()
        .iter()
        .map(|tool| tool.origin().name())
        .collect::<Vec<_>>();
    let advertised_names = prepared
        .request()
        .tools()
        .iter()
        .map(|tool| tool.name())
        .collect::<Vec<_>>();
    assert_eq!(executable_names, ["dynamic_enabled", "static_enabled"]);
    assert_eq!(advertised_names, executable_names);
    assert_eq!(prepared.request().input(), input);
    assert_eq!(
        prepared.request().system_instructions(),
        Some("Use only the enabled tool snapshot.")
    );

    let settings = prepared.request().model_settings();
    assert_eq!(settings.temperature(), Some(0.3));
    assert_eq!(settings.max_tokens(), Some(1_000));
    assert_eq!(settings.tool_choice(), Some(&ToolChoice::Required));
    assert_eq!(
        settings.metadata().get("provider").map(String::as_str),
        Some("yes")
    );
    assert_eq!(
        settings.metadata().get("agent").map(String::as_str),
        Some("yes")
    );
    assert_eq!(
        settings.metadata().get("model").map(String::as_str),
        Some("yes")
    );
    assert_eq!(
        settings.metadata().get("run").map(String::as_str),
        Some("yes")
    );
}

#[tokio::test]
async fn advertised_与_hidden_工具分别进入或离开模型面() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let advertised: Arc<dyn Tool> = Arc::new(RecordingTool::new(
        "advertised",
        ToolAvailability::Enabled,
        Arc::clone(&event_log),
        Ok(true),
    ));
    let hidden: Arc<dyn Tool> = Arc::new(
        RecordingTool::new(
            "hidden",
            ToolAvailability::Enabled,
            Arc::clone(&event_log),
            Ok(true),
        )
        .with_options(ToolOptions::new().with_exposure(ToolExposure::Hidden)),
    );
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([advertised, hidden])
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host();
    let cancel = CancelScope::root();

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        Vec::new(),
    ))
    .await
    .unwrap();

    let request_names = prepared
        .request()
        .tools()
        .iter()
        .map(|tool| tool.name())
        .collect::<Vec<_>>();
    let surface_names = prepared
        .tools()
        .iter()
        .map(|tool| tool.origin().name())
        .collect::<Vec<_>>();
    assert_eq!(request_names, ["advertised"]);
    assert_eq!(surface_names, ["advertised"]);
    assert!(prepared.action_surface().find_tool("hidden").is_none());
}

#[tokio::test]
async fn enabled_deferred_工具在_tool_search_落地前明确拒绝() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let deferred: Arc<dyn Tool> = Arc::new(
        RecordingTool::new(
            "deferred",
            ToolAvailability::Enabled,
            Arc::clone(&event_log),
            Ok(true),
        )
        .with_options(ToolOptions::new().with_exposure(ToolExposure::Deferred)),
    );
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([deferred])
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host();
    let cancel = CancelScope::root();

    let error = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        Vec::new(),
    ))
    .await
    .expect_err("deferred tools cannot silently disappear before tool_search exists");

    assert!(error.to_string().contains("Deferred exposure"));
    assert!(error.to_string().contains("R2-5c"));
}

#[tokio::test]
async fn 动态工具失败会在模型解析前终止准备() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let broken: Arc<dyn Tool> = Arc::new(RecordingTool::new(
        "broken",
        ToolAvailability::Dynamic,
        Arc::clone(&event_log),
        Err(Error::caller("dynamic availability failed")),
    ));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool(broken)
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(Arc::clone(&event_log));
    let context = host();
    let cancel = CancelScope::root();

    let error = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        Vec::new(),
    ))
    .await
    .unwrap_err();

    assert!(error.to_string().contains("dynamic availability failed"));
    assert_eq!(events(&event_log), ["is_enabled:broken"]);
    assert!(resolver.resolved_names().is_empty());
}

#[tokio::test]
async fn agent_模型在没有_run_override_时传给_resolver() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .model("agent/model")
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host();
    let cancel = CancelScope::root();

    prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        Vec::new(),
    ))
    .await
    .unwrap();

    assert_eq!(resolver.resolved_names(), [Some("agent/model".to_owned())]);
}

#[tokio::test]
async fn 没有模型选择器时_resolver_收到_none_走注册表默认() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host();
    let cancel = CancelScope::root();

    prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        Vec::new(),
    ))
    .await
    .unwrap();

    assert_eq!(resolver.resolved_names(), [None]);
}

#[tokio::test]
async fn 已取消的作用域不启动任何准备工作() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool(dynamic_tool("dynamic_enabled", &event_log, true))
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(Arc::clone(&event_log));
    let context = host();
    let cancel = CancelScope::root();
    cancel.cancel(CancelReason::UserInterrupt);

    let error = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        Vec::new(),
    ))
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Cancelled { .. }));
    assert!(events(&event_log).is_empty());
}

#[tokio::test]
async fn turn_作用域取消随父作用域传播进准备阶段() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool(dynamic_tool("dynamic_enabled", &event_log, true))
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(Arc::clone(&event_log));
    let context = host();
    let run = CancelScope::root();
    let turn = run.child(ScopeKind::Turn);
    run.cancel(CancelReason::Shutdown);

    let error = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &turn,
        Vec::new(),
    ))
    .await
    .unwrap_err();

    // `Error::Cancelled` carries human text only; machine attribution stays on the scope.
    assert!(matches!(error, Error::Cancelled { .. }));
    assert_eq!(turn.reason(), Some(CancelReason::Shutdown));
    assert!(events(&event_log).is_empty());
    assert!(resolver.resolved_names().is_empty());
}

#[tokio::test]
async fn 工具面被动态清空时_required_不会带着空工具表发出去() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool(dynamic_tool("sometimes_available", &event_log, false))
        .model_settings(
            ModelSettings::new()
                .with_tool_choice(ToolChoice::Required)
                .with_parallel_tool_calls(true),
        )
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(Arc::clone(&event_log));
    let context = host();
    let cancel = CancelScope::root();

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        Vec::new(),
    ))
    .await
    .unwrap();

    assert!(prepared.tools().is_empty());
    assert!(prepared.request().tools().is_empty());
    assert_eq!(prepared.request().model_settings().tool_choice(), None);
    assert_eq!(
        prepared.request().model_settings().parallel_tool_calls(),
        None
    );
}

#[tokio::test]
async fn 指名一个本轮未广播的工具时选择器降级而不是终止_run() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([
            dynamic_tool("still_here", &event_log, true),
            dynamic_tool("gone_this_turn", &event_log, false),
        ])
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(Arc::clone(&event_log));
    let context = host();
    let cancel = CancelScope::root();

    let pinned_to_live_tool = prepare_turn(
        TurnPreparationRequest::new(&direct(&agent), &resolver, &context, &cancel, Vec::new())
            .with_model_settings(
                ModelSettings::new().with_tool_choice(ToolChoice::Tool("still_here".to_owned())),
            ),
    )
    .await
    .unwrap();
    assert_eq!(
        pinned_to_live_tool.request().model_settings().tool_choice(),
        Some(&ToolChoice::Tool("still_here".to_owned()))
    );

    let pinned_to_disabled_tool = prepare_turn(
        TurnPreparationRequest::new(&direct(&agent), &resolver, &context, &cancel, Vec::new())
            .with_model_settings(
                ModelSettings::new()
                    .with_tool_choice(ToolChoice::Tool("gone_this_turn".to_owned())),
            ),
    )
    .await
    .unwrap();
    assert_eq!(
        pinned_to_disabled_tool
            .request()
            .model_settings()
            .tool_choice(),
        None
    );
    assert_eq!(pinned_to_disabled_tool.request().tools().len(), 1);
}

#[tokio::test]
async fn tracing_三态可从准备请求设置() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host();
    let cancel = CancelScope::root();

    let default = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        Vec::new(),
    ))
    .await
    .unwrap();
    assert!(default.request().tracing().is_disabled());

    let with_data = prepare_turn(
        TurnPreparationRequest::new(&direct(&agent), &resolver, &context, &cancel, Vec::new())
            .with_tracing(ModelTracing::Enabled),
    )
    .await
    .unwrap();
    assert!(with_data.request().tracing().include_data());

    let without_data = prepare_turn(
        TurnPreparationRequest::new(&direct(&agent), &resolver, &context, &cancel, Vec::new())
            .with_tracing(ModelTracing::EnabledWithoutData),
    )
    .await
    .unwrap();
    assert!(!without_data.request().tracing().is_disabled());
    assert!(!without_data.request().tracing().include_data());
}

#[tokio::test]
async fn into_request_交出请求所有权而不复制输入历史() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host();
    let cancel = CancelScope::root();
    let input = vec![ModelInputItem::Message(Message::user("hello"))];

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        input.clone(),
    ))
    .await
    .unwrap();

    let model = Arc::clone(prepared.model());
    let request = prepared.into_request();
    assert_eq!(request.input(), input);
    assert!(model.get_response(request).await.is_err());
}
