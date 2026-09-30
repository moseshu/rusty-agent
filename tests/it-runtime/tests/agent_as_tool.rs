//! An agent exposed as a tool, run end to end. Ported from the reference `test_agent_as_tool.py`
//! where the case applies to this runner.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec, ToolUseBehavior},
    cancel::{CancelReason, CancelScope},
    context::{RunAgent, RunContext},
    error::{Error, Result},
    finish::FinishReason,
    guardrail::{
        GuardrailFinalOutput, GuardrailFunctionOutput, OutputGuardrail,
        ToolGuardrailFunctionOutput, ToolInputGuardrail, ToolInputGuardrailData,
        ToolOutputGuardrail, ToolOutputGuardrailData,
    },
    item::{
        CallId, ItemId, Message, MessageRole, ModelInputItem, ModelResponse, OutputPhase, RunItem,
        RunItemKind, ToolCall,
    },
    lifecycle::{LifecycleHook, LifecycleScope, ToolEndInput, ToolStartInput},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolGuardrailId, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_macros::ToolInput;
use ra_runtime::{
    agent::{
        AgentBinding,
        tool::{
            AgentAsTool, AgentTool, AgentToolErrorFunction, AgentToolOutputExtractor,
            STRUCTURED_INPUT_PREAMBLE, StructuredToolInputBuilder,
            StructuredToolInputBuilderOptions, StructuredToolInputResult,
            transform_string_function_style,
        },
    },
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// What one model call was handed.
#[derive(Debug, Clone)]
struct Call {
    model: Option<String>,
    instructions: Option<String>,
    input: Vec<ModelInputItem>,
    tools: Vec<String>,
}

/// One script shared by the parent and every nested run; `None` is a scripted model failure.
///
/// A nested run executes inside the parent's tool dispatch, so the order of model calls is fixed:
/// parent turn, the nested agent's turns, then the parent again.
type Script = Arc<Mutex<Vec<Option<ModelResponse>>>>;

struct ScriptedResolver {
    script: Script,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl ScriptedResolver {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Self::with_steps(script.into_iter().map(Some).collect())
    }

    fn with_steps(steps: Vec<Option<ModelResponse>>) -> Arc<Self> {
        Arc::new(Self {
            script: Arc::new(Mutex::new(steps)),
            calls: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl ModelResolver for ScriptedResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        let model = Arc::new(ScriptedModel {
            script: Arc::clone(&self.script),
            calls: Arc::clone(&self.calls),
            model_name: model_name.map(str::to_owned),
        });
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("canonical-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            model as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// Records each request under the model name it was resolved for, then answers from the script.
struct ScriptedModel {
    script: Script,
    calls: Arc<Mutex<Vec<Call>>>,
    model_name: Option<String>,
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.calls.lock().unwrap().push(Call {
            model: self.model_name.clone(),
            instructions: request.system_instructions().map(str::to_owned),
            input: request.input().to_vec(),
            tools: request
                .tools()
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect(),
        });
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        script
            .remove(0)
            .ok_or_else(|| Error::caller("the scripted model call failed"))
    }
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn final_message(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn tool_call(id: &str, call_id: &str, name: &str, arguments: Value) -> ModelResponse {
    ModelResponse::new(vec![item(
        id,
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, arguments)),
    )])
}

fn agent(id: &str, name: &str, instructions: &str) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name(name)
        .instructions(instructions)
        .build()
        .unwrap()
}

fn orchestrator(tool: AgentTool) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("orchestrator"))
        .name("Orchestrator")
        .instructions("orchestrate")
        .tool(Arc::new(tool))
        .build()
        .unwrap()
}

fn request(parent: Arc<AgentSpec>, resolver: &Arc<ScriptedResolver>) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(parent),
        Arc::clone(resolver) as Arc<dyn ModelResolver>,
        RunId::new("run-parent"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("please delegate"))],
    )
}

fn message_texts(input: &[ModelInputItem]) -> Vec<String> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) => Some(message.text_content()),
            _ => None,
        })
        .collect()
}

fn tool_output_texts(input: &[ModelInputItem]) -> Vec<String> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) => Some(
                ToolOutput::from_stored(output.output())
                    .ok()
                    .flatten()
                    .map(|output| {
                        output
                            .model_blocks()
                            .iter()
                            .filter_map(|block| block.as_text().map(str::to_owned))
                            .collect::<String>()
                    })
                    .unwrap_or_else(|| output.output().to_string()),
            ),
            _ => None,
        })
        .collect()
}

/// A plain tool for nested agents: answers with fixed text and records what it saw.
struct ProbeTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    output: String,
    seen_tool_input: SeenToolInput,
    started: Arc<tokio::sync::Notify>,
    hang: bool,
    dropped: Arc<AtomicBool>,
}

impl ProbeTool {
    fn new(name: &str, output: &str) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            options: ToolOptions::default(),
            output: output.to_owned(),
            seen_tool_input: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(tokio::sync::Notify::new()),
            hang: false,
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl Tool for ProbeTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.seen_tool_input
            .lock()
            .unwrap()
            .push(context.run().tool_input().cloned());
        if self.hang {
            let _guard = SetOnDrop(Arc::clone(&self.dropped));
            self.started.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text(self.output.clone()))
    }
}

// ---------------------------------------------------------------------------------------------
// Default input, output, and inheritance
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn agent_as_tool_returns_final_output_and_the_parent_continues() {
    let researcher = agent("researcher", "Researcher", "research things");
    let tool = researcher
        .as_tool()
        .tool_name("research")
        .tool_description("Research a topic.")
        .build()
        .unwrap();
    assert_eq!(tool.schema().description(), Some("Research a topic."));
    assert_eq!(
        tool.schema().input_schema(),
        &ra_runtime::agent::tool::AgentAsToolInput::json_schema()
    );

    let resolver = ScriptedResolver::new(vec![
        tool_call(
            "p-1",
            "call-1",
            "research",
            json!({"input": "rust ownership"}),
        ),
        final_message("n-1", "nested answer"),
        final_message("p-2", "done"),
    ]);
    let result = Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();
    assert_eq!(result.final_text(), "done");

    let calls = resolver.calls();
    assert_eq!(calls.len(), 3);
    // The nested agent starts from the generated input alone: no parent history, its own
    // instructions.
    assert_eq!(calls[1].instructions.as_deref(), Some("research things"));
    assert_eq!(calls[1].input.len(), 1);
    assert_eq!(message_texts(&calls[1].input), ["rust ownership"]);
    // The parent continues with the nested answer as the call's output.
    assert_eq!(tool_output_texts(&calls[2].input), ["nested answer"]);
}

#[tokio::test]
async fn default_tool_name_is_the_agent_name_in_function_style() {
    assert_eq!(
        transform_string_function_style("Research Agent-2!"),
        "research_agent_2_"
    );
    let tool = agent("a", "Research Agent", "x").as_tool().build().unwrap();
    assert_eq!(tool.origin().name(), "research_agent");
    assert_eq!(tool.schema().name(), "research_agent");
    assert_eq!(tool.agent().id(), &AgentId::new("a"));
}

#[tokio::test]
async fn agent_as_tool_rejects_colliding_derived_names() {
    let first = agent("writer-1", "Writer", "x").as_tool().build().unwrap();
    let second = agent("writer-2", "writer", "y").as_tool().build().unwrap();
    let error = AgentSpec::builder()
        .id(AgentId::new("orchestrator"))
        .name("Orchestrator")
        .tool(Arc::new(first))
        .tool(Arc::new(second))
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("writer"), "{error}");

    // An explicit name resolves the collision.
    let first = agent("writer-1", "Writer", "x").as_tool().build().unwrap();
    let second = agent("writer-2", "writer", "y")
        .as_tool()
        .tool_name("writer_two")
        .build()
        .unwrap();
    AgentSpec::builder()
        .id(AgentId::new("orchestrator"))
        .name("Orchestrator")
        .tool(Arc::new(first))
        .tool(Arc::new(second))
        .build()
        .unwrap();
}

/// Tool name, call ID, arguments, and parent run of one nested result.
type SeenInvocation = (String, CallId, Value, Option<RunId>);

struct CapturingExtractor {
    seen: Arc<Mutex<Vec<SeenInvocation>>>,
}

#[async_trait]
impl AgentToolOutputExtractor for CapturingExtractor {
    async fn extract(&self, result: &RunResult) -> Result<String> {
        let invocation = result.agent_tool_invocation().expect("nested invocation");
        self.seen.lock().unwrap().push((
            invocation.tool_name().to_owned(),
            invocation.tool_call_id().clone(),
            invocation.tool_arguments().clone(),
            result.state().parent_run_id().cloned(),
        ));
        Ok(format!("extracted: {}", result.final_text()))
    }
}

#[tokio::test]
async fn custom_output_extractor_sees_the_invocation_and_the_parent_run() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tool = agent("summarizer", "Summarizer", "summarize")
        .as_tool()
        .custom_output_extractor(Arc::new(CapturingExtractor {
            seen: Arc::clone(&seen),
        }))
        .build()
        .unwrap();

    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-7", "summarizer", json!({"input": "long text"})),
        final_message("n-1", "short"),
        final_message("p-2", "done"),
    ]);
    Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(
        *seen,
        [(
            "summarizer".to_owned(),
            CallId::new("call-7"),
            json!({"input": "long text"}),
            Some(RunId::new("run-parent")),
        )]
    );
    assert_eq!(
        tool_output_texts(&resolver.calls()[2].input),
        ["extracted: short"]
    );
}

#[tokio::test]
async fn fallback_returns_the_most_recent_output_when_there_is_no_final_answer() {
    let echo = ProbeTool::new("echo", "Newest tool output");
    let nested = AgentSpec::builder()
        .id(AgentId::new("summarizer"))
        .name("Summarizer")
        .instructions("summarize")
        .tool(Arc::new(echo))
        .tool_use_behavior(ToolUseBehavior::StopOnFirstTool)
        .build()
        .unwrap();
    let tool = nested.as_tool().build().unwrap();

    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "summarizer", json!({"input": "go"})),
        ModelResponse::new(vec![
            item(
                "n-1",
                RunItemKind::Message(Message::assistant(
                    "Older message output",
                    OutputPhase::Commentary,
                )),
            ),
            item(
                "n-2",
                RunItemKind::ToolCall(ToolCall::new(CallId::new("n-call"), "echo", json!({}))),
            ),
        ]),
        final_message("p-2", "done"),
    ]);
    Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();
    assert_eq!(
        tool_output_texts(&resolver.calls()[2].input),
        ["Newest tool output"]
    );
}

struct PassingGuardrail;

#[async_trait]
impl OutputGuardrail for PassingGuardrail {
    fn name(&self) -> &str {
        "passing"
    }

    async fn check(
        &self,
        _context: &RunContext,
        _output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        Ok(GuardrailFunctionOutput::pass())
    }
}

#[tokio::test]
async fn an_empty_answer_checked_by_output_guardrails_is_kept() {
    let nested = AgentSpec::builder()
        .id(AgentId::new("checked"))
        .name("Checked")
        .instructions("answer")
        .output_guardrail(Arc::new(PassingGuardrail))
        .build()
        .unwrap();
    let tool = nested.as_tool().build().unwrap();

    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "checked", json!({"input": "go"})),
        ModelResponse::new(vec![
            item(
                "n-0",
                RunItemKind::Message(Message::assistant(
                    "thinking aloud",
                    OutputPhase::Commentary,
                )),
            ),
            item(
                "n-1",
                RunItemKind::Message(Message::assistant("", OutputPhase::Final)),
            ),
        ]),
        final_message("p-2", "done"),
    ]);
    Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();
    // The guardrail approved the empty answer; the earlier commentary does not stand in for it.
    assert_eq!(tool_output_texts(&resolver.calls()[2].input), [""]);
}

#[tokio::test]
async fn nested_run_inherits_the_parent_run_config_when_not_set() {
    let tool = agent("nested", "Nested", "n").as_tool().build().unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        final_message("n-1", "ok"),
        final_message("p-2", "done"),
    ]);
    Runner::run(
        request(orchestrator(tool), &resolver)
            .with_config(RunConfig::new().with_model("parent-model")),
    )
    .await
    .unwrap();
    let models: Vec<_> = resolver
        .calls()
        .into_iter()
        .map(|call| call.model)
        .collect();
    assert_eq!(
        models,
        [
            Some("parent-model".to_owned()),
            Some("parent-model".to_owned()),
            Some("parent-model".to_owned())
        ]
    );
}

#[tokio::test]
async fn an_explicit_run_config_overrides_the_parent() {
    let tool = agent("nested", "Nested", "n")
        .as_tool()
        .run_config(RunConfig::new().with_model("nested-model"))
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        final_message("n-1", "ok"),
        final_message("p-2", "done"),
    ]);
    Runner::run(
        request(orchestrator(tool), &resolver)
            .with_config(RunConfig::new().with_model("parent-model")),
    )
    .await
    .unwrap();
    let models: Vec<_> = resolver
        .calls()
        .into_iter()
        .map(|call| call.model)
        .collect();
    assert_eq!(
        models,
        [
            Some("parent-model".to_owned()),
            Some("nested-model".to_owned()),
            Some("parent-model".to_owned())
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Turn cap and failures
// ---------------------------------------------------------------------------------------------

fn looping_nested(max_turns: u32) -> AgentTool {
    let nested = AgentSpec::builder()
        .id(AgentId::new("looper"))
        .name("Looper")
        .instructions("loop")
        .tool(Arc::new(ProbeTool::new("step", "stepped")))
        .build()
        .unwrap();
    nested.as_tool().max_turns(max_turns).build().unwrap()
}

#[tokio::test]
async fn a_nested_run_that_hits_its_turn_cap_fails_the_call_visibly() {
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "looper", json!({"input": "x"})),
        tool_call("n-1", "n-call-1", "step", json!({})),
        final_message("p-2", "recovered"),
    ]);
    // The parent's own cap is larger; the nested run is held to the tool's.
    let result = Runner::run(
        request(orchestrator(looping_nested(1)), &resolver)
            .with_config(RunConfig::new().with_max_turns(10)),
    )
    .await
    .unwrap();
    assert_eq!(result.final_text(), "recovered");
    let outputs = tool_output_texts(&resolver.calls()[2].input);
    assert_eq!(outputs.len(), 1);
    // Model-visible failures are rendered as a stable code, never as framework prose.
    assert!(outputs[0].contains("tool.execution_failed"), "{outputs:?}");
}

#[tokio::test]
async fn propagate_failures_stops_the_parent_turn() {
    let tool = agent("nested", "Nested", "n")
        .as_tool()
        .propagate_failures()
        .build()
        .unwrap();
    let resolver = ScriptedResolver::with_steps(vec![
        Some(tool_call("p-1", "call-1", "nested", json!({"input": "x"}))),
        None,
    ]);
    let error = Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("scripted model call failed"),
        "{error}"
    );
}

struct Apologize;

#[async_trait]
impl AgentToolErrorFunction for Apologize {
    async fn error_message(&self, context: &ToolContext<'_>, error: &Error) -> Result<String> {
        Ok(format!(
            "{} could not answer: {}",
            context.origin().name(),
            error.code()
        ))
    }
}

#[tokio::test]
async fn failure_error_function_renders_the_failure_for_the_model() {
    let tool = agent("nested", "Nested", "n")
        .as_tool()
        .failure_error_function(Arc::new(Apologize))
        .build()
        .unwrap();
    let resolver = ScriptedResolver::with_steps(vec![
        Some(tool_call("p-1", "call-1", "nested", json!({"input": "x"}))),
        None,
        Some(final_message("p-2", "handled")),
    ]);
    let result = Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();
    assert_eq!(result.final_text(), "handled");
    let outputs = tool_output_texts(&resolver.calls()[2].input);
    assert_eq!(outputs.len(), 1);
    assert!(
        outputs[0].starts_with("nested could not answer:"),
        "{outputs:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Structured input
// ---------------------------------------------------------------------------------------------

/// Arguments for a translation request.
#[derive(Debug, Deserialize, Serialize, JsonSchema, ToolInput)]
struct TranslationInput {
    /// Text to translate.
    text: String,
    /// Target language code.
    target: String,
}

/// The `tool_input` each probe call observed.
type SeenToolInput = Arc<Mutex<Vec<Option<Value>>>>;

fn translator_with_probe() -> (Arc<AgentSpec>, SeenToolInput) {
    let probe = ProbeTool::new("probe", "probed");
    let seen = Arc::clone(&probe.seen_tool_input);
    let nested = AgentSpec::builder()
        .id(AgentId::new("translator"))
        .name("Translator")
        .instructions("translate")
        .tool(Arc::new(probe))
        .build()
        .unwrap();
    (nested, seen)
}

#[tokio::test]
async fn structured_input_renders_data_and_exposes_tool_input_to_the_nested_run() {
    let (nested, seen) = translator_with_probe();
    let tool = nested
        .as_tool()
        .parameters::<TranslationInput>()
        .build()
        .unwrap();
    assert_eq!(
        tool.schema().input_schema()["required"],
        json!(["target", "text"])
    );

    let resolver = ScriptedResolver::new(vec![
        tool_call(
            "p-1",
            "call-1",
            "translator",
            json!({"text": "hola", "target": "en"}),
        ),
        tool_call("n-1", "n-call-1", "probe", json!({})),
        final_message("n-2", "hello"),
        final_message("p-2", "done"),
    ]);
    Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();

    let nested_input = message_texts(&resolver.calls()[1].input);
    assert_eq!(nested_input.len(), 1);
    let rendered = &nested_input[0];
    assert!(
        rendered.starts_with(STRUCTURED_INPUT_PREAMBLE),
        "{rendered}"
    );
    assert!(rendered.contains("\"text\": \"hola\""), "{rendered}");
    assert!(rendered.contains("## Input Schema Summary:"), "{rendered}");
    assert!(
        rendered.contains("- text (string, required) - Text to translate."),
        "{rendered}"
    );
    assert!(!rendered.contains("## Input JSON Schema:"), "{rendered}");
    assert_eq!(
        *seen.lock().unwrap(),
        [Some(json!({"text": "hola", "target": "en"}))]
    );
}

#[tokio::test]
async fn plain_agent_tools_leave_tool_input_empty() {
    let (nested, seen) = translator_with_probe();
    let tool = nested.as_tool().build().unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "translator", json!({"input": "hola"})),
        tool_call("n-1", "n-call-1", "probe", json!({})),
        final_message("n-2", "hello"),
        final_message("p-2", "done"),
    ]);
    Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), [None]);
}

#[tokio::test]
async fn include_input_schema_adds_the_full_schema_only_with_parameters() {
    let with_parameters = agent("translator", "Translator", "t")
        .as_tool()
        .parameters::<TranslationInput>()
        .include_input_schema(true)
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call(
            "p-1",
            "call-1",
            "translator",
            json!({"text": "hola", "target": "en"}),
        ),
        final_message("n-1", "hello"),
        final_message("p-2", "done"),
    ]);
    Runner::run(request(orchestrator(with_parameters), &resolver))
        .await
        .unwrap();
    let rendered = &message_texts(&resolver.calls()[1].input)[0];
    assert!(rendered.contains("## Input JSON Schema:"), "{rendered}");
    assert!(!rendered.contains("## Input Schema Summary:"), "{rendered}");

    // Without a parameter type there is no schema worth showing: the string passes unchanged.
    let without_parameters = agent("translator", "Translator", "t")
        .as_tool()
        .include_input_schema(true)
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "translator", json!({"input": "hola"})),
        final_message("n-1", "hello"),
        final_message("p-2", "done"),
    ]);
    Runner::run(request(orchestrator(without_parameters), &resolver))
        .await
        .unwrap();
    assert_eq!(message_texts(&resolver.calls()[1].input), ["hola"]);
}

#[tokio::test]
async fn a_custom_input_builder_supplies_the_nested_input_items() {
    let builder: Arc<dyn StructuredToolInputBuilder> = Arc::new(
        |options: StructuredToolInputBuilderOptions<'_>| -> Result<StructuredToolInputResult> {
            Ok(StructuredToolInputResult::Items(vec![
                ModelInputItem::Message(Message::text(MessageRole::System, "be brief")),
                ModelInputItem::Message(Message::user(format!(
                    "translate {}",
                    options.params()["text"].as_str().unwrap_or_default()
                ))),
            ]))
        },
    );
    let (nested, seen) = translator_with_probe();
    let tool = nested
        .as_tool()
        .parameters::<TranslationInput>()
        .input_builder(builder)
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call(
            "p-1",
            "call-1",
            "translator",
            json!({"text": "hola", "target": "en"}),
        ),
        tool_call("n-1", "n-call-1", "probe", json!({})),
        final_message("n-2", "hello"),
        final_message("p-2", "done"),
    ]);
    Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();
    assert_eq!(
        message_texts(&resolver.calls()[1].input),
        ["be brief", "translate hola"]
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [Some(json!({"text": "hola", "target": "en"}))]
    );
}

#[tokio::test]
async fn invalid_default_arguments_are_shown_to_the_model() {
    let tool = agent("nested", "Nested", "n").as_tool().build().unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": 42})),
        final_message("p-2", "done"),
    ]);
    let result = Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();
    assert_eq!(result.final_text(), "done");
    // The nested model was never called.
    assert_eq!(resolver.calls().len(), 2);
    let outputs = tool_output_texts(&resolver.calls()[1].input);
    assert!(outputs[0].contains("tool.invalid_input"), "{outputs:?}");
}

// ---------------------------------------------------------------------------------------------
// Enablement, approval of the call itself, and nested approvals
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn is_enabled_hides_the_tool_statically_and_per_run() {
    let disabled = agent("nested", "Nested", "n")
        .as_tool()
        .is_enabled(false)
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![final_message("p-1", "done")]);
    Runner::run(request(orchestrator(disabled), &resolver))
        .await
        .unwrap();
    assert!(resolver.calls()[0].tools.is_empty());

    let seen_agents = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen_agents);
    let dynamic = agent("nested", "Nested", "n")
        .as_tool()
        .is_enabled_fn(Arc::new(
            move |context: &RunContext, agent: &RunAgent| -> Result<bool> {
                recorder
                    .lock()
                    .unwrap()
                    .push((context.agent_id().clone(), agent.id().clone()));
                Ok(true)
            },
        ))
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![final_message("p-1", "done")]);
    Runner::run(request(orchestrator(dynamic), &resolver))
        .await
        .unwrap();
    assert_eq!(resolver.calls()[0].tools, ["nested"]);
    // The callback is told who is using the tool, not which agent the tool wraps.
    assert_eq!(
        *seen_agents.lock().unwrap(),
        [(AgentId::new("orchestrator"), AgentId::new("orchestrator"))]
    );
}

#[tokio::test]
async fn one_agent_tool_is_enabled_per_calling_agent() {
    let shared: Arc<dyn Tool> = Arc::new(
        agent("nested", "Nested", "n")
            .as_tool()
            .is_enabled_fn(Arc::new(
                |_context: &RunContext, agent: &RunAgent| -> Result<bool> {
                    Ok(agent.id() == &AgentId::new("alpha"))
                },
            ))
            .build()
            .unwrap(),
    );
    let parent = |id: &str| {
        AgentSpec::builder()
            .id(AgentId::new(id))
            .name(id)
            .instructions("orchestrate")
            .tool(Arc::clone(&shared))
            .build()
            .unwrap()
    };

    let resolver = ScriptedResolver::new(vec![final_message("a-1", "done")]);
    Runner::run(request(parent("alpha"), &resolver))
        .await
        .unwrap();
    assert_eq!(resolver.calls()[0].tools, ["nested"]);

    let resolver = ScriptedResolver::new(vec![final_message("b-1", "done")]);
    Runner::run(request(parent("beta"), &resolver))
        .await
        .unwrap();
    assert!(resolver.calls()[0].tools.is_empty());
}

#[tokio::test]
async fn needs_approval_interrupts_before_the_nested_run_starts() {
    let tool = agent("nested", "Nested", "n")
        .as_tool()
        .needs_approval(true)
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![tool_call(
        "p-1",
        "call-1",
        "nested",
        json!({"input": "x"}),
    )]);
    let result = Runner::run(request(orchestrator(tool), &resolver))
        .await
        .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Interrupted { .. }));
    assert_eq!(resolver.calls().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Approvals inside the nested run (the reference's nested approval bubbling and mirroring)
// ---------------------------------------------------------------------------------------------

fn gated_probe(name: &str, output: &str) -> ProbeTool {
    let mut probe = ProbeTool::new(name, output);
    probe.options = ToolOptions::default().with_approval(ToolApprovalPolicy::Always);
    probe
}

/// A nested agent whose one tool always asks for approval, and the log of that tool's calls.
fn guarded_nested() -> (Arc<AgentSpec>, SeenToolInput) {
    let guarded = gated_probe("guarded", "guarded output");
    let calls = Arc::clone(&guarded.seen_tool_input);
    let nested = AgentSpec::builder()
        .id(AgentId::new("nested"))
        .name("Nested")
        .instructions("n")
        .tool(Arc::new(guarded))
        .build()
        .unwrap();
    (nested, calls)
}

fn resume(parent: Arc<AgentSpec>, resolver: &Arc<ScriptedResolver>, state: RunState) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(parent),
        Arc::clone(resolver) as Arc<dyn ModelResolver>,
        RunId::new("run-parent"),
        CancelScope::root(),
        Vec::new(),
    )
    .with_state(state)
}

fn interruptions(result: &RunResult) -> Vec<RunItem> {
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("expected an interruption, got {:?}", result.outcome());
    };
    items.clone()
}

fn asked_tool(item: &RunItem) -> &str {
    let RunItemKind::ToolApproval(approval) = item.kind() else {
        panic!("expected an approval, got {item:?}");
    };
    approval.tool_name()
}

fn approve_all(result: &RunResult, always: bool) -> RunState {
    let mut state = result.state().clone();
    for item in interruptions(result) {
        state.approve(&item, always).unwrap();
    }
    state
}

fn two_calls(first: &str, second: &str) -> ModelResponse {
    ModelResponse::new(vec![
        item(
            "p-1a",
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(first),
                "nested",
                json!({"input": "x"}),
            )),
        ),
        item(
            "p-1b",
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(second),
                "nested",
                json!({"input": "y"}),
            )),
        ),
    ])
}

#[tokio::test]
async fn nested_approvals_are_asked_through_the_parent_and_resume_once_answered() {
    let (nested, guarded_calls) = guarded_nested();
    // Model-visible failure handling is the default, and it must not swallow the question.
    let parent = orchestrator(nested.as_tool().build().unwrap());
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        tool_call("n-1", "n-call-1", "guarded", json!({})),
        final_message("n-2", "nested done"),
        final_message("p-2", "done"),
    ]);

    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    let items = interruptions(&first);
    assert_eq!(items.len(), 1);
    assert_eq!(asked_tool(&items[0]), "guarded");
    assert_eq!(resolver.calls().len(), 2);
    assert!(guarded_calls.lock().unwrap().is_empty());
    // The call waits for its nested run: it has no output yet, so nothing reaches the model.
    assert!(
        !first
            .new_items()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(_)))
    );
    let state = first.state();
    assert!(state.pending_interruptions().is_empty());
    assert_eq!(
        state
            .pending_interruption_items()
            .cloned()
            .collect::<Vec<_>>(),
        items
    );
    let [paused] = state.nested_runs() else {
        panic!("one paused nested run is kept with the parent");
    };
    assert_eq!(paused.call_id(), &CallId::new("call-1"));
    assert_eq!(paused.scope_id(), "run-parent");
    assert_eq!(paused.arguments(), &json!({"input": "x"}));

    let mut state = state.clone();
    state.approve(&items[0], true).unwrap();
    assert!(
        state.permission_rules().is_empty(),
        "an always answer to a nested question is the nested run's rule"
    );
    let resumed = Runner::run(resume(parent, &resolver, state)).await.unwrap();

    assert!(
        matches!(resumed.outcome(), RunOutcome::Completed { .. }),
        "{:?}",
        resumed.outcome()
    );
    assert_eq!(resumed.final_text(), "done");
    assert_eq!(guarded_calls.lock().unwrap().len(), 1);
    assert!(resumed.state().nested_runs().is_empty());
    let calls = resolver.calls();
    assert_eq!(calls.len(), 4);
    // The nested run continued its own history instead of starting over.
    assert_eq!(tool_output_texts(&calls[2].input), ["guarded output"]);
    assert_eq!(tool_output_texts(&calls[3].input), ["nested done"]);
}

#[tokio::test]
async fn an_agent_tool_that_needs_approval_still_surfaces_its_nested_approval() {
    let (nested, guarded_calls) = guarded_nested();
    let parent = orchestrator(nested.as_tool().needs_approval(true).build().unwrap());
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        tool_call("n-1", "n-call-1", "guarded", json!({})),
        final_message("n-2", "hola"),
        final_message("p-2", "done"),
    ]);

    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    assert_eq!(asked_tool(&interruptions(&first)[0]), "nested");

    let second = Runner::run(resume(
        Arc::clone(&parent),
        &resolver,
        approve_all(&first, true),
    ))
    .await
    .unwrap();
    let items = interruptions(&second);
    assert_eq!(asked_tool(&items[0]), "guarded");
    assert_eq!(
        second.turns(),
        0,
        "the parent model has nothing new to read"
    );
    assert_eq!(resolver.calls().len(), 2);
    assert!(guarded_calls.lock().unwrap().is_empty());

    let last = Runner::run(resume(parent, &resolver, approve_all(&second, true)))
        .await
        .unwrap();
    assert_eq!(last.final_text(), "done");
    assert_eq!(guarded_calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_rejected_nested_approval_resumes_the_nested_run_with_the_refusal() {
    let (nested, guarded_calls) = guarded_nested();
    let parent = orchestrator(nested.as_tool().build().unwrap());
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        tool_call("n-1", "n-call-1", "guarded", json!({})),
        final_message("n-2", "rejected, so no"),
        final_message("p-2", "done"),
    ]);

    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    let mut state = first.state().clone();
    state.reject(&interruptions(&first)[0], false).unwrap();
    let resumed = Runner::run(resume(parent, &resolver, state)).await.unwrap();

    assert_eq!(resumed.final_text(), "done");
    assert!(guarded_calls.lock().unwrap().is_empty());
    let calls = resolver.calls();
    let [refusal] = tool_output_texts(&calls[2].input).try_into().unwrap();
    assert!(refusal.contains("approval_rejected"), "{refusal}");
    assert_eq!(tool_output_texts(&calls[3].input), ["rejected, so no"]);
}

/// Ported from the reference's `test_parent_approval_does_not_authorize_independent_nested_run`,
/// whose resume without an answer is interrupted again on the same question.
#[tokio::test]
async fn an_unanswered_nested_approval_is_asked_again_without_a_model_call() {
    let (nested, guarded_calls) = guarded_nested();
    let parent = orchestrator(nested.as_tool().build().unwrap());
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        tool_call("n-1", "n-call-1", "guarded", json!({})),
        final_message("n-2", "nested done"),
        final_message("p-2", "done"),
    ]);
    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();

    let again = Runner::run(resume(
        Arc::clone(&parent),
        &resolver,
        first.state().clone(),
    ))
    .await
    .unwrap();
    assert_eq!(interruptions(&again), interruptions(&first));
    assert_eq!(again.turns(), 0);
    assert_eq!(resolver.calls().len(), 2);
    assert!(guarded_calls.lock().unwrap().is_empty());

    let last = Runner::run(resume(parent, &resolver, approve_all(&again, false)))
        .await
        .unwrap();
    assert_eq!(last.final_text(), "done");
    assert_eq!(guarded_calls.lock().unwrap().len(), 1);
}

/// The paused nested run's questions, found through the call that started it.
fn questions_of(state: &RunState, call_id: &str) -> Vec<RunItem> {
    state
        .nested_runs()
        .iter()
        .find(|nested| nested.call_id() == &CallId::new(call_id))
        .and_then(|nested| nested.state())
        .expect("the call has a paused nested run")
        .pending_interruption_items()
        .cloned()
        .collect()
}

#[tokio::test]
async fn only_the_nested_runs_that_have_their_answers_continue() {
    let (nested, guarded_calls) = guarded_nested();
    let parent = orchestrator(nested.as_tool().build().unwrap());
    let resolver = ScriptedResolver::new(vec![
        two_calls("call-a", "call-b"),
        tool_call("n-1", "n-call-1", "guarded", json!({})),
        tool_call("n-2", "n-call-2", "guarded", json!({})),
        final_message("n-3", "nested done"),
        final_message("n-4", "nested done"),
        final_message("p-2", "done"),
    ]);
    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    let mut state = first.state().clone();
    let answered = questions_of(&state, "call-a");
    let waiting = questions_of(&state, "call-b");
    state.approve(&answered[0], false).unwrap();

    let second = Runner::run(resume(Arc::clone(&parent), &resolver, state))
        .await
        .unwrap();
    assert_eq!(interruptions(&second), waiting);
    assert_eq!(
        second.turns(),
        0,
        "the parent model waits for call-b's output"
    );
    assert_eq!(guarded_calls.lock().unwrap().len(), 1);
    let [still_paused] = second.state().nested_runs() else {
        panic!("only call-b is still paused");
    };
    assert_eq!(still_paused.call_id(), &CallId::new("call-b"));
    assert!(second.state().generated_items().iter().any(|item| matches!(
        item.kind(),
        RunItemKind::ToolCallOutput(output) if output.call_id() == &CallId::new("call-a")
    )));

    let last = Runner::run(resume(parent, &resolver, approve_all(&second, false)))
        .await
        .unwrap();
    assert_eq!(last.final_text(), "done");
    assert_eq!(guarded_calls.lock().unwrap().len(), 2);
    assert_eq!(
        tool_output_texts(&resolver.calls()[5].input),
        ["nested done", "nested done"]
    );
}

#[tokio::test]
async fn a_partly_answered_nested_run_waits_for_the_rest_and_asks_only_what_is_unanswered() {
    for reject_first in [false, true] {
        for deep in [false, true] {
            check_partial_nested_resume(reject_first, deep).await;
        }
    }
}

async fn check_partial_nested_resume(reject_first: bool, deep: bool) {
    let (nested, guarded_calls) = guarded_nested();
    let parent = if deep {
        let middle = AgentSpec::builder()
            .id(AgentId::new("middle"))
            .name("Middle")
            .instructions("delegate")
            .tool(Arc::new(nested.as_tool().build().unwrap()))
            .build()
            .unwrap();
        orchestrator(middle.as_tool().build().unwrap())
    } else {
        orchestrator(nested.as_tool().build().unwrap())
    };
    let both = ModelResponse::new(vec![
        item(
            "n-1a",
            RunItemKind::ToolCall(ToolCall::new(CallId::new("n-a"), "guarded", json!({}))),
        ),
        item(
            "n-1b",
            RunItemKind::ToolCall(ToolCall::new(CallId::new("n-b"), "guarded", json!({}))),
        ),
    ]);
    let mut script = Vec::new();
    if deep {
        script.push(tool_call(
            "root-1",
            "root-call",
            "middle",
            json!({"input": "x"}),
        ));
    }
    script.extend([
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        both,
        final_message("n-2", "nested done"),
    ]);
    if deep {
        script.push(final_message("middle-2", "middle done"));
    }
    script.push(final_message("p-2", "done"));
    let resolver = ScriptedResolver::new(script);
    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    let asked = interruptions(&first);
    assert_eq!(asked.len(), 2);
    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(first.state()).unwrap()).unwrap();
    if reject_first {
        state.reject(&asked[0], false).unwrap();
    } else {
        state.approve(&asked[0], false).unwrap();
    }

    let second = Runner::run(resume(Arc::clone(&parent), &resolver, state))
        .await
        .unwrap();
    // As the upstream run behaves: only the unanswered question is asked again, and nothing the
    // nested run asked about has run. An approval waits with the paused run; a rejection outranks
    // the open question, so the nested run is continued far enough to settle the refusal.
    assert_eq!(interruptions(&second), vec![asked[1].clone()]);
    assert!(guarded_calls.lock().unwrap().is_empty());
    assert_eq!(
        resolver.calls().len(),
        2 + usize::from(deep),
        "no model call while an approval remains"
    );
    let mut owner = second.state();
    while let [paused] = owner.nested_runs() {
        owner = paused.state().unwrap();
    }
    let refused = owner.generated_items().iter().any(|item| {
        matches!(
            item.kind(),
            RunItemKind::ToolCallOutput(output)
                if output.call_id() == &CallId::new("n-a")
                    && output.output().to_string().contains("approval_rejected")
        )
    });
    if reject_first {
        assert!(
            owner.pending_interruption_resolutions().is_empty(),
            "the rejection is settled at once"
        );
        assert!(refused, "the refusal is in the nested run's history");
    } else {
        assert_eq!(
            owner.pending_interruption_resolutions().len(),
            1,
            "the approval waits with the paused run"
        );
        assert!(!refused);
    }
    assert!(owner.has_unanswered_interruptions());

    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(second.state()).unwrap()).unwrap();
    state.approve(&asked[1], false).unwrap();
    let last = Runner::run(resume(parent, &resolver, state)).await.unwrap();
    assert_eq!(last.final_text(), "done");
    assert_eq!(
        guarded_calls.lock().unwrap().len(),
        1 + usize::from(!reject_first)
    );
    let calls = resolver.calls();
    let nested_outputs = tool_output_texts(&calls[2 + usize::from(deep)].input);
    assert_eq!(nested_outputs.len(), 2, "each call has exactly one output");
    if reject_first {
        assert!(nested_outputs[0].contains("approval_rejected"));
    }
}

/// Records the tool outputs each delivery it checks carries.
#[derive(Default)]
struct DeliveredOutputs(Mutex<Vec<Vec<String>>>);

struct RecordingGuardrail(Arc<DeliveredOutputs>);

#[async_trait]
impl OutputGuardrail for RecordingGuardrail {
    fn name(&self) -> &str {
        "recording"
    }

    async fn check(
        &self,
        _context: &RunContext,
        output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        self.0.0.lock().unwrap().push(
            output
                .tool_outputs()
                .iter()
                .map(|output| output.output().to_string())
                .collect(),
        );
        Ok(GuardrailFunctionOutput::pass())
    }
}

#[tokio::test]
async fn an_approved_call_ends_the_resumed_run_under_the_stop_policy() {
    let gated = gated_probe("write", "written");
    let calls = Arc::clone(&gated.seen_tool_input);
    let delivered = Arc::new(DeliveredOutputs::default());
    let parent = AgentSpec::builder()
        .id(AgentId::new("orchestrator"))
        .name("Orchestrator")
        .instructions("orchestrate")
        .tool(Arc::new(gated))
        .tool_use_behavior(ToolUseBehavior::StopOnFirstTool)
        .output_guardrail(Arc::new(RecordingGuardrail(Arc::clone(&delivered))))
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![tool_call("p-1", "call-1", "write", json!({}))]);
    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();

    let resumed = Runner::run(resume(parent, &resolver, approve_all(&first, false)))
        .await
        .unwrap();
    assert_eq!(
        resumed.outcome().finish_reason(),
        Some(FinishReason::ToolStop)
    );
    assert_eq!(resolver.calls().len(), 1, "no model call after the stop");
    assert_eq!(calls.lock().unwrap().len(), 1);
    let checked = delivered.0.lock().unwrap().clone();
    let [outputs] = checked.as_slice() else {
        panic!("the delivery is checked once, got {checked:?}");
    };
    assert_eq!(outputs.len(), 1);
    assert!(outputs[0].contains("written"), "{outputs:?}");
}

#[tokio::test]
async fn a_continued_agent_tool_ends_the_resumed_run_under_the_stop_policy() {
    let (nested, guarded_calls) = guarded_nested();
    let delivered = Arc::new(DeliveredOutputs::default());
    let parent = AgentSpec::builder()
        .id(AgentId::new("orchestrator"))
        .name("Orchestrator")
        .instructions("orchestrate")
        .tool(Arc::new(nested.as_tool().build().unwrap()))
        .tool_use_behavior(ToolUseBehavior::StopAtTools {
            names: ["nested".to_owned()].into_iter().collect(),
        })
        .output_guardrail(Arc::new(RecordingGuardrail(Arc::clone(&delivered))))
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        tool_call("n-1", "n-call-1", "guarded", json!({})),
        final_message("n-2", "nested done"),
    ]);
    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();

    let resumed = Runner::run(resume(parent, &resolver, approve_all(&first, false)))
        .await
        .unwrap();
    assert_eq!(
        resumed.outcome().finish_reason(),
        Some(FinishReason::ToolStop)
    );
    assert_eq!(
        resolver.calls().len(),
        3,
        "the parent model is not asked again"
    );
    assert_eq!(guarded_calls.lock().unwrap().len(), 1);
    let checked = delivered.0.lock().unwrap().clone();
    let [outputs] = checked.as_slice() else {
        panic!("the delivery is checked once, got {checked:?}");
    };
    assert_eq!(outputs.len(), 1);
    assert!(outputs[0].contains("nested done"), "{outputs:?}");
}

#[tokio::test]
async fn a_nested_run_that_pauses_again_is_asked_before_the_parent_model() {
    let (nested, guarded_calls) = guarded_nested();
    let parent = orchestrator(nested.as_tool().build().unwrap());
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        tool_call("n-1", "n-call-1", "guarded", json!({})),
        tool_call("n-2", "n-call-2", "guarded", json!({})),
        final_message("n-3", "nested done"),
        final_message("p-2", "done"),
    ]);

    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    let second = Runner::run(resume(
        Arc::clone(&parent),
        &resolver,
        approve_all(&first, false),
    ))
    .await
    .unwrap();
    let items = interruptions(&second);
    let RunItemKind::ToolApproval(approval) = items[0].kind() else {
        panic!("expected an approval");
    };
    assert_eq!(approval.call_id(), &CallId::new("n-call-2"));
    assert_eq!(second.turns(), 0);
    assert_eq!(resolver.calls().len(), 3);
    assert_eq!(guarded_calls.lock().unwrap().len(), 1);

    let last = Runner::run(resume(parent, &resolver, approve_all(&second, false)))
        .await
        .unwrap();
    assert_eq!(last.final_text(), "done");
    assert_eq!(guarded_calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_parent_approval_does_not_authorize_the_nested_run() {
    let parent_sensitive = gated_probe("sensitive", "outer");
    let parent_calls = Arc::clone(&parent_sensitive.seen_tool_input);
    let nested_sensitive = gated_probe("sensitive", "inner");
    let nested_calls = Arc::clone(&nested_sensitive.seen_tool_input);
    let nested = AgentSpec::builder()
        .id(AgentId::new("nested"))
        .name("Nested")
        .instructions("n")
        .tool(Arc::new(nested_sensitive))
        .build()
        .unwrap();
    let parent = AgentSpec::builder()
        .id(AgentId::new("orchestrator"))
        .name("Orchestrator")
        .instructions("orchestrate")
        .tool(Arc::new(parent_sensitive))
        .tool(Arc::new(nested.as_tool().build().unwrap()))
        .build()
        .unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "shared", "sensitive", json!({})),
        tool_call("p-2", "outer-nested", "nested", json!({"input": "x"})),
        tool_call("n-1", "shared", "sensitive", json!({})),
        final_message("n-2", "inner done"),
        final_message("p-3", "done"),
    ]);

    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    let second = Runner::run(resume(
        Arc::clone(&parent),
        &resolver,
        approve_all(&first, true),
    ))
    .await
    .unwrap();
    let items = interruptions(&second);
    assert_eq!(asked_tool(&items[0]), "sensitive");
    assert_eq!(parent_calls.lock().unwrap().len(), 1);
    assert!(
        nested_calls.lock().unwrap().is_empty(),
        "the parent's always rule must not answer the nested run's question"
    );

    let last = Runner::run(resume(parent, &resolver, approve_all(&second, false)))
        .await
        .unwrap();
    assert_eq!(last.final_text(), "done");
    assert_eq!(nested_calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_nested_call_id_may_repeat_the_parents() {
    let (nested, guarded_calls) = guarded_nested();
    let parent = orchestrator(nested.as_tool().build().unwrap());
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "shared", "nested", json!({"input": "x"})),
        tool_call("n-1", "shared", "guarded", json!({})),
        final_message("n-2", "inner done"),
        final_message("p-2", "outer done"),
    ]);

    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    assert_eq!(interruptions(&first).len(), 1);
    let resumed = Runner::run(resume(parent, &resolver, approve_all(&first, false)))
        .await
        .unwrap();

    assert_eq!(resumed.final_text(), "outer done");
    assert!(resumed.outcome().interruptions().is_empty());
    assert_eq!(guarded_calls.lock().unwrap().len(), 1);
    assert_eq!(
        tool_output_texts(&resolver.calls()[3].input),
        ["inner done"]
    );
}

#[tokio::test]
async fn parallel_agent_tool_calls_pause_and_resume_independently() {
    let (nested, guarded_calls) = guarded_nested();
    let parent = orchestrator(nested.as_tool().build().unwrap());
    let resolver = ScriptedResolver::new(vec![
        two_calls("call-a", "call-b"),
        tool_call("n-1", "n-call-1", "guarded", json!({})),
        tool_call("n-2", "n-call-2", "guarded", json!({})),
        final_message("n-3", "nested done"),
        final_message("n-4", "nested done"),
        final_message("p-2", "done"),
    ]);

    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    assert_eq!(interruptions(&first).len(), 2);
    assert_eq!(first.state().nested_runs().len(), 2);

    let resumed = Runner::run(resume(parent, &resolver, approve_all(&first, false)))
        .await
        .unwrap();
    assert_eq!(resumed.final_text(), "done");
    assert_eq!(guarded_calls.lock().unwrap().len(), 2);
    assert_eq!(
        tool_output_texts(&resolver.calls()[5].input),
        ["nested done", "nested done"]
    );
}

#[tokio::test]
async fn identical_nested_approvals_in_two_calls_are_refused_rather_than_guessed() {
    let (nested, guarded_calls) = guarded_nested();
    let parent = orchestrator(nested.as_tool().build().unwrap());
    let resolver = ScriptedResolver::new(vec![
        two_calls("call-a", "call-b"),
        tool_call("n-1", "dup", "guarded", json!({})),
        tool_call("n-1", "dup", "guarded", json!({})),
    ]);

    let first = Runner::run(request(Arc::clone(&parent), &resolver))
        .await
        .unwrap();
    let items = interruptions(&first);
    assert_eq!(items.len(), 2);
    let error = first.state().clone().approve(&items[0], false).unwrap_err();
    assert!(error.to_string().contains("unique call IDs"), "{error}");
    assert!(guarded_calls.lock().unwrap().is_empty());
}

/// Counts what happens around the `delegate` call: its checks, its narration, and its extractor.
#[derive(Default)]
struct OuterCallbacks {
    events: Mutex<Vec<&'static str>>,
}

impl OuterCallbacks {
    fn push(&self, event: &'static str) {
        self.events.lock().unwrap().push(event);
    }

    fn count(&self, event: &str) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| **seen == event)
            .count()
    }
}

struct CountingHook(Arc<OuterCallbacks>);

#[async_trait]
impl LifecycleHook for CountingHook {
    fn name(&self) -> &str {
        "counting"
    }

    async fn on_tool_start(
        &self,
        _scope: LifecycleScope,
        input: &ToolStartInput<'_>,
    ) -> Result<()> {
        if input.origin().qualified_name() == "delegate" {
            self.0.push("start");
        }
        Ok(())
    }

    async fn on_tool_end(&self, _scope: LifecycleScope, input: &ToolEndInput<'_>) -> Result<()> {
        if input.origin().qualified_name() == "delegate" {
            self.0.push("end");
        }
        Ok(())
    }
}

struct CountingCheck {
    id: ToolGuardrailId,
    callbacks: Arc<OuterCallbacks>,
}

#[async_trait]
impl ToolInputGuardrail for CountingCheck {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }

    async fn check(
        &self,
        _data: &ToolInputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        self.callbacks.push("input_guardrail");
        Ok(ToolGuardrailFunctionOutput::allow())
    }
}

#[async_trait]
impl ToolOutputGuardrail for CountingCheck {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }

    async fn check(
        &self,
        _data: &ToolOutputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        self.callbacks.push("output_guardrail");
        Ok(ToolGuardrailFunctionOutput::allow())
    }
}

/// Ported from the reference's `test_nested_agent_tool_continuation_runs_outer_callbacks_once`.
#[tokio::test]
async fn a_serialized_nested_pause_resumes_and_runs_the_outer_callbacks_once() {
    let callbacks = Arc::new(OuterCallbacks::default());
    let (nested, guarded_calls) = guarded_nested();
    let extractor_callbacks = Arc::clone(&callbacks);
    let input_id = ToolGuardrailId::new("track_input").unwrap();
    let output_id = ToolGuardrailId::new("track_output").unwrap();
    let tool = nested
        .as_tool()
        .tool_name("delegate")
        .custom_output_extractor(Arc::new(move |result: &RunResult| {
            extractor_callbacks.push("custom_output");
            Ok(result.final_text())
        }))
        .options(
            ToolOptions::default()
                .with_input_guardrail(input_id.clone())
                .with_output_guardrail(output_id.clone()),
        )
        .build()
        .unwrap();
    let parent = orchestrator(tool);
    let config = RunConfig::new()
        .with_lifecycle_hook(Arc::new(CountingHook(Arc::clone(&callbacks))))
        .with_tool_input_guardrail(Arc::new(CountingCheck {
            id: input_id,
            callbacks: Arc::clone(&callbacks),
        }))
        .with_tool_output_guardrails([Arc::new(CountingCheck {
            id: output_id,
            callbacks: Arc::clone(&callbacks),
        }) as Arc<dyn ToolOutputGuardrail>]);
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "outer-call", "delegate", json!({"input": "hello"})),
        tool_call("n-1", "inner-call", "guarded", json!({})),
        final_message("n-2", "nested done"),
        final_message("p-2", "outer done"),
    ]);

    let first = Runner::run(request(Arc::clone(&parent), &resolver).with_config(config.clone()))
        .await
        .unwrap();
    assert_eq!(interruptions(&first).len(), 1);
    assert_eq!(callbacks.count("input_guardrail"), 1);
    assert_eq!(callbacks.count("start"), 1);
    assert_eq!(callbacks.count("output_guardrail"), 0);
    assert_eq!(callbacks.count("custom_output"), 0);
    assert_eq!(callbacks.count("end"), 0);

    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(first.state()).unwrap()).unwrap();
    let asked = state
        .pending_interruption_items()
        .cloned()
        .collect::<Vec<_>>();
    state.approve(&asked[0], false).unwrap();
    let last = Runner::run(resume(parent, &resolver, state).with_config(config))
        .await
        .unwrap();

    assert_eq!(last.final_text(), "outer done");
    assert_eq!(guarded_calls.lock().unwrap().len(), 1);
    for event in [
        "input_guardrail",
        "start",
        "output_guardrail",
        "custom_output",
        "end",
    ] {
        assert_eq!(callbacks.count(event), 1, "{event}");
    }
}

// ---------------------------------------------------------------------------------------------
// Boundaries
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_agent_tool_called_outside_a_runner_is_a_caller_error() {
    let tool = agent("nested", "Nested", "n").as_tool().build().unwrap();
    let parent = agent("parent", "Parent", "p");
    let run = RunContext::new(RunId::new("run-direct"), &parent);
    let call_id = CallId::new("call-1");
    let arguments = json!({"input": "x"});
    let error = tool
        .call(ToolContext::new(&run, &tool, &call_id, &arguments))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Caller { .. }), "{error:?}");
}

#[tokio::test]
async fn cancelling_the_parent_reaches_the_nested_run() {
    let mut hanging = ProbeTool::new("hang", "never");
    hanging.hang = true;
    let started = Arc::clone(&hanging.started);
    let dropped = Arc::clone(&hanging.dropped);
    let nested = AgentSpec::builder()
        .id(AgentId::new("nested"))
        .name("Nested")
        .instructions("n")
        .tool(Arc::new(hanging))
        .build()
        .unwrap();
    let tool = nested.as_tool().build().unwrap();
    let resolver = ScriptedResolver::new(vec![
        tool_call("p-1", "call-1", "nested", json!({"input": "x"})),
        tool_call("n-1", "n-call-1", "hang", json!({})),
    ]);
    let root = CancelScope::root();
    let run = tokio::spawn(Runner::run(RunRequest::new(
        AgentBinding::direct(orchestrator(tool)),
        Arc::clone(&resolver) as Arc<dyn ModelResolver>,
        RunId::new("run-parent"),
        root.clone(),
        vec![ModelInputItem::Message(Message::user("go"))],
    )));
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("the nested tool started");
    root.cancel(CancelReason::UserInterrupt);
    let error = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the parent stopped")
        .unwrap()
        .unwrap_err();
    assert!(error.is_cancelled(), "{error:?}");
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn zero_max_turns_is_rejected_at_build() {
    let error = agent("nested", "Nested", "n")
        .as_tool()
        .max_turns(0)
        .build()
        .unwrap_err();
    assert!(matches!(error, Error::Config { .. }), "{error:?}");
}
