//! Contracts for validating an installed capability set and folding it into one agent.

use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    capability::{
        Capability, CapabilityFamily, ContextProcessor, ContextProcessorRequest,
        ContextProcessorResult, ContextSummarizer,
    },
    context::RunContext,
    error::Result,
    item::{ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    prompt::{
        PromptSection, PromptSectionName, PromptSource, ResolvedPrompt, SectionPosition,
        SectionStability,
    },
    state::RunId,
    tool::{Tool, ToolContext, ToolOrigin, ToolOutput, ToolSchema},
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
    temperature: Option<f64>,
    /// What the sampling fold handed this capability, recorded so order is observable.
    observed_temperature: Arc<Mutex<Option<f64>>>,
    processor: bool,
    /// Set when this capability supplies a per-run form instead of serving the run itself.
    binds: bool,
    /// The family a bound form reports, when it disagrees with the installed one.
    bound_kind: Option<CapabilityFamily>,
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
            temperature: None,
            observed_temperature: Arc::new(Mutex::new(None)),
            processor: false,
            binds: false,
            bound_kind: None,
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
        Ok(self.static_section.clone())
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
}

impl Capability for BoundCapability {
    fn kind(&self) -> CapabilityFamily {
        self.kind.clone()
    }

    fn sampling_params(&self, settings: ModelSettings) -> ModelSettings {
        settings.with_metadata("bound_run", self.run_id.clone())
    }
}

struct NamedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
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

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("done"))
    }
}

/// Answers one turn and records the surface it was asked with.
struct RecordingModel {
    calls: AtomicUsize,
    instructions: Mutex<Vec<Option<String>>>,
    tools: Mutex<Vec<Vec<String>>>,
    temperatures: Mutex<Vec<Option<f64>>>,
    inputs: Mutex<Vec<Vec<ModelInputItem>>>,
}

impl RecordingModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            instructions: Mutex::new(Vec::new()),
            tools: Mutex::new(Vec::new()),
            temperatures: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
        })
    }

    fn answer(&self, request: &ModelRequest) -> ModelResponse {
        self.calls.fetch_add(1, Ordering::SeqCst);
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
        ModelResponse::new(vec![RunItem::new(
            ItemId::new("msg-1"),
            RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
        )])
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
fn a_declared_dependency_is_also_an_ordering_edge() {
    let plan = CapabilityPlan::resolve([
        TestCapability::new(CapabilityFamily::MEMORY)
            .requiring(CapabilityFamily::SHELL)
            .into_shared(),
        TestCapability::new(CapabilityFamily::SHELL).into_shared(),
    ])
    .unwrap();

    assert_eq!(
        families(&plan),
        vec!["shell", "memory"],
        "a capability declaring a dependency must be assembled after the family it names, or the \
         one arrangement its own declaration calls wrong is the one that runs"
    );
}

#[test]
fn capabilities_no_dependency_constrains_keep_installation_order() {
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
        vec!["todo", "filesystem", "web", "search"],
        "the edge costs exactly one swap: `filesystem` moves ahead of the `web` that requires it, \
         and `search` — which no edge touches — keeps its place. Sending `web` to the end instead \
         would satisfy the same dependency while pushing it behind every unrelated capability the \
         host happened to install after it"
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
fn a_capability_cannot_depend_on_its_own_family() {
    let error = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::MEMORY)
        .requiring(CapabilityFamily::MEMORY)
        .into_shared()])
    .unwrap_err();

    assert!(
        error.to_string().contains("its own family"),
        "a one-node circle deserves its own message; reported as a cycle it would name one \
         capability twice and read like a defect in the report: {error}"
    );
}

#[test]
fn capabilities_that_require_one_another_have_no_order_to_pick() {
    let error = CapabilityPlan::resolve([
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
    .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("`memory` -> `shell` -> `search` -> `memory`"),
        "the circle itself is the actionable part: a list of the capabilities involved leaves the \
         reader to work out which edge to cut; got: {message}"
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
async fn the_sampling_fold_follows_assembly_order_rather_than_installation_order() {
    let dependent = Arc::new(
        TestCapability::new(CapabilityFamily::MEMORY)
            .requiring(CapabilityFamily::SHELL)
            .with_temperature(0.9),
    );
    let dependency = Arc::new(TestCapability::new(CapabilityFamily::SHELL).with_temperature(0.2));
    let plan = CapabilityPlan::resolve([
        Arc::clone(&dependent) as Arc<dyn Capability>,
        Arc::clone(&dependency) as Arc<dyn Capability>,
    ])
    .unwrap();

    let assembled = plan.assemble(&run_context("run-1")).await.unwrap();
    let prepared = assembled.prepare_agent(&agent()).unwrap();

    assert_eq!(
        *dependency.observed_temperature.lock().unwrap(),
        None,
        "the dependency folds first and sees only the agent's layer"
    );
    assert_eq!(
        *dependent.observed_temperature.lock().unwrap(),
        Some(0.2),
        "a capability that declared a dependency must see what that dependency produced, which is \
         the whole reason the edge reorders the fold"
    );
    assert_eq!(prepared.model_settings().temperature(), Some(0.9));
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
#[tokio::test]
async fn a_static_fragment_does_not_cross_the_binding_boundary() {
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::MEMORY)
        .with_static_section("memory", "static fragment")
        .binding()
        .into_shared()])
    .unwrap();

    let static_sections = plan.static_prompt_sections().await.unwrap();
    assert_eq!(static_sections.len(), 1);

    let assembled = plan.assemble(&run_context("run-1")).await.unwrap();
    assert!(
        assembled.prompt_sections().is_empty(),
        "the bound form supplies no per-run fragment, so assembly must not re-read the installed \
         capability's static one"
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

#[tokio::test]
async fn prefix_text_and_a_per_turn_prompt_cannot_share_one_instruction_slot() {
    let dynamic_agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .dynamic_instructions_fn(|_context| async {
            Ok(ResolvedPrompt::new("generated", PromptSource::Agent))
        })
        .build()
        .unwrap();
    let plan = CapabilityPlan::resolve([TestCapability::new(CapabilityFamily::SHELL)
        .with_section("shell", "Run commands.")
        .into_shared()])
    .unwrap();

    let assembled = plan.assemble(&run_context("run-1")).await.unwrap();
    let error = assembled.prepare_agent(&dynamic_agent).unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("`shell`") && message.contains("volatile tail"),
        "an agent has one instruction slot and a generated prompt may only reach the tail, so the \
         refusal has to name the capability whose prefix text has nowhere to go: {message}"
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
