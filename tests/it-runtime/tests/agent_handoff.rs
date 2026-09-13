//! Contracts for a transfer of control inside the run loop.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ra_core::{
    agent::{
        AgentId, AgentSpec, HandoffInputData, HandoffInputFilter, HandoffSpec, HistoryProjection,
    },
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    guardrail::{GuardrailFinalOutput, GuardrailFunctionOutput, OutputGuardrail},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    state::{RunId, RunState},
    tool::ToolSchema,
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    runner::result::ContinuationInput,
    runner::{RunConfig, RunRequest, Runner},
};
use serde_json::json;

/// A model that replays a fixed script and records what each call was handed.
struct ScriptedModel {
    script: Mutex<Vec<ModelResponse>>,
    inputs: Mutex<Vec<Vec<ModelInputItem>>>,
    instructions: Mutex<Vec<Option<String>>>,
    handoff_names: Mutex<Vec<Vec<String>>>,
    calls: AtomicUsize,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            inputs: Mutex::new(Vec::new()),
            instructions: Mutex::new(Vec::new()),
            handoff_names: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn next_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(request.input().to_vec());
        self.instructions
            .lock()
            .unwrap()
            .push(request.system_instructions().map(str::to_owned));
        self.handoff_names.lock().unwrap().push(
            request
                .handoffs()
                .iter()
                .map(|handoff| handoff.name().to_owned())
                .collect(),
        );
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }

    /// The kinds one model call was handed, in order.
    fn input_labels(&self, call: usize) -> Vec<&'static str> {
        self.inputs.lock().unwrap()[call]
            .iter()
            .map(ModelInputItem::label)
            .collect()
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.next_response(request)
    }
}

struct FixedResolver {
    model: Arc<ScriptedModel>,
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

/// Records that it ran, so a run can say whose promise was checked at delivery.
struct RecordingGuardrail {
    name: &'static str,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl OutputGuardrail for RecordingGuardrail {
    fn name(&self) -> &str {
        self.name
    }

    async fn check(
        &self,
        _context: &RunContext,
        _output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        self.seen.lock().unwrap().push(self.name);
        Ok(GuardrailFunctionOutput::pass())
    }
}

/// Keeps only the caller's opening input, dropping everything the projection let through.
struct KeepOnlyOpeningInput;

#[async_trait]
impl HandoffInputFilter for KeepOnlyOpeningInput {
    async fn filter(
        &self,
        _context: &RunContext,
        data: HandoffInputData,
    ) -> Result<HandoffInputData> {
        Ok(data
            .with_pre_handoff_items(Vec::new())
            .with_new_items(Vec::new()))
    }
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn message(id: &str, text: &str) -> RunItem {
    item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn commentary(id: &str, text: &str) -> RunItem {
    item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Commentary)),
    )
}

fn transfer_call(id: &str, call_id: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            "transfer_to_reviewer",
            json!({ "reason": "需要审阅" }),
        )),
    )
}

fn handoff_schema() -> ToolSchema {
    ToolSchema::new(
        "transfer_to_reviewer",
        json!({
            "type": "object",
            "properties": { "reason": { "type": "string" } },
            "required": ["reason"],
            "additionalProperties": false
        }),
    )
    .unwrap()
}

fn reviewer(output_guardrails: Vec<Arc<dyn OutputGuardrail>>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .instructions("review the work")
        .output_guardrails(output_guardrails)
        .build()
        .unwrap()
}

fn planner(
    handoff: HandoffSpec,
    output_guardrails: Vec<Arc<dyn OutputGuardrail>>,
) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .instructions("plan the work")
        .handoff(handoff)
        .output_guardrails(output_guardrails)
        .build()
        .unwrap()
}

/// A planner that transfers to a reviewer, which then answers.
fn script() -> Arc<ScriptedModel> {
    ScriptedModel::new(vec![
        ModelResponse::new(vec![
            commentary("msg-1", "这件事该交给审阅者"),
            transfer_call("call-item-1", "call-1"),
        ]),
        ModelResponse::new(vec![message("msg-2", "审阅完成")]),
    ])
}

fn run_request(
    planner: Arc<AgentSpec>,
    reviewer: Arc<AgentSpec>,
    model: &Arc<ScriptedModel>,
    config: RunConfig,
) -> RunRequest {
    let registry = AgentRegistry::builder()
        .register(Arc::clone(&planner))
        .register(reviewer)
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(planner),
        Arc::new(FixedResolver {
            model: Arc::clone(model),
        }),
        RunId::new("run-handoff"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("帮我处理这件事"))],
    )
    .with_config(config.with_agent_registry(registry))
}

#[tokio::test]
async fn control_moves_to_the_target_and_the_session_keeps_what_the_projection_withheld() {
    let model = script();
    let result = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema()),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();

    // The run is delivered by whoever ended up holding the answer.
    assert_eq!(result.turns(), 2);
    assert_eq!(result.last_agent().id().as_str(), "reviewer");
    assert_eq!(result.final_text(), "审阅完成");
    assert_eq!(result.state().current_agent().unwrap().as_str(), "reviewer");

    // The second turn runs as the receiving agent: its instructions, and no handoff of its own.
    let instructions = model.instructions.lock().unwrap().clone();
    assert_eq!(
        instructions,
        [
            Some("plan the work".to_owned()),
            Some("review the work".to_owned())
        ]
    );
    let advertised = model.handoff_names.lock().unwrap().clone();
    assert_eq!(advertised[0], ["transfer_to_reviewer"]);
    assert!(advertised[1].is_empty());

    // `HistoryProjection::None` is the default: the receiving agent is handed the brief that moved
    // control and nothing else — not the caller's request, and not the planner's narration.
    assert_eq!(model.input_labels(0), ["message"]);
    assert_eq!(model.input_labels(1), ["tool_call", "handoff_output"]);

    // The session is authoritative and keeps every record, including the ones the projection
    // withheld from the receiving agent.
    let stored = result
        .state()
        .generated_items()
        .iter()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(stored, ["msg-1", "call-item-1", "call-1.output", "msg-2"]);

    // Records stay attributed to whoever produced them, so a transcript read back still says which
    // agent said what.
    let authors = result
        .state()
        .generated_items()
        .iter()
        .filter_map(|item| item.provenance().map(|p| p.agent_id().as_str().to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(authors, ["planner", "planner", "planner", "reviewer"]);

    // The turn records say the same thing, one turn at a time.
    let agents = result
        .turn_records()
        .iter()
        .map(|record| record.agent().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(agents, ["planner", "reviewer"]);
    assert_eq!(result.turn_records()[0].next_step_code(), "handoff");
}

#[tokio::test]
async fn a_full_projection_hands_the_whole_conversation_over() {
    let model = script();
    let result = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::Full),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();

    assert_eq!(result.last_agent().id().as_str(), "reviewer");
    assert_eq!(
        model.input_labels(1),
        ["message", "message", "tool_call", "handoff_output"]
    );
}

#[tokio::test]
async fn an_input_filter_narrows_what_the_declaration_allowed_through() {
    let model = script();
    let result = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::Full)
                .with_input_filter(Arc::new(KeepOnlyOpeningInput)),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();

    // The filter dropped both record lists, so only the caller's opening input survives — and the
    // session still stores everything the turn produced.
    assert_eq!(model.input_labels(1), ["message"]);
    assert_eq!(result.state().generated_items().len(), 4);
}

#[tokio::test]
async fn the_projection_survives_a_checkpoint_and_governs_the_next_segment() {
    let model = script();
    let paused = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema()),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new().with_max_turns(1),
    ))
    .await
    .unwrap();

    // A host that continues the run itself is handed the transfer's view, not the segment's. The
    // convenience projection would otherwise be the one place the withheld history leaks out, with
    // nothing about the call saying so.
    let continuation = paused.continuation_input(ContinuationInput::PreserveAll);
    assert_eq!(
        continuation
            .iter()
            .map(ModelInputItem::label)
            .collect::<Vec<_>>(),
        ["tool_call", "handoff_output"]
    );

    // The first segment stopped on its turn cap, with control already transferred.
    assert_eq!(paused.state().current_agent().unwrap().as_str(), "reviewer");
    let restored: RunState =
        serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
    let (base, carried) = restored.model_input_base().unwrap();
    assert_eq!(base.len(), 2);
    assert!(carried.is_empty());

    // A continuation from that checkpoint builds its request from the projection rather than from
    // the history the session kept.
    let resumed = Runner::run(
        RunRequest::new(
            AgentBinding::direct(reviewer(Vec::new())),
            Arc::new(FixedResolver {
                model: Arc::clone(&model),
            }),
            RunId::new("run-handoff"),
            CancelScope::root(),
            Vec::new(),
        )
        .with_state(restored)
        .with_config(RunConfig::new()),
    )
    .await
    .unwrap();

    assert_eq!(resumed.final_text(), "审阅完成");
    assert_eq!(model.input_labels(1), ["tool_call", "handoff_output"]);
}

#[tokio::test]
async fn delivery_is_checked_by_the_agent_that_answered_not_the_one_that_started() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let model = script();
    let result = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema()),
            vec![Arc::new(RecordingGuardrail {
                name: "planner_guard",
                seen: Arc::clone(&seen),
            })],
        ),
        reviewer(vec![Arc::new(RecordingGuardrail {
            name: "reviewer_guard",
            seen: Arc::clone(&seen),
        })]),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();

    // The answer belongs to the reviewer, and so does the promise about what may be in it. The
    // planner's check examines an answer it did not write, so it does not run.
    assert_eq!(*seen.lock().unwrap(), ["reviewer_guard"]);
    assert_eq!(result.output_guardrail_results().len(), 1);
}

#[tokio::test]
async fn a_target_the_registry_cannot_resolve_is_refused_before_the_model_is_called() {
    // The registry builder already refuses a declaration set with a missing target, so the gap a
    // run actually hits is the one this covers: an agent that declares a transfer while the run was
    // given no registry to resolve it against.
    let model = script();
    let error = Runner::run(RunRequest::new(
        AgentBinding::direct(planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema()),
            Vec::new(),
        )),
        Arc::new(FixedResolver {
            model: Arc::clone(&model),
        }),
        RunId::new("run-handoff"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("帮我处理这件事"))],
    ))
    .await
    .unwrap_err();

    // Advertising a transfer nothing can execute would spend a model call to reach a dead end.
    assert!(error.to_string().contains("reviewer"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_summary_projection_is_refused_before_it_is_advertised() {
    let model = script();
    let error = Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema())
                .with_history_projection(HistoryProjection::Summary),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap_err();

    assert!(error.to_string().contains("summary history projection"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn typed_handoff_uses_the_named_history_projection() {
    let narrow = HandoffSpec::new(AgentId::new("reviewer"), handoff_schema());
    let wide = HandoffSpec::new(
        AgentId::new("reviewer"),
        ToolSchema::new(
            "transfer_full",
            json!({
                "type": "object", "properties": {}, "required": [], "additionalProperties": false
            }),
        )
        .unwrap(),
    )
    .with_history_projection(HistoryProjection::Full);
    let source = planner(narrow, Vec::new())
        .to_builder()
        .handoff(wide)
        .build()
        .unwrap();
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![item(
            "transfer",
            RunItemKind::HandoffCall(
                ra_core::item::HandoffCall::new(
                    CallId::new("call"),
                    AgentId::new("reviewer"),
                    json!({"reason": "review"}),
                )
                .with_tool_name("transfer_to_reviewer"),
            ),
        )]),
        ModelResponse::new(vec![message("done", "done")]),
    ]);
    Runner::run(run_request(
        source,
        reviewer(Vec::new()),
        &model,
        RunConfig::new(),
    ))
    .await
    .unwrap();
    assert_eq!(model.input_labels(1), ["handoff_call", "handoff_output"]);
}

struct ProjectedCloseout {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ra_runtime::runner::result::RunErrorHandler for ProjectedCloseout {
    async fn handle(
        &self,
        input: ra_runtime::runner::result::RunErrorHandlerInput<'_>,
    ) -> Result<Option<ra_runtime::runner::result::RunErrorHandlerResult>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(input.data().last_agent().id().as_str(), "reviewer");
        assert_eq!(
            input
                .data()
                .original_input()
                .iter()
                .map(ModelInputItem::label)
                .collect::<Vec<_>>(),
            ["tool_call", "handoff_output"]
        );
        assert!(input.data().new_items().is_empty());
        Ok(None)
    }
}

#[tokio::test]
async fn budget_closeout_respects_the_receiving_agents_projection() {
    let calls = Arc::new(AtomicUsize::new(0));
    Runner::run(run_request(
        planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema()),
            Vec::new(),
        ),
        reviewer(Vec::new()),
        &script(),
        RunConfig::new()
            .with_max_turns(1)
            .with_error_handler(Arc::new(ProjectedCloseout {
                calls: Arc::clone(&calls),
            })),
    ))
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
