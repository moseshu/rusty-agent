//! Contracts for the assembly unit that packages tools, prompt text, sampling, and context.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    capability::{
        Capability, CapabilityFamily, ContextProcessor, ContextProcessorRequest,
        ContextProcessorResult, ContextSummarizer, ContextSummaryRequest, ContextSummaryResponse,
    },
    context::RunContext,
    error::Result,
    item::{ItemId, Message, ModelInputItem, ModelResponse},
    model::ModelSettings,
    prompt::{
        PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
        estimate_tokens,
    },
    state::RunId,
    tool::{Tool, ToolContext, ToolOrigin, ToolOutput, ToolSchema},
};
use serde_json::json;

/// A capability that declares nothing beyond its identity, which is the whole required surface.
struct BareCapability(CapabilityFamily);

impl Capability for BareCapability {
    fn kind(&self) -> CapabilityFamily {
        self.0.clone()
    }
}

/// A capability that contributes one setting and records what the fold handed it.
struct SamplingCapability {
    kind: CapabilityFamily,
    temperature: f64,
    observed_temperature: Arc<std::sync::Mutex<Option<f64>>>,
}

impl Capability for SamplingCapability {
    fn kind(&self) -> CapabilityFamily {
        self.kind.clone()
    }

    fn sampling_params(&self, settings: ModelSettings) -> ModelSettings {
        *self.observed_temperature.lock().unwrap() = settings.temperature();
        settings.with_temperature(self.temperature)
    }
}

/// A capability with a prompt fragment and a dependency on another family.
struct SkillsCapability;

#[async_trait]
impl Capability for SkillsCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SKILLS
    }

    fn required_capabilities(&self) -> BTreeSet<CapabilityFamily> {
        // Spelled the way a third party would spell it, not with the constant. The two have to be
        // the same value or a declared dependency never matches the capability that satisfies it.
        BTreeSet::from([CapabilityFamily::new("filesystem").unwrap()])
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(LoadSkillTool::new())]
    }

    async fn instructions(&self) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(
            PromptSectionName::new("skills"),
            "How to discover and load a skill",
            self.kind().prompt_source(),
            SectionStability::Stable,
            SectionPosition::Prefix,
            "Call `load_skill` before following a skill's steps.",
        )?))
    }
}

/// A capability whose per-run form remembers which run it was bound to.
struct BindingCapability;

struct BoundCapability(RunId);

impl Capability for BindingCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::MEMORY
    }

    fn bind(&self, context: &RunContext) -> Result<Option<Arc<dyn Capability>>> {
        Ok(Some(Arc::new(BoundCapability(context.run_id().clone()))))
    }
}

impl Capability for BoundCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::MEMORY
    }

    fn sampling_params(&self, settings: ModelSettings) -> ModelSettings {
        settings.with_metadata("run", self.0.as_str())
    }
}

/// A capability that transforms context, which it does by being a context processor.
struct TrimmingCapability;

impl Capability for TrimmingCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::COMPACTION
    }

    fn context_processor(&self) -> Option<&dyn ContextProcessor> {
        Some(self)
    }
}

#[async_trait]
impl ContextProcessor for TrimmingCapability {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _summarizer: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        let kept = request.input().iter().rev().take(1).cloned().collect();
        Ok(ContextProcessorResult::new(kept))
    }
}

struct RefusingSummarizer;

#[async_trait]
impl ContextSummarizer for RefusingSummarizer {
    async fn summarize(&self, _request: ContextSummaryRequest) -> Result<ContextSummaryResponse> {
        Ok(ContextSummaryResponse::new(
            "unused",
            ModelResponse::new(Vec::new()),
        ))
    }
}

struct LoadSkillTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl LoadSkillTool {
    fn new() -> Self {
        Self {
            origin: ToolOrigin::new("load_skill").unwrap(),
            schema: ToolSchema::new(
                "load_skill",
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
impl Tool for LoadSkillTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("loaded"))
    }
}

fn run_context(run_id: &str) -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .build()
        .unwrap();
    RunContext::new(RunId::new(run_id), &agent)
}

fn user_item(text: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(text))
}

#[test]
fn a_family_has_exactly_one_spelling_so_a_dependency_matches_what_satisfies_it() {
    assert_eq!(
        CapabilityFamily::new("shell").unwrap(),
        CapabilityFamily::SHELL,
        "a family written out by a third party must equal the built-in constant, or a declared \
         dependency never matches the installed capability that satisfies it"
    );
    assert_eq!(CapabilityFamily::SHELL.as_str(), "shell");
    assert_eq!(CapabilityFamily::APPLY_PATCH.to_string(), "apply_patch");

    // A namespaced family is how a plugin stays out of the built-in set's way.
    assert_eq!(
        CapabilityFamily::new("plugin.browser").unwrap().as_str(),
        "plugin.browser"
    );

    for rejected in [
        "",         // nothing to name
        "Shell",    // a second spelling of `shell`
        "SHELL",    //
        " shell",   //
        "shell ",   //
        "shell\n",  //
        "1shell",   // not a name
        "_shell",   //
        "shell-fs", // `-` is not in the set, so it cannot become a second spelling of `shell_fs`
        "shell fs",
    ] {
        assert!(
            CapabilityFamily::new(rejected.to_owned()).is_err(),
            "`{rejected}` must be refused: two spellings of one family compare unequal"
        );
    }
}

#[test]
fn a_family_is_validated_on_the_way_in_from_the_wire() {
    let family = CapabilityFamily::MEMORY;
    let encoded = serde_json::to_string(&family).unwrap();
    assert_eq!(encoded, "\"memory\"");
    assert_eq!(
        serde_json::from_str::<CapabilityFamily>(&encoded).unwrap(),
        family
    );

    assert!(
        serde_json::from_str::<CapabilityFamily>("\"Memory\"").is_err(),
        "a stored non-canonical name must fail on the way in, not become a family that matches \
         nothing"
    );
}

#[tokio::test]
async fn a_capability_declaring_only_its_kind_contributes_nothing() {
    let capability = BareCapability(CapabilityFamily::new("plugin.browser").unwrap());
    let settings = ModelSettings::new().with_temperature(0.4);

    assert_eq!(capability.kind().as_str(), "plugin.browser");
    assert!(capability.required_capabilities().is_empty());
    assert!(capability.tools().is_empty());
    assert!(capability.instructions().await.unwrap().is_none());
    assert!(capability.context_processor().is_none());
    assert!(
        capability.bind(&run_context("run-1")).unwrap().is_none(),
        "the default binding shares the installed capability rather than copying it per run"
    );
    assert_eq!(
        capability.sampling_params(settings.clone()).temperature(),
        settings.temperature(),
        "a capability that states no settings must pass the fold through untouched"
    );
}

#[test]
fn sampling_params_fold_in_installation_order_and_each_sees_the_last() {
    let first_saw = Arc::new(std::sync::Mutex::new(None));
    let second_saw = Arc::new(std::sync::Mutex::new(None));
    let capabilities: Vec<Box<dyn Capability>> = vec![
        Box::new(SamplingCapability {
            kind: CapabilityFamily::SHELL,
            temperature: 0.2,
            observed_temperature: Arc::clone(&first_saw),
        }),
        Box::new(SamplingCapability {
            kind: CapabilityFamily::MEMORY,
            temperature: 0.9,
            observed_temperature: Arc::clone(&second_saw),
        }),
    ];

    let agent_layer = ModelSettings::new()
        .with_temperature(0.1)
        .with_max_tokens(64);
    let folded = capabilities
        .iter()
        .fold(agent_layer, |settings, capability| {
            capability.sampling_params(settings)
        });

    assert_eq!(*first_saw.lock().unwrap(), Some(0.1));
    assert_eq!(
        *second_saw.lock().unwrap(),
        Some(0.2),
        "each capability must see what the ones before it produced, or installation order says \
         nothing and two capabilities that disagree resolve by luck"
    );
    assert_eq!(folded.temperature(), Some(0.9));
    assert_eq!(
        folded.max_tokens(),
        Some(64),
        "the fold carries the agent's own settings through the capabilities that do not speak to \
         them"
    );
}

#[tokio::test]
async fn a_prompt_fragment_is_attributed_to_the_capability_that_wrote_it() {
    let capability = SkillsCapability;
    let section = capability.instructions().await.unwrap().unwrap();

    assert_eq!(
        section.source(),
        &PromptSource::Capability("skills".to_owned()),
        "a prompt dump has to name which capability spent these tokens"
    );
    assert_eq!(
        section.source().to_string(),
        format!("capability({})", capability.kind()),
        "the attribution in a dump must be the same value `kind` returns"
    );
    assert!(
        section.stability().is_stable() && section.position().is_prefix(),
        "a fragment resolved once per run belongs in the cached prefix"
    );
    assert_eq!(section.token_estimate(), estimate_tokens(section.content()));
}

#[test]
fn a_capability_ships_its_tools_and_the_families_they_depend_on() {
    let capability = SkillsCapability;

    let tools = capability.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].origin().qualified_name(), "load_skill");

    assert_eq!(
        capability.required_capabilities(),
        BTreeSet::from([CapabilityFamily::FILESYSTEM]),
        "a dependency written as a plain name must land on the same value as the constant"
    );
}

#[test]
fn binding_returns_the_per_run_form_instead_of_mutating_the_installed_one() {
    let installed = BindingCapability;

    let bound = installed
        .bind(&run_context("run-7"))
        .unwrap()
        .expect("a capability with per-run state supplies its bound form");

    assert_eq!(bound.kind(), CapabilityFamily::MEMORY);
    assert_eq!(
        bound
            .sampling_params(ModelSettings::new())
            .metadata()
            .get("run")
            .map(String::as_str),
        Some("run-7"),
        "the bound form is what carries the run it was bound to"
    );

    let other = installed
        .bind(&run_context("run-8"))
        .unwrap()
        .expect("binding again must produce a second value");
    assert_eq!(
        other
            .sampling_params(ModelSettings::new())
            .metadata()
            .get("run")
            .map(String::as_str),
        Some("run-8"),
        "one installed capability serves concurrent runs, so binding must not write into it"
    );
}

#[tokio::test]
async fn a_context_transform_is_the_processor_contract_rather_than_a_second_copy_of_it() {
    let capability = TrimmingCapability;
    let processor = capability
        .context_processor()
        .expect("a capability that transforms context exposes the processor it implements");

    let input = vec![user_item("first"), user_item("second")];
    let request = ContextProcessorRequest::new(
        RunId::new("run-9"),
        3,
        ItemId::new("item-1"),
        Some("test-model".to_owned()),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        input.clone(),
    );

    let result = processor
        .process_context(request, &RefusingSummarizer)
        .await
        .unwrap();

    assert_eq!(result.input(), &input[1..]);
    assert!(
        result.generated_items().is_empty() && result.model_responses().is_empty(),
        "a projection that asked for no summary owes no authoritative record and no spend"
    );
}
