//! Contracts for validating an installed capability set and folding it into one agent.

#[path = "capability_assembly/handoff.rs"]
mod handoff;

use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentInstructions, AgentSpec, ResolvedInstructions},
    cancel::CancelScope,
    capability::{
        Capability, CapabilityFamily, ContextProcessor, ContextProcessorRequest,
        ContextProcessorResult, ContextSummarizer, LoadSignal, SamplingContext,
    },
    context::RunContext,
    error::Result,
    item::{
        CallId, ItemId, Message, MessageRole, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    prompt::{
        PromptSection, PromptSectionName, PromptSource, ResolvedPrompt, SectionPosition,
        SectionStability,
    },
    state::RunId,
    tool::{Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::{
    agent::AgentBinding,
    capability::CapabilityPlan,
    runner::{RunConfig, RunRequest, Runner},
};
use serde_json::json;

// -- fixtures -----------------------------------------------------------------------------------

/// A capability assembled from parts, so one fixture covers every contribution a case needs.
struct TestCapability {
    kind: CapabilityFamily,
    requires: BTreeSet<CapabilityFamily>,
    tools: Vec<Arc<dyn Tool>>,
    section: Option<PromptSection>,
    static_section: Option<PromptSection>,
    static_resolutions: Arc<AtomicUsize>,
    deferred_section: Option<PromptSection>,
    /// How many times the deferred fragment's source material was read.
    deferred_resolutions: Arc<AtomicUsize>,
    temperature: Option<f64>,
    /// What the sampling fold handed this capability, recorded so order is observable.
    observed_temperature: Arc<Mutex<Option<f64>>>,
    processor: bool,
    /// Set when this capability supplies a per-run form instead of serving the run itself.
    binds: bool,
    /// The family a bound form reports, when it disagrees with the installed one.
    bound_kind: Option<CapabilityFamily>,
    bound_tools: Vec<Arc<dyn Tool>>,
    bound_to: Arc<Mutex<Vec<String>>>,
}

impl TestCapability {
    fn new(kind: CapabilityFamily) -> Self {
        Self {
            kind,
            requires: BTreeSet::new(),
            tools: Vec::new(),
            section: None,
            static_section: None,
            static_resolutions: Arc::new(AtomicUsize::new(0)),
            deferred_section: None,
            deferred_resolutions: Arc::new(AtomicUsize::new(0)),
            temperature: None,
            observed_temperature: Arc::new(Mutex::new(None)),
            processor: false,
            binds: false,
            bound_kind: None,
            bound_tools: Vec::new(),
            bound_to: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requiring(mut self, family: CapabilityFamily) -> Self {
        self.requires.insert(family);
        self
    }

    fn with_tool(mut self, name: &str) -> Self {
        self.tools.push(Arc::new(NamedTool::new(name)));
        self
    }

    fn with_section(mut self, name: &str, content: &str) -> Self {
        self.section = Some(
            PromptSection::new(
                PromptSectionName::new(name.to_owned()),
                "capability fragment",
                self.kind.prompt_source(),
                SectionStability::Stable,
                SectionPosition::Prefix,
                content,
            )
            .unwrap(),
        );
        self
    }

    fn with_static_section(mut self, name: &str, content: &str) -> Self {
        self.static_section = Some(
            PromptSection::new(
                PromptSectionName::new(name.to_owned()),
                "static capability fragment",
                self.kind.prompt_source(),
                SectionStability::Stable,
                SectionPosition::Prefix,
                content,
            )
            .unwrap(),
        );
        self
    }

    fn with_section_source(mut self, source: PromptSource) -> Self {
        self.section = Some(
            PromptSection::new(
                self.kind.prompt_section_name(),
                "capability fragment",
                source,
                SectionStability::Stable,
                SectionPosition::Prefix,
                "capability prompt text",
            )
            .unwrap(),
        );
        self
    }

    fn with_tail_section(mut self, name: &str) -> Self {
        self.section = Some(
            PromptSection::new(
                PromptSectionName::new(name.to_owned()),
                "capability fragment",
                self.kind.prompt_source(),
                SectionStability::Volatile,
                SectionPosition::TailMessage,
                "the date is today",
            )
            .unwrap(),
        );
        self
    }

    fn with_static_tail_section(mut self, name: &str) -> Self {
        self.static_section = Some(
            PromptSection::new(
                PromptSectionName::new(name.to_owned()),
                "static capability fragment",
                self.kind.prompt_source(),
                SectionStability::Volatile,
                SectionPosition::TailMessage,
                "the date is today",
            )
            .unwrap(),
        );
        self
    }

    fn with_deferred_section(mut self, content: &str) -> Self {
        self.deferred_section = Some(
            PromptSection::new(
                self.kind.deferred_prompt_section_name(),
                "deferred capability fragment",
                self.kind.prompt_source(),
                SectionStability::Stable,
                SectionPosition::TailMessage,
                content,
            )
            .unwrap(),
        );
        self
    }

    /// A deferred fragment claiming a name other than its family's deferred one.
    fn with_deferred_section_named(mut self, name: &str) -> Self {
        self.deferred_section = Some(
            PromptSection::new(
                PromptSectionName::new(name.to_owned()),
                "deferred capability fragment",
                self.kind.prompt_source(),
                SectionStability::Stable,
                SectionPosition::TailMessage,
                "deferred capability text",
            )
            .unwrap(),
        );
        self
    }

    /// A deferred fragment asking to be resident after all.
    fn with_deferred_section_in_prefix(mut self) -> Self {
        self.deferred_section = Some(
            PromptSection::new(
                self.kind.deferred_prompt_section_name(),
                "deferred capability fragment",
                self.kind.prompt_source(),
                SectionStability::Stable,
                SectionPosition::Prefix,
                "deferred capability text",
            )
            .unwrap(),
        );
        self
    }

    fn deferred_resolutions(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.deferred_resolutions)
    }

    fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    fn with_processor(mut self) -> Self {
        self.processor = true;
        self
    }

    fn binding(mut self) -> Self {
        self.binds = true;
        self
    }

    fn binding_as(mut self, kind: CapabilityFamily) -> Self {
        self.binds = true;
        self.bound_kind = Some(kind);
        self
    }

    fn binding_with_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.binds = true;
        self.bound_tools = tools;
        self
    }

    fn into_shared(self) -> Arc<dyn Capability> {
        Arc::new(self)
    }
}

#[async_trait]
impl Capability for TestCapability {
    fn kind(&self) -> CapabilityFamily {
        self.kind.clone()
    }

    fn required_capabilities(&self) -> BTreeSet<CapabilityFamily> {
        self.requires.clone()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    async fn instructions(&self) -> Result<Option<PromptSection>> {
        Ok(self.section.clone())
    }

    async fn static_instructions(&self) -> Result<Option<PromptSection>> {
        self.static_resolutions.fetch_add(1, Ordering::SeqCst);
        Ok(self.static_section.clone())
    }

    // Deliberately no `wants_deferred_instructions` override: the cases below exercise the trait's
    // own default, which is what a capability that adds a tool gets without writing anything.
    async fn deferred_instructions(&self) -> Result<Option<PromptSection>> {
        self.deferred_resolutions.fetch_add(1, Ordering::SeqCst);
        Ok(self.deferred_section.clone())
    }

    fn sampling_params(&self, settings: ModelSettings) -> ModelSettings {
        *self.observed_temperature.lock().unwrap() = settings.temperature();
        match self.temperature {
            Some(temperature) => settings.with_temperature(temperature),
            None => settings,
        }
    }

    fn context_processor(&self) -> Option<&dyn ContextProcessor> {
        self.processor.then_some(self as &dyn ContextProcessor)
    }

    fn bind(&self, context: &RunContext) -> Result<Option<Arc<dyn Capability>>> {
        if !self.binds {
            return Ok(None);
        }
        self.bound_to
            .lock()
            .unwrap()
            .push(context.run_id().as_str().to_owned());
        Ok(Some(Arc::new(BoundCapability {
            kind: self.bound_kind.clone().unwrap_or_else(|| self.kind.clone()),
            run_id: context.run_id().as_str().to_owned(),
            tools: self.bound_tools.clone(),
        })))
    }
}

#[async_trait]
impl ContextProcessor for TestCapability {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _summarizer: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        let mut input = request.input().to_vec();
        input.push(ModelInputItem::Message(Message::system(format!(
            "processed by {}",
            self.kind
        ))));
        Ok(ContextProcessorResult::new(input))
    }
}

/// The per-run form a binding capability supplies, which reports the run it was given.
struct BoundCapability {
    kind: CapabilityFamily,
    run_id: String,
    tools: Vec<Arc<dyn Tool>>,
}

impl Capability for BoundCapability {
    fn kind(&self) -> CapabilityFamily {
        self.kind.clone()
    }

    fn sampling_params(&self, settings: ModelSettings) -> ModelSettings {
        settings.with_metadata("bound_run", self.run_id.clone())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

struct NamedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    output: &'static str,
}

impl NamedTool {
    fn new(name: &str) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false,
                }),
            )
            .unwrap(),
            options: ToolOptions::new(),
            output: "done",
        }
    }
}

#[async_trait]
impl Tool for NamedTool {
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
        Ok(ToolOutput::text(self.output))
    }
}

/// Answers one turn and records the surface it was asked with.
struct RecordingModel {
    calls: AtomicUsize,
    instructions: Mutex<Vec<Option<String>>>,
    tools: Mutex<Vec<Vec<String>>>,
    temperatures: Mutex<Vec<Option<f64>>>,
    inputs: Mutex<Vec<Vec<ModelInputItem>>>,
    /// One tool name per turn to call before answering; turns past the end answer for good.
    script: Mutex<VecDeque<String>>,
}

impl RecordingModel {
    fn new() -> Arc<Self> {
        Self::scripted(&[])
    }

    /// Answers with one tool call per scripted name, then with a final message.
    fn scripted(calls: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            instructions: Mutex::new(Vec::new()),
            tools: Mutex::new(Vec::new()),
            temperatures: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
            script: Mutex::new(calls.iter().map(|name| (*name).to_owned()).collect()),
        })
    }

    /// Every message the model was sent on one turn, in order.
    fn messages(&self, turn: usize) -> Vec<String> {
        self.inputs.lock().unwrap()[turn]
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::Message(message) => Some(message.text_content()),
                _ => None,
            })
            .collect()
    }

    fn answer(&self, request: &ModelRequest) -> ModelResponse {
        let turn = self.calls.fetch_add(1, Ordering::SeqCst);
        self.instructions
            .lock()
            .unwrap()
            .push(request.system_instructions().map(str::to_owned));
        self.tools.lock().unwrap().push(
            request
                .tools()
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect(),
        );
        self.temperatures
            .lock()
            .unwrap()
            .push(request.model_settings().temperature());
        self.inputs.lock().unwrap().push(request.input().to_vec());
        let scripted = self.script.lock().unwrap().pop_front();
        match scripted {
            Some(name) => ModelResponse::new(vec![RunItem::new(
                ItemId::new(format!("call-{turn}")),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new(format!("call-{turn}")),
                    name,
                    json!({}),
                )),
            )]),
            None => ModelResponse::new(vec![RunItem::new(
                ItemId::new(format!("msg-{turn}")),
                RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
            )]),
        }
    }
}

#[async_trait]
impl Model for RecordingModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        Ok(self.answer(&request))
    }

    fn stream_response(&self, request: ModelRequest) -> ModelStream<'_> {
        let response = self.answer(&request);
        stream::iter(vec![Ok(ModelStreamEvent::Completed(Box::new(response)))]).boxed()
    }
}

struct FixedResolver {
    model: Arc<RecordingModel>,
}

impl ModelResolver for FixedResolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.model) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .instructions("do the thing")
        .model_settings(ModelSettings::new().with_max_tokens(64))
        .build()
        .unwrap()
}

fn run_context(run_id: &str) -> RunContext {
    RunContext::new(RunId::new(run_id), &agent())
}

fn families(plan: &CapabilityPlan) -> Vec<String> {
    plan.families()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
}

fn run_request(model: &Arc<RecordingModel>, config: RunConfig, cancel: &CancelScope) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(agent()),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
        }),
        RunId::new("run-capabilities"),
        cancel.clone(),
        vec![ModelInputItem::Message(Message::user("go"))],
    )
    .with_config(config)
}

// -- validation and order -----------------------------------------------------------------------

#[test]
fn a_declared_dependency_does_not_move_the_capability_that_declared_it() {
    let plan = CapabilityPlan::resolve([
        TestCapability::new(CapabilityFamily::MEMORY)
            .requiring(CapabilityFamily::SHELL)
            .into_shared(),
        TestCapability::new(CapabilityFamily::SHELL).into_shared(),
    ])
    .unwrap();

    assert_eq!(
        families(&plan),
        vec!["memory", "shell"],
        "a dependency asks for `shell` to be installed, not for it to be installed first. Deriving \
         an order from the declaration was removed once it turned out nothing declares one; the \
         host decides the fold by the order it writes the installs in"
    );
}

#[test]
fn capabilities_are_folded_in_the_order_the_host_installed_them() {
    let plan = CapabilityPlan::resolve([
        TestCapability::new(CapabilityFamily::TODO).into_shared(),
        TestCapability::new(CapabilityFamily::WEB)
            .requiring(CapabilityFamily::FILESYSTEM)
            .into_shared(),
        TestCapability::new(CapabilityFamily::FILESYSTEM).into_shared(),
        TestCapability::new(CapabilityFamily::SEARCH).into_shared(),
    ])
    .unwrap();

    assert_eq!(
        families(&plan),
        vec!["todo", "web", "filesystem", "search"],
        "every capability keeps its place, including the one whose dependency is installed after \
         it: the declaration is satisfied by presence"
    );
}

#[test]
fn a_dependency_nothing_installs_names_both_sides_and_reports_every_one_at_once() {
    let error = CapabilityPlan::resolve([
        TestCapability::new(CapabilityFamily::MEMORY)
            .requiring(CapabilityFamily::SHELL)
            .into_shared(),
        TestCapability::new(CapabilityFamily::SKILLS)
            .requiring(CapabilityFamily::FILESYSTEM)
            .into_shared(),
    ])
    .unwrap_err();

    let message = error.to_string();
    for expected in [
        "`memory` requires `shell`",
        "`skills` requires `filesystem`",
    ] {
        assert!(
            message.contains(expected),
            "the error must say which capability needs what, and say it for the whole set at once \
             so the fix is one edit rather than one run per missing family; got: {message}"
        );
    }
}

#[test]
fn one_family_may_name_only_one_installed_capability() {
    let error = CapabilityPlan::resolve([
        TestCapability::new(CapabilityFamily::SHELL).into_shared(),
        TestCapability::new(CapabilityFamily::SHELL).into_shared(),
    ])
    .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("`shell`") && message.contains("plugin.shell"),
        "a dependency names a family rather than an object, so two capabilities sharing one family \
         has to be refused with the way out named; got: {message}"
    );
}

#[test]
fn a_self_requirement_is_satisfied_by_the_installed_set() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::MEMORY)
        .requiring(CapabilityFamily::MEMORY)
        .into_shared()])
    .unwrap();
    assert_eq!(families(&plan), ["memory"]);
}

#[test]
fn capabilities_that_require_one_another_are_all_present_and_all_accepted() {
    let plan = CapabilityPlan::resolve([
        TestCapability::new(CapabilityFamily::MEMORY)
            .requiring(CapabilityFamily::SHELL)
            .into_shared(),
        TestCapability::new(CapabilityFamily::SHELL)
            .requiring(CapabilityFamily::SEARCH)
            .into_shared(),
        TestCapability::new(CapabilityFamily::SEARCH)
            .requiring(CapabilityFamily::MEMORY)
            .into_shared(),
    ])
    .unwrap();

    assert_eq!(
        families(&plan),
        vec!["memory", "shell", "search"],
        "a circle has no order to derive, and none is derived: each capability asked for a family \
         that is installed, which is the whole of what the declaration claims. The earlier version \
         rejected this because it was sorting the graph"
    );
}

// -- binding ------------------------------------------------------------------------------------

#[tokio::test]
async fn the_bound_form_is_what_contributes_to_the_run() {
    let installed = Arc::new(TestCapability::new(CapabilityFamily::MEMORY).binding());
    let plan = CapabilityPlan::resolve([Arc::clone(&installed) as Arc<dyn Capability>]).unwrap();

    let assembled = plan.assemble(&run_context("run-42")).await.unwrap();
    let prepared = assembled.prepare_agent(&agent()).unwrap();

    assert_eq!(
        *installed.bound_to.lock().unwrap(),
        vec!["run-42".to_owned()],
        "each capability is bound to the run it is about to serve"
    );
    assert_eq!(
        prepared.model_settings().metadata().get("bound_run"),
        Some(&"run-42".to_owned()),
        "contributions must be read from the bound form; reading the installed value would \
         describe a different run than the one starting"
    );
}

#[tokio::test]
async fn a_bound_form_may_not_change_the_family_the_set_was_validated_against() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::MEMORY)
        .binding_as(CapabilityFamily::SHELL)
        .into_shared()])
    .unwrap();

    let error = plan.assemble(&run_context("run-1")).await.unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("`memory`") && message.contains("`shell`"),
        "the dependency check and the assembly order were both decided against the installed \
         family, so a bound form that renames itself invalidates them: {message}"
    );
}

// -- what the assembled set becomes ---------------------------------------------------------------

#[tokio::test]
async fn tools_prompt_text_and_settings_all_reach_the_instance_that_executes() {
    let plan = CapabilityPlan::resolve([
        TestCapability::new(CapabilityFamily::SHELL)
            .with_tool("exec_command")
            .with_section("shell", "Run commands with `exec_command`.")
            .with_temperature(0.2)
            .into_shared(),
        TestCapability::new(CapabilityFamily::SKILLS)
            .requiring(CapabilityFamily::SHELL)
            .with_tool("load_skill")
            .with_section("skills", "Load a skill before following it.")
            .into_shared(),
    ])
    .unwrap();

    let assembled = plan.assemble(&run_context("run-1")).await.unwrap();
    let prepared = assembled.prepare_agent(&agent()).unwrap();

    assert_eq!(
        prepared
            .tools()
            .iter()
            .map(|tool| tool.origin().qualified_name().to_owned())
            .collect::<Vec<_>>(),
        vec!["exec_command", "load_skill"],
        "a capability's tools join the agent's own in assembly order"
    );
    assert_eq!(
        prepared.instructions().unwrap().as_static().unwrap(),
        "do the thing\n\nRun commands with `exec_command`.\n\nLoad a skill before following it.",
        "the paragraph that tells the model a tool exists must ship with the tool; the agent's own \
         text keeps the front of the prefix"
    );
    assert_eq!(prepared.model_settings().temperature(), Some(0.2));
    assert_eq!(
        prepared.model_settings().max_tokens(),
        Some(64),
        "the fold starts from the agent's settings layer and returns the same layer, so a setting \
         no capability speaks to survives"
    );
    assert_eq!(prepared.id(), agent().id());
}

#[tokio::test]
async fn the_sampling_fold_hands_each_capability_what_the_ones_before_it_produced() {
    let first = Arc::new(TestCapability::new(CapabilityFamily::SHELL).with_temperature(0.2));
    let second = Arc::new(TestCapability::new(CapabilityFamily::MEMORY).with_temperature(0.9));
    let plan = CapabilityPlan::resolve([
        Arc::clone(&first) as Arc<dyn Capability>,
        Arc::clone(&second) as Arc<dyn Capability>,
    ])
    .unwrap();

    let assembled = plan.assemble(&run_context("run-1")).await.unwrap();
    let prepared = assembled.prepare_agent(&agent()).unwrap();

    assert_eq!(
        *first.observed_temperature.lock().unwrap(),
        None,
        "the first capability installed folds first and sees only the agent's layer"
    );
    assert_eq!(
        *second.observed_temperature.lock().unwrap(),
        Some(0.2),
        "the fold is a chain, so a capability sees what the ones installed before it produced. \
         Which capability that is, is the host's own install order — no declaration reorders it"
    );
    assert_eq!(prepared.model_settings().temperature(), Some(0.9));
}

#[tokio::test]
async fn sampling_preserves_nested_provider_parameters_and_receives_the_resolved_model() {
    struct NestedSampling {
        family: CapabilityFamily,
        nested: serde_json::Value,
        seen: Arc<Mutex<Vec<SamplingContext>>>,
    }
    impl Capability for NestedSampling {
        fn kind(&self) -> CapabilityFamily {
            self.family.clone()
        }
        fn sampling_params_for(
            &self,
            settings: ModelSettings,
            context: &SamplingContext,
        ) -> ModelSettings {
            self.seen.lock().unwrap().push(context.clone());
            settings.with_extra_body_value(
                ProviderKey::new("test-provider"),
                "nested",
                self.nested.clone(),
            )
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capabilities: Vec<Arc<dyn Capability>> = vec![
        Arc::new(NestedSampling {
            family: CapabilityFamily::SHELL,
            nested: json!({"first":true,"list":[1]}),
            seen: seen.clone(),
        }),
        Arc::new(NestedSampling {
            family: CapabilityFamily::SEARCH,
            nested: json!({"second":true,"list":[2]}),
            seen: seen.clone(),
        }),
    ];
    let plan = CapabilityPlan::resolve(capabilities.clone()).unwrap();
    let prepared = plan
        .assemble(&run_context("nested"))
        .await
        .unwrap()
        .prepare_agent(&agent())
        .unwrap();
    assert_eq!(
        prepared.model_settings().extra_body()[&ProviderKey::new("test-provider")]["nested"],
        json!({"first":true,"second":true,"list":[2]})
    );
    seen.lock().unwrap().clear();
    Runner::run(run_request(
        &RecordingModel::new(),
        RunConfig::new()
            .with_model("alias")
            .with_capabilities(capabilities),
        &CancelScope::root(),
    ))
    .await
    .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for sampling in seen.iter() {
        assert_eq!(sampling.model(), Some("canonical-model"));
        assert_eq!(
            sampling.provider(),
            Some(&ProviderKey::new("test-provider"))
        );
    }
}

#[tokio::test]
async fn a_fragment_that_asks_for_the_tail_is_refused_rather_than_placed_in_the_prefix() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::WEB)
        .with_tail_section("web")
        .into_shared()])
    .unwrap();

    let error = plan.assemble(&run_context("run-1")).await.unwrap_err();

    assert!(
        error.to_string().contains("context processor"),
        "a fragment is resolved once per run, so text that varies per turn has to be sent to the \
         channel that runs per turn instead of quietly invalidating the cached prefix: {error}"
    );
}

#[tokio::test]
async fn a_capability_prompt_section_must_name_the_capability_that_contributed_it() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_section_source(PromptSource::Capability("search".to_owned()))
        .into_shared()])
    .unwrap();

    let error = plan.assemble(&run_context("run-1")).await.unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("`shell`")
            && message.contains("capability(search)")
            && message.contains("capability(shell)"),
        "a capability section with another capability's provenance would make prompt dumps and \
         assembly errors blame the wrong contributor; got: {message}"
    );
}

/// A plan resolves only static fragments before any run exists.
///
/// This is the reading an agent builder needs: the cached prefix is one span shared by every run
/// the agent serves, so the text in it cannot be a function of a run that has not started.
#[tokio::test]
async fn a_plan_resolves_its_static_fragments_before_a_run_exists() {
    let plan = CapabilityPlan::resolve([
        TestCapability::new(CapabilityFamily::SEARCH)
            .with_static_section("search", "search fragment")
            .into_shared(),
        TestCapability::new(CapabilityFamily::SHELL)
            .with_static_section("shell", "shell fragment")
            .into_shared(),
    ])
    .unwrap();

    let before_a_run: Vec<String> = plan
        .static_prompt_sections()
        .await
        .unwrap()
        .iter()
        .map(|section| format!("{}:{}", section.name(), section.content()))
        .collect();
    assert_eq!(
        before_a_run,
        ["search:search fragment", "shell:shell fragment"],
        "fragments arrive in assembly order, whether or not a run is asking for them"
    );
}

/// A static prefix contribution is not read again as a run-bound contribution.
///
/// The two methods have different owners: the installed capability writes the prefix an agent
/// shares, while a bound capability may write text particular to the run it serves. Reusing one
/// method for both would call it once before binding and again after binding, which makes a dump
/// disagree with the request a run sends.
///
#[tokio::test]
async fn a_static_fragment_does_not_cross_the_binding_boundary() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::MEMORY)
        .with_static_section("memory", "static fragment")
        .with_section("memory", "per-run fragment")
        .binding()
        .into_shared()])
    .unwrap();

    let resolved = plan.static_prompt_sections().await.unwrap();
    let sections: Vec<&str> = resolved.iter().map(|section| section.content()).collect();
    assert_eq!(
        sections,
        ["static fragment"],
        "the run-free reading takes the static method only; the per-run text belongs to a bound \
         form that does not exist yet"
    );
}

/// One runtime installation carries static text and runtime behavior together.
#[tokio::test]
async fn run_assembly_carries_static_text_and_all_runtime_contributions() {
    let model = RecordingModel::new();
    let config = RunConfig::new().with_capability(
        TestCapability::new(CapabilityFamily::SHELL)
            .with_tool("exec_command")
            .with_static_section("shell", "static fragment")
            .with_section("shell", "per-run fragment")
            .with_temperature(0.3)
            .with_processor()
            .into_shared(),
    );
    Runner::run(run_request(&model, config, &CancelScope::root()))
        .await
        .unwrap();
    assert_eq!(model.tools.lock().unwrap()[0], ["exec_command"]);
    assert_eq!(model.temperatures.lock().unwrap()[0], Some(0.3));
    assert_eq!(
        model.instructions.lock().unwrap()[0].as_deref(),
        Some("do the thing\n\nstatic fragment\n\nper-run fragment")
    );
    assert!(model.inputs.lock().unwrap()[0].iter().any(|item| {
        matches!(item, ModelInputItem::Message(message)
            if message.text_content() == "processed by shell")
    }));
}

#[tokio::test]
async fn a_raw_prepared_capability_keeps_its_static_prefix_and_all_runtime_channels() {
    let capability = TestCapability::new(CapabilityFamily::SHELL)
        .with_tool("exec_command")
        .with_static_section("shell", "already prepared static fragment")
        .with_section("shell", "per-run fragment")
        .with_deferred_section("deferred fragment")
        .with_temperature(0.3)
        .with_processor();
    let static_reads = capability.static_resolutions.clone();
    let tools = capability.tools();
    let prepared = agent()
        .to_builder()
        .instructions("do the thing\n\nalready prepared static fragment")
        .tools(tools.iter().cloned())
        .build()
        .unwrap();
    let model = RecordingModel::scripted(&["exec_command"]);
    let request = RunRequest::new(
        AgentBinding::direct(prepared),
        Arc::new(FixedResolver {
            model: model.clone(),
        }),
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("go"))],
    )
    .with_prepared_capabilities([capability.into_shared()], tools);
    Runner::run(request).await.unwrap();
    assert_eq!(static_reads.load(Ordering::SeqCst), 0);
    for instructions in model.instructions.lock().unwrap().iter() {
        assert_eq!(
            instructions.as_deref(),
            Some("do the thing\n\nalready prepared static fragment\n\nper-run fragment")
        );
    }
    assert_eq!(model.temperatures.lock().unwrap()[0], Some(0.3));
    assert!(model.messages(0).contains(&"processed by shell".to_owned()));
    assert!(!model.messages(0).contains(&"deferred fragment".to_owned()));
    assert!(model.messages(1).contains(&"deferred fragment".to_owned()));
}

#[tokio::test]
async fn prepared_handlers_replace_their_original_slots_and_direct_tools_append() {
    let zeta: Arc<dyn Tool> = Arc::new(NamedTool::new("zeta"));
    let alpha: Arc<dyn Tool> = Arc::new(NamedTool::new("alpha"));
    let mut bound_alpha = NamedTool::new("alpha");
    bound_alpha.output = "bound handler result";
    let mut capability = TestCapability::new(CapabilityFamily::SHELL).binding_with_tools(vec![
        Arc::new(bound_alpha),
        Arc::new(NamedTool::new("zeta")),
    ]);
    capability.tools = vec![zeta.clone(), alpha.clone()];
    let prepared = agent()
        .to_builder()
        .tools([
            zeta.clone(),
            Arc::new(NamedTool::new("base")),
            alpha.clone(),
        ])
        .build()
        .unwrap();
    let config = RunConfig::new()
        .with_capability(
            TestCapability::new(CapabilityFamily::MEMORY)
                .with_tool("appended")
                .into_shared(),
        )
        .with_prepared_capabilities([capability.into_shared()], [zeta, alpha]);
    let model = RecordingModel::scripted(&["alpha"]);
    let request = RunRequest::new(
        AgentBinding::direct(prepared),
        Arc::new(FixedResolver {
            model: model.clone(),
        }),
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("go"))],
    )
    .with_config(config);
    Runner::run(request).await.unwrap();
    for tools in model.tools.lock().unwrap().iter() {
        assert_eq!(tools, &["zeta", "base", "alpha", "appended"]);
    }
    assert!(
        serde_json::to_string(&model.inputs.lock().unwrap()[1])
            .unwrap()
            .contains("bound handler result")
    );
}

#[tokio::test]
async fn raw_prepared_capabilities_reject_changed_or_duplicate_bound_declarations() {
    for change in ["key", "schema", "options", "added", "removed", "duplicate"] {
        let original: Arc<dyn Tool> = Arc::new(NamedTool::new("prepared"));
        let mut bound = NamedTool::new(if change == "key" {
            "renamed"
        } else {
            "prepared"
        });
        if change == "schema" {
            bound.schema = ToolSchema::new(
                "prepared",
                json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false}),
            )
            .unwrap();
        }
        if change == "options" {
            bound.options = ToolOptions::new()
                .with_permission_scope(ra_core::permission::PermissionScope::Edit);
        }
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(bound)];
        if change == "removed" {
            tools.clear();
        } else if change == "added" {
            tools.push(Arc::new(NamedTool::new("extra")));
        } else if change == "duplicate" {
            tools.push(Arc::new(NamedTool::new("prepared")));
        }
        let mut capability = TestCapability::new(CapabilityFamily::SHELL).binding_with_tools(tools);
        capability.tools = vec![original.clone()];
        let prepared = agent().to_builder().tool(original.clone()).build().unwrap();
        let model = RecordingModel::new();
        let request = RunRequest::new(
            AgentBinding::direct(prepared),
            Arc::new(FixedResolver {
                model: model.clone(),
            }),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("go"))],
        )
        .with_prepared_capabilities([capability.into_shared()], [original]);
        let error = Runner::run(request).await.unwrap_err();
        assert!(
            error.to_string().contains("tool surface")
                || error.to_string().contains("duplicate bound tool"),
            "{change}: {error}"
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 0, "{change}");
    }
}

#[tokio::test]
async fn prepared_tools_require_exact_ownership_and_do_not_hide_direct_collisions() {
    for owns_handler in [false, true] {
        let original: Arc<dyn Tool> = Arc::new(NamedTool::new("prepared"));
        let mut capability = TestCapability::new(CapabilityFamily::SHELL);
        capability.tools = vec![original.clone()];
        let tool = if owns_handler {
            original.clone()
        } else {
            Arc::new(NamedTool::new("prepared"))
        };
        let prepared = agent().to_builder().tool(tool).build().unwrap();
        let mut config =
            RunConfig::new().with_prepared_capabilities([capability.into_shared()], [original]);
        if owns_handler {
            config = config.with_capability(
                TestCapability::new(CapabilityFamily::MEMORY)
                    .with_tool("prepared")
                    .into_shared(),
            );
        }
        let model = RecordingModel::new();
        let request = RunRequest::new(
            AgentBinding::direct(prepared),
            Arc::new(FixedResolver {
                model: model.clone(),
            }),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user("go"))],
        )
        .with_config(config);
        let error = Runner::run(request).await.unwrap_err();
        assert!(
            error.to_string().contains(if owns_handler {
                "more than once"
            } else {
                "exact shared"
            }),
            "{error}"
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn ordinary_capabilities_may_still_change_their_bound_tool_declarations() {
    let capability = TestCapability::new(CapabilityFamily::SHELL)
        .with_tool("declared")
        .binding_with_tools(vec![Arc::new(NamedTool::new("bound"))])
        .into_shared();
    let model = RecordingModel::new();
    Runner::run(run_request(
        &model,
        RunConfig::new().with_capability(capability),
        &CancelScope::root(),
    ))
    .await
    .unwrap();
    assert_eq!(model.tools.lock().unwrap()[0], ["bound"]);
}

/// The per-run instruction channel remains independently usable.
#[tokio::test]
async fn run_assembly_accepts_a_capability_whose_text_is_per_run() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_tool("exec_command")
        .with_section("shell", "per-run fragment")
        .into_shared()])
    .unwrap();

    let assembled = plan.assemble(&run_context("run-1")).await.unwrap();
    assert_eq!(assembled.tools().len(), 1);
    let sections: Vec<&str> = assembled
        .prompt_sections()
        .iter()
        .map(|section| section.content())
        .collect();
    assert_eq!(sections, ["per-run fragment"]);
}

#[tokio::test]
async fn capability_instruction_fragments_preserve_their_original_whitespace() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_static_section("shell", "\n static text \n")
        .with_section("shell", " \n")
        .into_shared()])
    .unwrap();
    let prepared = plan
        .assemble(&run_context("whitespace"))
        .await
        .unwrap()
        .prepare_agent(&agent())
        .unwrap();
    assert_eq!(
        prepared.instructions().unwrap().as_static(),
        Some("do the thing\n\n\n static text \n\n\n \n")
    );
}

/// The run-free reading enforces the same structural prefix rules as the runtime path.
#[tokio::test]
async fn a_plan_refuses_invalid_static_fragments_before_a_run_exists() {
    let claimed = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_static_section("tool_use", "first")
        .into_shared()])
    .unwrap();
    let error = claimed.static_prompt_sections().await.unwrap_err();
    assert!(
        error.to_string().contains("`shell`") && error.to_string().contains("`tool_use`"),
        "{error}"
    );

    let tail = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::WEB)
        .with_static_tail_section("web")
        .into_shared()])
    .unwrap();
    let error = tail.static_prompt_sections().await.unwrap_err();
    assert!(error.to_string().contains("context processor"), "{error}");
}

#[tokio::test]
async fn a_per_run_fragment_must_claim_its_own_capability_section() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_section("tool_use", "first")
        .into_shared()])
    .unwrap();

    let error = plan.assemble(&run_context("run-1")).await.unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("`shell`") && message.contains("`tool_use`"),
        "a capability must not take a product section's slot: {message}"
    );
}

/// Capability fragments follow generated instructions, which are still evaluated every turn.
///
/// Upstream `build_sandbox_instructions` resolves a callable base on each call and appends the
/// capability instructions to it, so the fragments neither replace the generator nor freeze it.
#[tokio::test]
async fn capability_fragments_follow_generated_instructions_on_every_turn() {
    let turns = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&turns);
    let dynamic_agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .dynamic_instructions_sync_fn(move |_context| {
            let turn = counter.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(ResolvedPrompt::new(
                format!("generated turn {turn}\n"),
                PromptSource::Dynamic("callback".into()),
            )
            .with_version("v3"))
        })
        .build()
        .unwrap();
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_section("shell", "Run commands.")
        .into_shared()])
    .unwrap();
    let assembled = plan.assemble(&run_context("run-1")).await.unwrap();
    let prepared = assembled.prepare_agent(&dynamic_agent).unwrap();
    let instructions = prepared.instructions().unwrap();
    assert!(instructions.is_dynamic());

    for turn in 1..=2 {
        let ResolvedInstructions::Generated(prompt) =
            instructions.resolve(&run_context("run-1")).await.unwrap()
        else {
            panic!("composed instructions must stay generated system text");
        };
        assert_eq!(
            prompt.text(),
            format!("generated turn {turn}\n\n\nRun commands.")
        );
        assert_eq!(prompt.source(), &PromptSource::Dynamic("callback".into()));
        assert_eq!(prompt.version(), Some("v3"));
    }
    assert_eq!(turns.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn capability_composition_preserves_static_and_generated_instruction_whitespace() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_section("shell", "Run commands.")
        .into_shared()])
    .unwrap();
    let context = run_context("run-1");
    let assembled = plan.assemble(&context).await.unwrap();

    for base in ["", " \t\n ", "generated\n", "  generated \t\n\n"] {
        let mut sources = vec![AgentInstructions::dynamic_sync_fn(move |_| {
            Ok(ResolvedPrompt::new(base, PromptSource::Agent))
        })];
        // Static declarations require non-empty text; generated empty text is allowed.
        if !base.is_empty() {
            sources.push(AgentInstructions::static_text(base));
        }
        let expected = if base.is_empty() {
            "Run commands.".to_owned()
        } else {
            format!("{base}\n\nRun commands.")
        };
        for source in sources {
            let agent = AgentSpec::builder()
                .id(AgentId::new("coder"))
                .name("Coder")
                .with_instructions(source)
                .build()
                .unwrap();
            let prepared = assembled.prepare_agent(&agent).unwrap();
            let resolved = prepared
                .instructions()
                .unwrap()
                .resolve(&context)
                .await
                .unwrap();
            let text = match resolved {
                ResolvedInstructions::Prefix(text) => text,
                ResolvedInstructions::Generated(prompt) => {
                    let (text, provenance) = prompt.lower_to_system_instructions().unwrap();
                    assert_eq!(
                        provenance.content_hash(),
                        &ra_core::prompt::ContentHash::compute(&expected),
                    );
                    text
                }
                ResolvedInstructions::Tail(_) => panic!("expected system instructions"),
            };
            assert_eq!(text, expected, "base instruction text: {base:?}");
        }
    }
}

/// Capability fragments are system text, so they cannot merge with the tail-message extension.
#[tokio::test]
async fn capability_fragments_reject_the_tail_instruction_extension() {
    let tail_agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .with_instructions(AgentInstructions::dynamic_tail_fn(|_context| async {
            Ok(ResolvedPrompt::new("tail", PromptSource::Agent))
        }))
        .build()
        .unwrap();
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_section("shell", "Run commands.")
        .into_shared()])
    .unwrap();
    let assembled = plan.assemble(&run_context("run-1")).await.unwrap();
    let prepared = assembled.prepare_agent(&tail_agent).unwrap();

    let error = prepared
        .instructions()
        .unwrap()
        .resolve(&run_context("run-1"))
        .await
        .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("`shell`") && message.contains("tail-message"),
        "the refusal must name the capability whose text cannot be placed: {message}"
    );
}

#[tokio::test]
async fn a_capability_tool_that_collides_with_the_agents_own_names_the_assembly() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_tool("exec_command")
        .into_shared()])
    .unwrap();
    let host_agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .tool(Arc::new(NamedTool::new("exec_command")))
        .build()
        .unwrap();

    let assembled = plan.assemble(&run_context("run-1")).await.unwrap();
    let error = assembled.prepare_agent(&host_agent).unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("assembling capabilities `shell`"),
        "the agent builder catches the collision, but on its own it reports an agent nobody wrote \
         down; the assembly has to say which capabilities were folded onto it: {message}"
    );
}

// -- what the run does with it --------------------------------------------------------------------

#[tokio::test]
async fn a_run_sends_the_assembled_surface_and_keeps_the_configured_agent_as_its_identity() {
    let model = RecordingModel::new();
    let cancel = CancelScope::root();
    let config = RunConfig::new().with_capability(
        TestCapability::new(CapabilityFamily::SHELL)
            .with_tool("exec_command")
            .with_section("shell", "Run commands with `exec_command`.")
            .with_temperature(0.2)
            .into_shared(),
    );

    let result = Runner::run(run_request(&model, config, &cancel))
        .await
        .unwrap();

    assert_eq!(
        model.tools.lock().unwrap()[0],
        vec!["exec_command".to_owned()],
        "the capability's tool has to reach the provider request"
    );
    assert_eq!(
        model.instructions.lock().unwrap()[0].as_deref(),
        Some("do the thing\n\nRun commands with `exec_command`."),
        "and so does the paragraph that says the tool exists"
    );
    assert_eq!(model.temperatures.lock().unwrap()[0], Some(0.2));
    assert_eq!(
        result.last_agent().id(),
        agent().id(),
        "assembly replaces what executes, never who the run is attributed to"
    );
}

#[tokio::test]
async fn a_context_transform_is_installed_after_the_processors_the_host_put_there_itself() {
    let model = RecordingModel::new();
    let cancel = CancelScope::root();
    let config = RunConfig::new()
        .with_context_processor(Arc::new(TaggingProcessor::new("host")))
        .with_capability(
            TestCapability::new(CapabilityFamily::COMPACTION)
                .with_processor()
                .into_shared(),
        );

    Runner::run(run_request(&model, config, &cancel))
        .await
        .unwrap();

    let inputs = model.inputs.lock().unwrap();
    let tags: Vec<String> = inputs[0]
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) => Some(message.text_content()),
            _ => None,
        })
        .filter(|text| text.starts_with("processed by") || text == "host")
        .collect();
    assert_eq!(
        tags,
        vec!["host".to_owned(), "processed by compaction".to_owned()],
        "a processor the host installed directly was there before any capability was resolved, so \
         running the capability chain ahead of it would change what already worked"
    );
}

#[tokio::test]
async fn a_missing_dependency_stops_the_run_before_it_pays_for_a_model_call() {
    let model = RecordingModel::new();
    let cancel = CancelScope::root();
    let config = RunConfig::new().with_capability(
        TestCapability::new(CapabilityFamily::MEMORY)
            .requiring(CapabilityFamily::SHELL)
            .into_shared(),
    );

    let error = Runner::run(run_request(&model, config, &cancel))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("`memory` requires `shell`"));
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        0,
        "a half-assembled surface reported after a model has read it is reported too late"
    );
}

#[tokio::test]
async fn a_run_with_no_capabilities_is_left_exactly_as_it_was() {
    let model = RecordingModel::new();
    let cancel = CancelScope::root();

    let result = Runner::run(run_request(&model, RunConfig::new(), &cancel))
        .await
        .unwrap();

    assert_eq!(
        model.instructions.lock().unwrap()[0].as_deref(),
        Some("do the thing")
    );
    assert!(model.tools.lock().unwrap()[0].is_empty());
    assert_eq!(result.last_agent().id(), agent().id());
}

// -- deferred fragments -------------------------------------------------------------------------

#[tokio::test]
async fn a_deferred_fragment_costs_a_run_that_never_reaches_for_it_nothing() {
    let model = RecordingModel::new();
    let cancel = CancelScope::root();
    let capability = TestCapability::new(CapabilityFamily::WEB)
        .with_tool("browse")
        .with_section("web", "The `browse` entry fetches a page.")
        .with_deferred_section("Browsing: coordinates are viewport-relative, and a screenshot ...");
    let resolutions = capability.deferred_resolutions();
    let config = RunConfig::new().with_capability(capability.into_shared());

    let result = Runner::run(run_request(&model, config, &cancel))
        .await
        .unwrap();

    assert_eq!(
        model.instructions.lock().unwrap()[0].as_deref(),
        Some("do the thing\n\nThe `browse` entry fetches a page."),
        "the resident half is the only half in the cached prefix; deferring exists because the \
         other half would be charged on every turn of every run"
    );
    assert!(
        !model
            .messages(0)
            .iter()
            .any(|text| text.contains("Browsing:")),
        "and it is not in the tail either — a run that never called the entry has earned nothing"
    );
    assert!(
        !result
            .new_items()
            .iter()
            .any(|item| item.id().as_str().starts_with("capability-prompt.")),
        "nothing was delivered, so there is no record of a delivery"
    );
    assert_eq!(
        resolutions.load(Ordering::SeqCst),
        1,
        "the text is still resolved once at assembly: a source read on the turn its signal fires \
         would pay for itself mid-turn, and fail a run that was going fine"
    );
}

#[tokio::test]
async fn a_deferred_fragment_arrives_once_the_model_reaches_for_the_entry_it_describes() {
    let model = RecordingModel::scripted(&["browse"]);
    let cancel = CancelScope::root();
    let config = RunConfig::new().with_capability(
        TestCapability::new(CapabilityFamily::WEB)
            .with_tool("browse")
            .with_deferred_section("Browsing: coordinates are viewport-relative.")
            .into_shared(),
    );

    let result = Runner::run(run_request(&model, config, &cancel))
        .await
        .unwrap();

    assert!(
        !model
            .messages(0)
            .iter()
            .any(|text| text.contains("Browsing:")),
        "the first turn had nothing to go on but the entry's own schema"
    );
    assert!(
        model
            .messages(1)
            .iter()
            .any(|text| text.contains("Browsing:")),
        "the turn that carries the first result is also the first turn the mechanism could be \
         acted on, so that is where it arrives"
    );
    let delivered: Vec<&str> = result
        .new_items()
        .iter()
        .map(|item| item.id().as_str())
        .filter(|id| id.starts_with("capability-prompt."))
        .collect();
    assert_eq!(
        delivered,
        vec!["capability-prompt.web"],
        "delivery is an authoritative record, filed under the family that earned it"
    );
    assert!(
        result.new_items().iter().any(|item| {
            item.id().as_str() == "capability-prompt.web"
                && matches!(item.kind(), RunItemKind::Message(message) if message.role() == MessageRole::User)
        }),
        "a delivered fragment is a user tail message, so every supported protocol can carry it in \
         input history"
    );
    assert!(
        model.instructions.lock().unwrap()[1]
            .as_deref()
            .is_some_and(|text| !text.contains("Browsing:")),
        "and it never joins the cached prefix, which is what a later turn would have to rebuild"
    );
}

#[tokio::test]
async fn a_delivered_fragment_is_not_delivered_again_on_the_turns_after_it() {
    let model = RecordingModel::scripted(&["browse", "browse"]);
    let cancel = CancelScope::root();
    let config = RunConfig::new().with_capability(
        TestCapability::new(CapabilityFamily::WEB)
            .with_tool("browse")
            .with_deferred_section("Browsing: coordinates are viewport-relative.")
            .into_shared(),
    );

    let result = Runner::run(run_request(&model, config, &cancel))
        .await
        .unwrap();

    let repeats = model
        .messages(2)
        .iter()
        .filter(|text| text.contains("Browsing:"))
        .count();
    assert_eq!(
        repeats, 1,
        "the fragment stays in history, so re-sending it would be a second copy — and rebuilding \
         it into every tail is the per-turn charge deferring exists to avoid"
    );
    assert_eq!(
        result
            .new_items()
            .iter()
            .filter(|item| item.id().as_str() == "capability-prompt.web")
            .count(),
        1,
        "one delivery, one record; the record is also what says it already happened"
    );
}

#[tokio::test]
async fn one_capabilitys_call_does_not_arm_another_capabilitys_text() {
    let model = RecordingModel::scripted(&["browse"]);
    let cancel = CancelScope::root();
    let config = RunConfig::new()
        .with_capability(
            TestCapability::new(CapabilityFamily::WEB)
                .with_tool("browse")
                .with_deferred_section("Browsing: coordinates are viewport-relative.")
                .into_shared(),
        )
        .with_capability(
            TestCapability::new(CapabilityFamily::VIEW_IMAGE)
                .with_tool("view_image")
                .with_deferred_section("Images: a screenshot is downsampled before it is attached.")
                .into_shared(),
        );

    let result = Runner::run(run_request(&model, config, &cancel))
        .await
        .unwrap();

    let delivered: Vec<&str> = result
        .new_items()
        .iter()
        .map(|item| item.id().as_str())
        .filter(|id| id.starts_with("capability-prompt."))
        .collect();
    assert_eq!(
        delivered,
        vec!["capability-prompt.web"],
        "the default signal is the capability's own entries, read off its own tools; a shared \
         trigger would charge every deferred fragment for the first tool call of any of them"
    );
}

#[tokio::test]
async fn a_capability_may_decide_its_own_signal_instead_of_taking_the_default() {
    let model = RecordingModel::scripted(&["nothing"]);
    let cancel = CancelScope::root();
    let config = RunConfig::new().with_capability(Arc::new(TurnArmedCapability {
        kind: CapabilityFamily::SKILLS,
        turn: 2,
    }) as Arc<dyn Capability>);

    Runner::run(run_request(&model, config, &cancel))
        .await
        .unwrap();

    assert!(
        !model
            .messages(0)
            .iter()
            .any(|text| text.contains("Skills:")),
        "the first turn does not satisfy the override's own condition"
    );
    assert!(
        model
            .messages(1)
            .iter()
            .any(|text| text.contains("Skills:")),
        "a capability with no tools has no default signal to speak of, so the decision has to be \
         one it can make itself — from structured facts, never from words in the conversation"
    );
}

#[tokio::test]
async fn a_deferred_fragment_must_claim_its_familys_deferred_section() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::WEB)
        .with_deferred_section_named("web")
        .into_shared()])
    .unwrap();

    let error = plan
        .assemble(&run_context("run-deferred"))
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("`web.deferred`"),
        "the resident and deferred halves are two texts, and one name for both would report one \
         of them as the other: {error}"
    );
}

#[tokio::test]
async fn a_deferred_fragment_that_asks_for_the_prefix_is_refused() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::WEB)
        .with_deferred_section_in_prefix()
        .into_shared()])
    .unwrap();

    let error = plan
        .assemble(&run_context("run-deferred"))
        .await
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("resident text with a delay on it"),
        "a deferred fragment placed in the prefix is charged on every turn of every run, which is \
         the arrangement it was written to avoid: {error}"
    );
}

#[tokio::test]
async fn a_resumed_run_does_not_deliver_a_fragment_its_own_history_already_holds() {
    let model = RecordingModel::scripted(&["browse"]);
    let cancel = CancelScope::root();
    let capability = || {
        TestCapability::new(CapabilityFamily::WEB)
            .with_tool("browse")
            .with_deferred_section("Browsing: coordinates are viewport-relative.")
            .into_shared()
    };

    let first = Runner::run(run_request(
        &model,
        RunConfig::new().with_capability(capability()),
        &cancel,
    ))
    .await
    .unwrap();
    let carried = first.state().clone();

    let resumed = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent()),
            Arc::new(FixedResolver {
                model: Arc::clone(&model),
            }),
            RunId::new("run-capabilities"),
            cancel.clone(),
            Vec::new(),
        )
        .with_config(RunConfig::new().with_capability(capability()))
        .with_state(carried),
    )
    .await
    .unwrap();

    assert!(
        !resumed
            .new_items()
            .iter()
            .any(|item| item.id().as_str() == "capability-prompt.web"),
        "the signal is still satisfied on a resume — what says the text already arrived is the \
         history, which a checkpoint carries and an in-memory flag would not"
    );
    let last = model.inputs.lock().unwrap().len() - 1;
    assert_eq!(
        model
            .messages(last)
            .iter()
            .filter(|text| text.contains("Browsing:"))
            .count(),
        1,
        "and the resumed segment still shows the model the fragment exactly once"
    );
}

/// A capability whose deferred text is earned by the turn count rather than by its own entries.
struct TurnArmedCapability {
    kind: CapabilityFamily,
    turn: u64,
}

#[async_trait]
impl Capability for TurnArmedCapability {
    fn kind(&self) -> CapabilityFamily {
        self.kind.clone()
    }

    async fn deferred_instructions(&self) -> Result<Option<PromptSection>> {
        Ok(Some(
            PromptSection::new(
                self.kind.deferred_prompt_section_name(),
                "deferred capability fragment",
                self.kind.prompt_source(),
                SectionStability::Stable,
                SectionPosition::TailMessage,
                "Skills: a skill is loaded by name.",
            )
            .unwrap(),
        ))
    }

    fn wants_deferred_instructions(&self, signal: &LoadSignal<'_>) -> bool {
        signal.turn() >= self.turn
    }
}

/// A processor installed directly by a host, which marks the input it saw.
struct TaggingProcessor {
    tag: String,
}

impl TaggingProcessor {
    fn new(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
        }
    }
}

#[async_trait]
impl ContextProcessor for TaggingProcessor {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _summarizer: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        let mut input = request.input().to_vec();
        input.push(ModelInputItem::Message(Message::system(self.tag.clone())));
        Ok(ContextProcessorResult::new(input))
    }
}
