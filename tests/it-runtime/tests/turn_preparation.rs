//! R3-0 contracts for the fixed preparation order before a model call.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope, ScopeKind},
    context::RunContext,
    error::{Error, Result},
    item::{CallId, Message, ModelInputItem, ModelResponse},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelTracing, ProviderKey, ResolvedModel, ToolChoice,
    },
    state::{RunId, ToolUse, ToolUseAttempt, ToolUseTracker},
    tool::{
        Tool, ToolAvailability, ToolContext, ToolExposure, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema,
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

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("unused"))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn is_enabled(&self, context: &RunContext) -> Result<bool> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("is_enabled:{}", self.origin.name()));
        // Dynamic availability is the first stage that enters third-party code, and it reaches
        // host state the same way a running tool does: one checked read, through the run.
        let host = context
            .app_context::<HostContext>()
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

/// The live run context preparation is handed, carrying the host's own state.
fn host(agent: &Arc<AgentSpec>) -> RunContext {
    RunContext::new(RunId::new("run-preparation"), agent.as_ref()).with_app_context(Arc::new(
        HostContext {
            dynamic_tools_enabled: true,
        },
    ))
}

#[tokio::test]
async fn test_turn_preparation_01() {
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
    let context = host(&agent);
    let cancel = CancelScope::root();
    let input = vec![ModelInputItem::Message(Message::user("hello"))];

    let prepared = prepare_turn(
        TurnPreparationRequest::new(
            &direct(&agent),
            &resolver,
            &context,
            &cancel,
            &ToolUseTracker::new(),
            input.clone(),
        )
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
    assert_eq!(
        prepared
            .request()
            .cache_plan()
            .map(|plan| plan.prefix_hash().clone()),
        Some(ra_core::prompt::ContentHash::compute(
            "Use only the enabled tool snapshot."
        )),
        "the plan states what is stable; whether that span is worth caching is the adapter's call, \
         since only it sees the merged tool table"
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
async fn test_turn_preparation_02() {
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
    let context = host(&agent);
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
async fn test_turn_preparation_03() {
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
    let context = host(&agent);
    let cancel = CancelScope::root();

    let error = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .expect_err("deferred tools cannot silently disappear before tool_search exists");

    assert!(error.to_string().contains("Deferred exposure"));
    assert!(error.to_string().contains("R2-5c"));
}

#[tokio::test]
async fn test_turn_preparation_04() {
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
    let context = host(&agent);
    let cancel = CancelScope::root();

    let error = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap_err();

    assert!(error.to_string().contains("dynamic availability failed"));
    assert_eq!(events(&event_log), ["is_enabled:broken"]);
    assert!(resolver.resolved_names().is_empty());
}

#[tokio::test]
async fn test_turn_preparation_05() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .model("agent/model")
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host(&agent);
    let cancel = CancelScope::root();

    prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap();

    assert_eq!(resolver.resolved_names(), [Some("agent/model".to_owned())]);
}

#[tokio::test]
async fn test_turn_preparation_06() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host(&agent);
    let cancel = CancelScope::root();

    prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap();

    assert_eq!(resolver.resolved_names(), [None]);
}

#[tokio::test]
async fn test_turn_preparation_07() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool(dynamic_tool("dynamic_enabled", &event_log, true))
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(Arc::clone(&event_log));
    let context = host(&agent);
    let cancel = CancelScope::root();
    cancel.cancel(CancelReason::UserInterrupt);

    let error = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Cancelled { .. }));
    assert!(events(&event_log).is_empty());
}

#[tokio::test]
async fn test_turn_preparation_08() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool(dynamic_tool("dynamic_enabled", &event_log, true))
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(Arc::clone(&event_log));
    let context = host(&agent);
    let run = CancelScope::root();
    let turn = run.child(ScopeKind::Turn);
    run.cancel(CancelReason::Shutdown);

    let error = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &turn,
        &ToolUseTracker::new(),
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
async fn test_turn_preparation_09() {
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
    let context = host(&agent);
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

    assert!(prepared.tools().is_empty());
    assert!(prepared.request().tools().is_empty());
    assert_eq!(prepared.request().model_settings().tool_choice(), None);
    assert_eq!(
        prepared.request().model_settings().parallel_tool_calls(),
        None
    );
}

#[tokio::test]
async fn test_turn_preparation_10() {
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
    let context = host(&agent);
    let cancel = CancelScope::root();

    let pinned_to_live_tool = prepare_turn(
        TurnPreparationRequest::new(
            &direct(&agent),
            &resolver,
            &context,
            &cancel,
            &ToolUseTracker::new(),
            Vec::new(),
        )
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
        TurnPreparationRequest::new(
            &direct(&agent),
            &resolver,
            &context,
            &cancel,
            &ToolUseTracker::new(),
            Vec::new(),
        )
        .with_model_settings(
            ModelSettings::new().with_tool_choice(ToolChoice::Tool("gone_this_turn".to_owned())),
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
async fn test_turn_preparation_11() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host(&agent);
    let cancel = CancelScope::root();

    let default = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap();
    assert!(default.request().tracing().is_disabled());

    let with_data = prepare_turn(
        TurnPreparationRequest::new(
            &direct(&agent),
            &resolver,
            &context,
            &cancel,
            &ToolUseTracker::new(),
            Vec::new(),
        )
        .with_tracing(ModelTracing::Enabled),
    )
    .await
    .unwrap();
    assert!(with_data.request().tracing().include_data());

    let without_data = prepare_turn(
        TurnPreparationRequest::new(
            &direct(&agent),
            &resolver,
            &context,
            &cancel,
            &ToolUseTracker::new(),
            Vec::new(),
        )
        .with_tracing(ModelTracing::EnabledWithoutData),
    )
    .await
    .unwrap();
    assert!(!without_data.request().tracing().is_disabled());
    assert!(!without_data.request().tracing().include_data());
}

#[tokio::test]
async fn test_turn_preparation_12() {
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(event_log);
    let context = host(&agent);
    let cancel = CancelScope::root();
    let input = vec![ModelInputItem::Message(Message::user("hello"))];

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        input.clone(),
    ))
    .await
    .unwrap();

    let model = Arc::clone(prepared.model());
    let request = prepared.into_request();
    assert_eq!(request.input(), input);
    assert!(model.get_response(request).await.is_err());
}

#[tokio::test]
async fn test_turn_preparation_13() {
    // The release reads the agent's own record and applies to the resolved value. Both halves
    // matter: a selection released for the agent that complied must not follow the run into
    // another agent's turn, and one that a *layer* was rewritten with could never be un-forced.
    let event_log = Arc::new(Mutex::new(Vec::new()));
    let worker = AgentId::new("worker");
    let agent = AgentSpec::builder()
        .id(worker.clone())
        .name("Worker")
        .tool(dynamic_tool("write_file", &event_log, true))
        .model_settings(ModelSettings::new().with_tool_choice(ToolChoice::Required))
        .build()
        .unwrap();
    let resolver = RecordingResolver::new(Arc::clone(&event_log));
    let context = host(&agent);
    let cancel = CancelScope::root();

    let untouched = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap();
    assert_eq!(
        untouched.request().model_settings().tool_choice(),
        Some(&ToolChoice::Required)
    );

    let mut tool_use = ToolUseTracker::new();
    tool_use.record_turn(&worker, [write_file_attempt("call-1")]);

    let released = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &tool_use,
        Vec::new(),
    ))
    .await
    .unwrap();
    assert_eq!(released.request().model_settings().tool_choice(), None);

    // A different agent complied; this one still owes its forced call.
    let mut other_agent_only = ToolUseTracker::new();
    other_agent_only.record_turn(&AgentId::new("reviewer"), [write_file_attempt("call-2")]);

    let still_forced = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &other_agent_only,
        Vec::new(),
    ))
    .await
    .unwrap();
    assert_eq!(
        still_forced.request().model_settings().tool_choice(),
        Some(&ToolChoice::Required)
    );
}

#[tokio::test]
async fn test_dynamic_instructions_lowering_to_tail_items() {
    let event_log: Events = Arc::new(Mutex::new(Vec::new()));
    let resolver = RecordingResolver::new(Arc::clone(&event_log));

    let agent = AgentSpec::builder()
        .id(AgentId::new("dynamic-agent"))
        .name("Dynamic Agent")
        .dynamic_instructions_fn(|ctx| {
            let run_id = ctx.run_id().as_str().to_string();
            async move {
                Ok(ra_core::prompt::ResolvedPrompt::new(
                    format!("Turn-specific volatile data for run {run_id}"),
                    ra_core::prompt::PromptSource::Agent,
                ))
            }
        })
        .build()
        .unwrap();

    let context = host(&agent);
    let cancel = CancelScope::root();

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        vec![ModelInputItem::Message(Message::user("Hello"))],
    ))
    .await
    .unwrap();

    // System instructions slot remains None: a generated prompt never reaches the cached prefix.
    assert_eq!(prepared.request().system_instructions(), None);

    // Input items have user message + dynamic tail item
    assert_eq!(prepared.request().input().len(), 2);
    let tail = &prepared.request().input()[1];
    if let ModelInputItem::Message(msg) = tail {
        assert_eq!(msg.role(), ra_core::item::MessageRole::User);
        assert_eq!(
            msg.content()[0].as_text(),
            Some("Turn-specific volatile data for run run-preparation")
        );
    } else {
        panic!("expected message input item for dynamic tail");
    }

    // The text is in the request; the record of what produced it travels alongside.
    let provenance = prepared
        .instruction_provenance()
        .expect("a generated prompt must leave a provenance record");
    assert_eq!(provenance.source(), &ra_core::prompt::PromptSource::Agent);
    assert_eq!(
        provenance.content_hash(),
        &ra_core::prompt::ContentHash::compute(
            "Turn-specific volatile data for run run-preparation"
        ),
        "the recorded hash must cover the text that was actually sent"
    );
}

/// A generated prompt asking for the prefix is rejected, not silently relocated or dropped.
///
/// The prefix is the cached span. A generator reads the run, so anything it writes there changes
/// every turn — this is the case that would quietly take prompt caching to a zero hit rate while
/// every test still passed.
#[tokio::test]
async fn test_dynamic_instructions_cannot_reach_the_stable_prefix() {
    let event_log: Events = Arc::new(Mutex::new(Vec::new()));
    let resolver = RecordingResolver::new(Arc::clone(&event_log));

    let agent = AgentSpec::builder()
        .id(AgentId::new("prefix-grabbing-agent"))
        .name("Prefix Grabbing Agent")
        .dynamic_instructions_fn(|_| async move {
            let prefix_section = ra_core::prompt::PromptSection::new(
                ra_core::prompt::PromptSectionName::CORE_BEHAVIOR,
                "Stable prefix constitution",
                ra_core::prompt::PromptSource::Agent,
                ra_core::prompt::SectionStability::Stable,
                ra_core::prompt::SectionPosition::Prefix,
                "Stable constitution text",
            )?;

            Ok(ra_core::prompt::ResolvedPrompt::new(
                "Full text representation",
                ra_core::prompt::PromptSource::Agent,
            )
            .with_sections(vec![prefix_section]))
        })
        .build()
        .unwrap();

    let context = host(&agent);
    let cancel = CancelScope::root();

    let err = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        vec![ModelInputItem::Message(Message::user("Hello"))],
    ))
    .await
    .unwrap_err();

    let message = err.to_string();
    assert!(
        message.contains("prefix-grabbing-agent"),
        "the error must name the agent whose generator misplaced the section: {message}"
    );
    assert!(
        message.contains("core_behavior"),
        "the error must name the offending section: {message}"
    );
}

/// Every tail section a generator emits reaches the model, in order.
#[tokio::test]
async fn test_dynamic_instructions_lower_every_tail_section() {
    let event_log: Events = Arc::new(Mutex::new(Vec::new()));
    let resolver = RecordingResolver::new(Arc::clone(&event_log));

    let agent = AgentSpec::builder()
        .id(AgentId::new("structured-dynamic-agent"))
        .name("Structured Dynamic Agent")
        .dynamic_instructions_fn(|_| async move {
            let first = ra_core::prompt::PromptSection::new(
                ra_core::prompt::PromptSectionName::new("volatile_delta"),
                "Volatile tail delta",
                ra_core::prompt::PromptSource::Dynamic("delta".into()),
                ra_core::prompt::SectionStability::Volatile,
                ra_core::prompt::SectionPosition::TailMessage,
                "Volatile delta update",
            )?;

            let second = ra_core::prompt::PromptSection::new(
                ra_core::prompt::PromptSectionName::new("volatile_status"),
                "Volatile tail status",
                ra_core::prompt::PromptSource::Dynamic("status".into()),
                ra_core::prompt::SectionStability::Volatile,
                ra_core::prompt::SectionPosition::TailMessage,
                "Volatile status update",
            )?;

            Ok(ra_core::prompt::ResolvedPrompt::new(
                "Full text representation",
                ra_core::prompt::PromptSource::Agent,
            )
            .with_sections(vec![first, second]))
        })
        .build()
        .unwrap();

    let context = host(&agent);
    let cancel = CancelScope::root();

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        vec![ModelInputItem::Message(Message::user("Hello"))],
    ))
    .await
    .unwrap();

    assert_eq!(prepared.request().system_instructions(), None);
    assert_eq!(prepared.request().input().len(), 3);

    let texts: Vec<Option<&str>> = prepared.request().input()[1..]
        .iter()
        .map(|item| match item {
            ModelInputItem::Message(msg) => msg.content()[0].as_text(),
            _ => panic!("expected message input items for dynamic tail"),
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            Some("Volatile delta update"),
            Some("Volatile status update")
        ]
    );
    let provenance = prepared
        .instruction_provenance()
        .expect("structured dynamic prompt must retain provenance");
    assert_eq!(
        provenance.content_hash(),
        &ra_core::prompt::ContentHash::compute("Volatile delta update\n\nVolatile status update")
    );
    assert_ne!(
        provenance.content_hash(),
        &ra_core::prompt::ContentHash::compute("Full text representation")
    );
}

/// A failing generator keeps its own classification, and still names the agent.
///
/// Adding "which agent" used to mean rebuilding the error as a configuration error, which turned
/// every transient generator failure into one needing a human. Retry and model fallback read
/// `recoverability()` alone, so the rewrite silently disabled both.
#[tokio::test]
async fn test_dynamic_instructions_failure_keeps_its_classification() {
    let event_log: Events = Arc::new(Mutex::new(Vec::new()));
    let resolver = RecordingResolver::new(Arc::clone(&event_log));

    let agent = AgentSpec::builder()
        .id(AgentId::new("failing-dynamic-agent"))
        .name("Failing Dynamic Agent")
        .dynamic_instructions_fn(|_| async move {
            Err(Error::provider(
                ra_core::error::ProviderErrorKind::Timeout,
                "custom generator network timeout",
            ))
        })
        .build()
        .unwrap();

    let context = host(&agent);
    let cancel = CancelScope::root();

    let err = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap_err();

    let err_msg = err.to_string();
    assert!(
        err_msg.contains("failing-dynamic-agent"),
        "error message must name the failing agent public id: {err_msg}"
    );
    assert!(
        err_msg.contains("custom generator network timeout"),
        "the generator's own message must survive: {err_msg}"
    );
    assert_eq!(
        err.recoverability(),
        ra_core::error::Recoverability::Retryable,
        "a transient generator failure must stay retryable rather than become a config error"
    );
}

/// Cancelling during generation reports as a cancellation, not as a configuration error.
///
/// The cancellation contract permits exactly one test for "was this cancelled": `is_cancelled()`.
/// A stage that rebuilds the error into another variant makes that test answer `false`, and the
/// run is then counted as a failure and considered for retry.
#[tokio::test]
async fn test_cancel_during_dynamic_instructions_stays_a_cancellation() {
    let event_log: Events = Arc::new(Mutex::new(Vec::new()));
    let resolver = RecordingResolver::new(Arc::clone(&event_log));

    let agent = AgentSpec::builder()
        .id(AgentId::new("slow-dynamic-agent"))
        .name("Slow Dynamic Agent")
        .dynamic_instructions_fn(|_| async move {
            std::future::pending::<()>().await;
            unreachable!("the scope cancels before the generator completes")
        })
        .build()
        .unwrap();

    let context = host(&agent);
    let cancel = CancelScope::root();

    let canceller = {
        let scope = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            scope.cancel(CancelReason::UserInterrupt);
        })
    };

    let err = prepare_turn(TurnPreparationRequest::new(
        &direct(&agent),
        &resolver,
        &context,
        &cancel,
        &ToolUseTracker::new(),
        Vec::new(),
    ))
    .await
    .unwrap_err();
    canceller.await.unwrap();

    assert!(
        err.is_cancelled(),
        "a cancelled generation must answer `true` to is_cancelled(), got `{}`",
        err.code()
    );
    assert_eq!(
        err.recoverability(),
        ra_core::error::Recoverability::Cancelled
    );
    assert_eq!(cancel.reason(), Some(CancelReason::UserInterrupt));
}

fn write_file_attempt(call_id: &str) -> ToolUseAttempt {
    ToolUseAttempt::new(
        ToolUse::Tool(ToolOrigin::new("write_file").unwrap().lookup_key().clone()),
        CallId::new(call_id),
        &json!({ "path": "a.txt" }),
    )
}

/// A cacheable static prefix carries a plan whose hash and scope match what is being sent.
///
/// The scope matters as much as the hash: a key that changed per turn would partition the cache
/// instead of sharing it, which reads as "caching is enabled" while never hitting.
#[tokio::test]
async fn test_cacheable_prefix_carries_a_run_scoped_cache_plan() {
    let event_log: Events = Arc::new(Mutex::new(Vec::new()));
    let resolver = RecordingResolver::new(Arc::clone(&event_log));

    let instructions = "You are an autonomous engineering assistant. ".repeat(150);
    let agent = AgentSpec::builder()
        .id(AgentId::new("cacheable-agent"))
        .name("Cacheable Agent")
        .instructions(instructions.clone())
        .build()
        .unwrap();

    let context = host(&agent);
    let cancel = CancelScope::root();

    let binding = direct(&agent);
    let tool_use = ToolUseTracker::new();
    let prepare = || {
        prepare_turn(TurnPreparationRequest::new(
            &binding,
            &resolver,
            &context,
            &cancel,
            &tool_use,
            Vec::new(),
        ))
    };

    let prepared = prepare().await.unwrap();
    let plan = prepared
        .request()
        .cache_plan()
        .expect("a cacheable prefix on a caching protocol carries a plan");

    assert_eq!(
        plan.prefix_hash(),
        &ra_core::prompt::ContentHash::compute(&instructions),
        "the plan must name the instructions the request actually carries"
    );
    assert_eq!(
        plan.cache_scope(),
        Some("run-preparation"),
        "the cache scope is the run, so every turn of one run shares a cache entry"
    );

    // The scope is stable across turns; that is the whole point of naming one.
    let second = prepare().await.unwrap();
    assert_eq!(
        second.request().cache_plan().and_then(|p| p.cache_scope()),
        Some("run-preparation")
    );
    assert_eq!(
        second.request().cache_plan().map(|p| p.prefix_hash()),
        prepared.request().cache_plan().map(|p| p.prefix_hash())
    );
}

/// Short instructions still reach the model with a cache plan attached.
///
/// Preparation deliberately makes no judgement about whether caching is worthwhile: the cached
/// prefix is the instructions plus the whole tool table, and hosted tools do not exist until the
/// adapter merges them. An earlier version decided here, from the instructions alone, and so
/// dropped the plan for exactly the request shape that benefits most — a small instruction block in
/// front of a large tool table.
#[tokio::test]
async fn test_short_instructions_with_a_large_tool_table_still_carry_a_cache_plan() {
    let event_log: Events = Arc::new(Mutex::new(Vec::new()));
    let resolver = RecordingResolver::new(Arc::clone(&event_log));

    let instructions = "Be helpful.";
    let mut builder = AgentSpec::builder()
        .id(AgentId::new("small-prompt-agent"))
        .name("Small Prompt Agent")
        .instructions(instructions);
    for index in 0..12 {
        builder = builder.tool(Arc::new(RecordingTool::new(
            &format!("tool_{index}"),
            ToolAvailability::Enabled,
            Arc::clone(&event_log),
            Ok(true),
        )));
    }
    let agent = builder.build().unwrap();

    let context = host(&agent);
    let cancel = CancelScope::root();
    let binding = direct(&agent);
    let tool_use = ToolUseTracker::new();

    let prepared = prepare_turn(TurnPreparationRequest::new(
        &binding,
        &resolver,
        &context,
        &cancel,
        &tool_use,
        Vec::new(),
    ))
    .await
    .unwrap();

    assert!(
        ra_core::prompt::estimate_tokens(instructions)
            < ra_core::prompt::MIN_CACHEABLE_PREFIX_TOKENS,
        "the fixture instructions must be below the floor for this test to mean anything"
    );
    let plan = prepared
        .request()
        .cache_plan()
        .expect("preparation must not withhold a plan over the instructions' own length");
    assert_eq!(
        plan.prefix_hash(),
        &ra_core::prompt::ContentHash::compute(instructions),
        "the plan must name the instructions the request actually carries"
    );
    assert_eq!(plan.cache_scope(), Some("run-preparation"));
    assert_eq!(prepared.request().tools().len(), 12);
}
