//! R3-1c contracts for immutable agent declarations and their builder.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentInstructions, AgentSpec, HandoffSpec, ToolUseBehavior},
    error::{Error, Result},
    model::ModelSettings,
    output::OutputSchema,
    tool::{
        Tool, ToolContext, ToolExposure, ToolNamespace, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema,
    },
};
use serde_json::{Value, json};

struct EchoTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
}

impl EchoTool {
    fn new() -> Self {
        Self::with_origin(ToolOrigin::new("echo").unwrap())
    }

    fn namespaced(namespace: &str) -> Self {
        Self::with_origin(
            ToolOrigin::namespaced(ToolNamespace::new(namespace).unwrap(), "echo").unwrap(),
        )
    }

    fn with_origin(origin: ToolOrigin) -> Self {
        Self {
            origin,
            schema: ToolSchema::new(
                "echo",
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            options: ToolOptions::new(),
        }
    }

    fn with_options(mut self, options: ToolOptions) -> Self {
        self.options = options;
        self
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("echo"))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
}

#[test]
fn builder_returns_clear_error_when_required_identity_is_missing() {
    let missing_id = AgentSpec::builder().name("Reviewer").build().unwrap_err();
    assert!(missing_id.to_string().contains("requires a stable `id`"));

    let missing_name = AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .build()
        .unwrap_err();
    assert!(
        missing_name
            .to_string()
            .contains("requires a display `name`")
    );

    assert!(
        AgentSpec::builder()
            .id(AgentId::new(" reviewer "))
            .name("Reviewer")
            .build()
            .is_err()
    );
    assert!(
        AgentSpec::builder()
            .id(AgentId::new("reviewer"))
            .name(" ")
            .build()
            .is_err()
    );
}

#[test]
fn spec_preserves_unresolved_model_and_agent_layer_in_four_layer_merge() {
    let settings = ModelSettings::new()
        .with_temperature(0.2)
        .with_max_tokens(1_024);
    let agent = AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .instructions("Review carefully.\nReturn concrete findings.")
        .model("openai/gpt-5")
        .model_settings(settings)
        .build()
        .unwrap();

    assert_eq!(agent.id().as_str(), "reviewer");
    assert_eq!(agent.name(), "Reviewer");
    assert_eq!(
        agent.instructions().and_then(AgentInstructions::as_static),
        Some("Review carefully.\nReturn concrete findings.")
    );
    assert_eq!(agent.model(), Some("openai/gpt-5"));
    assert_eq!(agent.model_settings().temperature(), Some(0.2));
    assert_eq!(agent.model_settings().max_tokens(), Some(1_024));
}

#[test]
fn agents_with_same_name_are_distinguished_and_looked_up_by_stable_id() {
    let first = AgentSpec::builder()
        .id(AgentId::new("reviewer-primary"))
        .name("Reviewer")
        .build()
        .unwrap();
    let second = AgentSpec::builder()
        .id(AgentId::new("reviewer-shadow"))
        .name("Reviewer")
        .build()
        .unwrap();

    let by_id = BTreeMap::from([
        (first.id().clone(), Arc::clone(&first)),
        (second.id().clone(), Arc::clone(&second)),
    ]);

    assert_eq!(first.name(), second.name());
    assert_ne!(first.id(), second.id());
    assert!(Arc::ptr_eq(by_id.get(first.id()).unwrap(), &first));
    assert!(Arc::ptr_eq(by_id.get(second.id()).unwrap(), &second));
}

#[test]
fn spec_is_shared_by_arc_and_derived_builder_reuses_tool_implementations() {
    let tool: Arc<dyn Tool> = Arc::new(EchoTool::new());
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool(Arc::clone(&tool))
        .build()
        .unwrap();
    let shared = Arc::clone(&agent);
    assert!(Arc::ptr_eq(&agent, &shared));

    let variant = agent
        .to_builder()
        .name("Worker with a different display label")
        .build()
        .unwrap();
    assert_eq!(variant.id(), agent.id());
    assert_ne!(variant.name(), agent.name());
    assert!(Arc::ptr_eq(&variant.tools()[0], &tool));
}

#[test]
fn builder_rejects_duplicate_tool_identities() {
    let first: Arc<dyn Tool> = Arc::new(EchoTool::new());
    let second: Arc<dyn Tool> = Arc::new(EchoTool::new());

    let error = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([first, second])
        .build()
        .unwrap_err();

    assert!(error.to_string().contains("more than once"));
}

#[test]
fn builder_rejects_tools_projecting_to_the_same_model_facing_name() {
    let first: Arc<dyn Tool> = Arc::new(EchoTool::namespaced("server_a"));
    let second: Arc<dyn Tool> = Arc::new(EchoTool::namespaced("server_b"));

    // Their lookup keys differ because namespaces separate them, but their model-facing names
    // collide. Without this check, every turn would fail at the provider boundary.
    assert_ne!(
        first.origin().lookup_key(),
        second.origin().lookup_key(),
        "两个命名空间应当产生不同的查找键"
    );
    assert_eq!(
        first.model_definition().name(),
        second.model_definition().name()
    );

    let error = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([Arc::clone(&first), second])
        .build()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("advertises the tool name `echo`")
    );

    assert!(
        AgentSpec::builder()
            .id(AgentId::new("worker"))
            .name("Worker")
            .tool(first)
            .build()
            .is_ok()
    );
}

#[test]
fn builder_rejects_a_tool_and_handoff_with_the_same_model_facing_name() {
    let error = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .tool(Arc::new(EchoTool::new()))
        .handoff(HandoffSpec::new(
            AgentId::new("reviewer"),
            ToolSchema::new(
                "echo",
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
        ))
        .build()
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("tools and handoffs share one provider namespace")
    );
}

#[test]
fn builder_lets_a_hidden_tool_share_a_model_facing_name() {
    // The name rule covers the tools that can reach a model surface, not every declared tool. A
    // hidden tool is never in a tool list, so its name is ambiguous with nothing — and refusing
    // this pair would make an ordinary installation, a host-only tool beside an integration's
    // tool of the same name, impossible to declare.
    let advertised: Arc<dyn Tool> = Arc::new(EchoTool::namespaced("server_a"));
    let hidden: Arc<dyn Tool> = Arc::new(
        EchoTool::namespaced("host")
            .with_options(ToolOptions::new().with_exposure(ToolExposure::Hidden)),
    );
    assert_eq!(
        advertised.model_definition().name(),
        hidden.model_definition().name()
    );

    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tools([advertised, hidden])
        .build()
        .expect("only one of the two is ever offered to a model");

    // Both stay declared and dispatchable; the lookup keys are what keep them apart.
    assert_eq!(agent.tools().len(), 2);
}

#[test]
fn derived_builder_can_clear_inherited_optional_fields() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .instructions("Inherited instructions.")
        .model("openai/gpt-5")
        .tool(Arc::new(EchoTool::new()))
        .build()
        .unwrap();

    let stripped = agent
        .to_builder()
        .clear_instructions()
        .clear_model()
        .clear_tools()
        .build()
        .unwrap();

    assert!(stripped.instructions().is_none());
    assert_eq!(stripped.model(), None);
    assert!(stripped.tools().is_empty());
    // Derivation must not mutate the original; that is the point of the immutability contract.
    assert!(agent.instructions().is_some());
    assert_eq!(agent.model(), Some("openai/gpt-5"));
    assert_eq!(agent.tools().len(), 1);
}

fn review_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"approved": {"type": "boolean"}},
        "required": ["approved"],
        "additionalProperties": false
    })
}

fn reviewer(output_schema: OutputSchema) -> Result<Arc<AgentSpec>> {
    AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .output_schema(output_schema)
        .build()
}

#[test]
fn output_schema_is_immutable_and_derived_builders_can_restore_plain_text() {
    let output_schema = OutputSchema::json_schema("review", review_schema());
    let agent = reviewer(output_schema.clone()).unwrap();

    assert_eq!(agent.output_schema(), &output_schema);
    assert_eq!(agent.output_schema().name(), Some("review"));
    assert_eq!(agent.output_schema().strict(), Some(true));
    assert_eq!(
        agent
            .output_schema()
            .json_schema_value()
            .expect("a structured declaration has a schema"),
        &review_schema()
    );

    // Deriving keeps the promise unless the derivation says otherwise. A prepared execution
    // instance inherits it for the same reason it inherits tools: it stands in for this agent.
    let derived = agent.to_builder().model("openai/gpt-5").build().unwrap();
    assert_eq!(derived.output_schema(), &output_schema);

    let plain_text = agent.to_builder().clear_output_schema().build().unwrap();
    assert!(plain_text.output_schema().is_plain_text());
    assert!(!agent.output_schema().is_plain_text());

    let default_agent = AgentSpec::builder()
        .id(AgentId::new("plain-text"))
        .name("Plain Text")
        .build()
        .unwrap();
    assert!(default_agent.output_schema().is_plain_text());
}

#[test]
fn build_rejects_an_output_declaration_no_provider_would_accept() {
    // Each of these reaches the wire unchanged and is refused there, on every model call of every
    // run. Nothing between the declaration and the request inspects it, so this build is the only
    // place the mistake can still be attributed to the code that made it.
    let unnamed = reviewer(OutputSchema::json_schema("", review_schema())).unwrap_err();
    assert!(matches!(unnamed, Error::Config { .. }), "{unnamed:?}");

    let not_a_schema =
        reviewer(OutputSchema::json_schema("review", json!("approved"))).unwrap_err();
    assert!(
        matches!(not_a_schema, Error::Config { .. }),
        "{not_a_schema:?}"
    );

    // A strict declaration is a claim about the schema, and the claim is what is checked: the same
    // open-ended schema is a perfectly ordinary non-strict declaration.
    let open_ended = json!({
        "type": "object",
        "properties": {"approved": {"type": "boolean"}}
    });
    let unenforceable =
        reviewer(OutputSchema::json_schema("review", open_ended.clone())).unwrap_err();
    assert!(
        matches!(unenforceable, Error::Config { .. }),
        "{unenforceable:?}"
    );
    reviewer(OutputSchema::json_schema("review", open_ended).with_strict(false))
        .expect("a non-strict declaration makes no claim the provider will check");

    // Nesting is where a hand-written schema usually goes wrong: the root looks right and the
    // violation sits one level down, where only a recursive check finds it.
    let nested = json!({
        "type": "object",
        "properties": {
            "review": {
                "type": "object",
                "properties": {"approved": {"type": "boolean"}},
                "required": ["approved"]
            }
        },
        "required": ["review"],
        "additionalProperties": false
    });
    let nested_error = reviewer(OutputSchema::json_schema("review", nested)).unwrap_err();
    assert!(
        matches!(nested_error, Error::Config { .. }),
        "{nested_error:?}"
    );
    assert!(
        nested_error.to_string().contains("$.review"),
        "the error must point at the offending node: {nested_error}"
    );
}

#[test]
fn debug_does_not_leak_sensitive_instruction_or_model_setting_values() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .instructions("private-system-instructions")
        .model_settings(ModelSettings::new().with_extra_header("authorization", "secret-token"))
        .build()
        .unwrap();

    let debug = format!("{agent:?}");
    assert!(!debug.contains("private-system-instructions"));
    assert!(!debug.contains("secret-token"));
    assert!(debug.contains("worker"));
}

#[test]
fn shared_spec_satisfies_concurrency_and_static_lifetime_contract() {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<Arc<AgentSpec>>();
}

#[test]
fn tool_use_behavior_defaults_to_continue_and_is_preserved_by_derivation() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .tool_use_behavior(ToolUseBehavior::StopOnFirstTool)
        .build()
        .unwrap();
    assert!(matches!(
        agent.tool_use_behavior(),
        ToolUseBehavior::StopOnFirstTool
    ));

    let variant = agent.to_builder().name("Worker v2").build().unwrap();
    assert!(matches!(
        variant.tool_use_behavior(),
        ToolUseBehavior::StopOnFirstTool
    ));

    let default_agent = AgentSpec::builder()
        .id(AgentId::new("default-worker"))
        .name("Default Worker")
        .build()
        .unwrap();
    assert!(matches!(
        default_agent.tool_use_behavior(),
        ToolUseBehavior::RunLlmAgain
    ));
}
