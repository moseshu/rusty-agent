//! What the checks a tool declares do to one call, and what they leave behind afterwards.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, GuardrailStage, Result},
    guardrail::{
        ToolGuardrailFunctionOutput, ToolInputGuardrail, ToolInputGuardrailData,
        ToolOutputGuardrail, ToolOutputGuardrailData,
    },
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolGuardrailId, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner},
};
use serde_json::json;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn guardrail_id(value: &str) -> ToolGuardrailId {
    ToolGuardrailId::new(value).expect("a valid guardrail identity")
}

/// A check that answers whatever it was built with, and remembers what it was asked about.
struct Recording {
    id: ToolGuardrailId,
    decision: ToolGuardrailFunctionOutput,
    fails: bool,
    /// The arguments (input side) or result text (output side) of every call it examined.
    seen: Arc<Mutex<Vec<String>>>,
}

impl Recording {
    fn new(id: &str, decision: ToolGuardrailFunctionOutput) -> Arc<Self> {
        Arc::new(Self {
            id: guardrail_id(id),
            decision,
            fails: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn allowing(id: &str) -> Arc<Self> {
        Self::new(
            id,
            ToolGuardrailFunctionOutput::allow().with_output_info(json!({"looked": true})),
        )
    }

    fn rejecting(id: &str, message: &str) -> Arc<Self> {
        Self::new(
            id,
            ToolGuardrailFunctionOutput::reject_content(message)
                .with_output_info(json!({"rule": "secrets"})),
        )
    }

    fn raising(id: &str) -> Arc<Self> {
        Self::new(
            id,
            ToolGuardrailFunctionOutput::raise_exception().with_output_info(json!({"rule": "rm"})),
        )
    }

    /// A check that cannot reach a decision at all.
    fn failing(id: &str) -> Arc<Self> {
        Arc::new(Self {
            id: guardrail_id(id),
            decision: ToolGuardrailFunctionOutput::allow(),
            fails: true,
            seen: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn answer(&self, saw: String) -> Result<ToolGuardrailFunctionOutput> {
        self.seen.lock().unwrap().push(saw);
        if self.fails {
            return Err(Error::caller("the classifier is unreachable"));
        }
        Ok(self.decision.clone())
    }
}

#[async_trait]
impl ToolInputGuardrail for Recording {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }

    async fn check(
        &self,
        data: &ToolInputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        self.answer(data.arguments().to_string())
    }
}

#[async_trait]
impl ToolOutputGuardrail for Recording {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }

    async fn check(
        &self,
        data: &ToolOutputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        self.answer(data.output().as_text().unwrap_or_default().to_owned())
    }
}

/// Replays one response per turn and counts how many it was asked for.
struct ScriptedModel {
    script: Mutex<Vec<ModelResponse>>,
    calls: AtomicUsize,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn next_response(&self) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.next_response()
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::iter(vec![
            self.next_response()
                .map(|response| ModelStreamEvent::Completed(Box::new(response))),
        ])
        .boxed()
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

/// A tool that answers with fixed text and counts how often it actually ran.
struct CountingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: Arc<AtomicUsize>,
}

impl CountingTool {
    fn new(options: ToolOptions) -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new("write_file").unwrap(),
            schema: ToolSchema::new(
                "write_file",
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            options,
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Tool for CountingTool {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("the file now contains the api key"))
    }
}

// ---------------------------------------------------------------------------
// request construction
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Setup {
    tools: Vec<Arc<dyn Tool>>,
    input: Vec<Arc<dyn ToolInputGuardrail>>,
    output: Vec<Arc<dyn ToolOutputGuardrail>>,
    pre_approval: bool,
    state: Option<RunState>,
}

impl Setup {
    fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    fn input(mut self, guardrail: Arc<dyn ToolInputGuardrail>) -> Self {
        self.input.push(guardrail);
        self
    }

    fn output(mut self, guardrail: Arc<dyn ToolOutputGuardrail>) -> Self {
        self.output.push(guardrail);
        self
    }

    fn pre_approval(mut self) -> Self {
        self.pre_approval = true;
        self
    }

    fn resuming(mut self, state: RunState) -> Self {
        self.state = Some(state);
        self
    }

    fn request(self, model: &Arc<ScriptedModel>) -> RunRequest {
        let agent = AgentSpec::builder()
            .id(AgentId::new("coder"))
            .name("Coder")
            .instructions("do the thing")
            .tools(self.tools)
            .build()
            .expect("the agent declaration must be valid");
        // One at a time on the argument side and in bulk on the result side, so both shapes of
        // installation are exercised by the suite that depends on them.
        let mut config = RunConfig::new()
            .with_tool_output_guardrails(self.output)
            .with_pre_approval_tool_input_guardrails(self.pre_approval);
        for guardrail in self.input {
            config = config.with_tool_input_guardrail(guardrail);
        }
        let input = if self.state.is_some() {
            Vec::new()
        } else {
            vec![ModelInputItem::Message(Message::user("写一下文件"))]
        };
        let request = RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(FixedResolver {
                model: Arc::clone(model),
            }),
            RunId::new("run-tool-guardrail"),
            CancelScope::root(),
            input,
        )
        .with_config(config);
        match self.state {
            Some(state) => request.with_state(state),
            None => request,
        }
    }
}

fn message(id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn tool_call(id: &str, call_id: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            "write_file",
            json!({"path": "secrets.env"}),
        )),
    )
}

/// Declares both boundaries on the tool, under the identities the run installs.
fn declaring(input: &[&str], output: &[&str]) -> ToolOptions {
    let mut options = ToolOptions::new();
    for id in input {
        options = options.with_input_guardrail(guardrail_id(id));
    }
    for id in output {
        options = options.with_output_guardrail(guardrail_id(id));
    }
    options
}

/// The model-visible text of the record answering one call.
fn answered_text(result: &RunResult, call_id: &str) -> String {
    let item = result
        .new_items()
        .iter()
        .find(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => output.call_id().as_str() == call_id,
            _ => false,
        })
        .expect("every call is answered");
    let RunItemKind::ToolCallOutput(output) = item.kind() else {
        unreachable!("filtered above")
    };
    ToolOutput::from_stored(output.output())
        .expect("the answer is a readable tool result")
        .expect("the answer is a tool result")
        .as_text()
        .expect("the answer carries text")
        .to_owned()
}

fn two_turns(first: RunItem) -> Vec<ModelResponse> {
    vec![
        ModelResponse::new(vec![first]),
        ModelResponse::new(vec![message("m2", "done")]),
    ]
}

// ---------------------------------------------------------------------------
// what a completed check leaves behind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_allowed_call_still_records_what_each_check_saw() {
    let arguments = Recording::allowing("secrets_in_arguments");
    let results = Recording::allowing("secrets_in_results");
    let tool = CountingTool::new(declaring(
        &["secrets_in_arguments"],
        &["secrets_in_results"],
    ));
    let model = ScriptedModel::new(two_turns(tool_call("c1", "call-1")));

    let result = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .input(arguments.clone())
            .output(results.clone())
            .request(&model),
    )
    .await
    .expect("checks that allow do not stop anything");

    assert_eq!(result.final_text(), "done");
    assert_eq!(tool.calls(), 1);
    assert_eq!(arguments.calls(), 1);
    assert_eq!(results.calls(), 1);
    // Each side is shown the call as the tool sees it: the parsed arguments, then the complete
    // result before any projection narrows it.
    assert_eq!(
        arguments.seen.lock().unwrap()[0],
        json!({"path": "secrets.env"}).to_string()
    );
    assert_eq!(
        results.seen.lock().unwrap()[0],
        "the file now contains the api key"
    );

    // A check that allowed is recorded, and with its evidence: "this check ran and found nothing"
    // and "this check was never installed" are the same absence without it.
    let recorded = result.tool_input_guardrail_results();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].guardrail(),
        &guardrail_id("secrets_in_arguments")
    );
    assert_eq!(recorded[0].call_id(), &CallId::new("call-1"));
    assert_eq!(recorded[0].origin().qualified_name(), "write_file");
    assert!(recorded[0].output().is_allow());
    assert_eq!(recorded[0].output().output_info(), &json!({"looked": true}));
    assert_eq!(result.tool_output_guardrail_results().len(), 1);

    // The decisions belong to the run, so they travel in the checkpoint rather than only in the
    // result of the segment that produced them.
    let restored: RunState =
        serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
    assert_eq!(
        restored.tool_input_guardrail_results(),
        result.tool_input_guardrail_results()
    );
    assert_eq!(
        restored.tool_output_guardrail_results(),
        result.tool_output_guardrail_results()
    );
}

// ---------------------------------------------------------------------------
// the two boundaries differ in what a refusal costs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_argument_means_the_tool_never_runs() {
    let arguments = Recording::rejecting("secrets_in_arguments", "that path holds credentials");
    let results = Recording::allowing("secrets_in_results");
    let tool = CountingTool::new(declaring(
        &["secrets_in_arguments"],
        &["secrets_in_results"],
    ));
    let model = ScriptedModel::new(two_turns(tool_call("c1", "call-1")));

    let result = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .input(arguments.clone())
            .output(results.clone())
            .request(&model),
    )
    .await
    .expect("a rejection answers the model rather than stopping the run");

    assert_eq!(tool.calls(), 0, "the tool must not run even once");
    // Nothing was produced, so the other boundary is never asked. It is not a check that passed.
    assert_eq!(results.calls(), 0);
    assert!(result.tool_output_guardrail_results().is_empty());
    // The message the host wrote is what the model reads in place of the call.
    assert_eq!(
        answered_text(&result, "call-1"),
        "that path holds credentials"
    );
    // The run continues: the model chooses again with the refusal in front of it.
    assert_eq!(result.final_text(), "done");
    assert_eq!(arguments.calls(), 1);
    assert_eq!(result.tool_input_guardrail_results().len(), 1);
}

#[tokio::test]
async fn a_refused_result_replaces_only_what_the_model_reads() {
    let results = Recording::rejecting("secrets_in_results", "the result carried a credential");
    let tool = CountingTool::new(declaring(&[], &["secrets_in_results"]));
    let model = ScriptedModel::new(two_turns(tool_call("c1", "call-1")));

    let result = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .output(results.clone())
            .request(&model),
    )
    .await
    .expect("a rejection answers the model rather than stopping the run");

    // The other half of the contrast above: here the call did happen, and whatever it did stands.
    assert_eq!(tool.calls(), 1);
    assert_eq!(
        answered_text(&result, "call-1"),
        "the result carried a credential"
    );
    assert_eq!(result.final_text(), "done");
    assert_eq!(result.tool_output_guardrail_results().len(), 1);
}

#[tokio::test]
async fn a_raise_ends_the_run_and_carries_the_decisions_behind_it() {
    let arguments = Recording::raising("no_destructive_paths");
    let tool = CountingTool::new(declaring(&["no_destructive_paths"], &[]));
    let model = ScriptedModel::new(two_turns(tool_call("c1", "call-1")));

    let error = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .input(arguments.clone())
            .request(&model),
    )
    .await
    .expect_err("a raise ends the run");

    assert_eq!(tool.calls(), 0);
    assert_eq!(error.code(), "guardrail.tool_input");
    // A refused run returns no result, so the error is the only thing the host receives. The
    // evidence the refusal was argued from has to be on it.
    let evidence = error
        .guardrail_evidence()
        .expect("a tool guardrail refusal carries its decisions");
    assert_eq!(evidence.stage(), GuardrailStage::ToolInput);
    let decisions = evidence
        .tool_input()
        .expect("the evidence is from the argument boundary");
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].output().output_info(), &json!({"rule": "rm"}));
}

/// A check that could not decide has not decided anything, and reporting it as a refusal would
/// blame the call for the checker being down.
#[tokio::test]
async fn a_check_that_cannot_decide_fails_the_turn_rather_than_refusing_the_call() {
    let arguments = Recording::failing("classifier");
    let tool = CountingTool::new(declaring(&["classifier"], &[]));
    let model = ScriptedModel::new(two_turns(tool_call("c1", "call-1")));

    let error = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .input(arguments)
            .request(&model),
    )
    .await
    .expect_err("an unreachable checker stops the turn");

    assert_eq!(tool.calls(), 0);
    assert_eq!(error.code(), "caller");
}

// ---------------------------------------------------------------------------
// resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_check_nobody_installed_stops_the_run_before_the_first_model_call() {
    let tool = CountingTool::new(declaring(&["secrets_in_arguments"], &[]));
    let model = ScriptedModel::new(two_turns(tool_call("c1", "call-1")));

    let error = Runner::run(Setup::default().tool(tool).request(&model))
        .await
        .expect_err("an unresolvable declaration is a configuration mistake");

    assert_eq!(error.code(), "config");
    assert_eq!(
        model.calls(),
        0,
        "the run must not be paid for before the configuration is checked"
    );
}

#[tokio::test]
async fn two_checks_installed_under_one_identity_are_refused() {
    let tool = CountingTool::new(ToolOptions::new());
    let model = ScriptedModel::new(two_turns(tool_call("c1", "call-1")));

    let error = Runner::run(
        Setup::default()
            .tool(tool)
            .input(Recording::allowing("secrets"))
            .input(Recording::rejecting("secrets", "no"))
            .request(&model),
    )
    .await
    .expect_err("a tool declaring `secrets` would have no way to say which it meant");

    assert_eq!(error.code(), "config");
    assert_eq!(model.calls(), 0);
}

// ---------------------------------------------------------------------------
// approval
// ---------------------------------------------------------------------------

/// The pre-approval pass spares the host a question about a call that is refused anyway.
#[tokio::test]
async fn a_call_refused_before_approval_never_reaches_the_host() {
    let arguments = Recording::rejecting("secrets_in_arguments", "that path holds credentials");
    let mut options = declaring(&["secrets_in_arguments"], &[]);
    options = options.with_approval(ToolApprovalPolicy::Always);
    let tool = CountingTool::new(options);
    let model = ScriptedModel::new(two_turns(tool_call("c1", "call-1")));

    let result = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .input(arguments.clone())
            .pre_approval()
            .request(&model),
    )
    .await
    .expect("the refusal answers the model instead of interrupting");

    assert!(
        !matches!(result.outcome(), RunOutcome::Interrupted { .. }),
        "nobody should be asked to approve a call that is already refused"
    );
    assert_eq!(tool.calls(), 0);
    assert_eq!(arguments.calls(), 1);
    assert_eq!(
        answered_text(&result, "call-1"),
        "that path holds credentials"
    );
}

/// The pass before the interruption is not the decision. An approval can come back in a later
/// process, against a registry the host has since changed.
#[tokio::test]
async fn an_approved_call_is_checked_again_after_the_answer_comes_back() {
    let arguments = Recording::allowing("secrets_in_arguments");
    let mut options = declaring(&["secrets_in_arguments"], &[]);
    options = options.with_approval(ToolApprovalPolicy::Always);
    let tool = CountingTool::new(options);
    let first = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call("c1", "call-1")])]);

    let interrupted = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .input(arguments.clone())
            .pre_approval()
            .request(&first),
    )
    .await
    .expect("the first segment stops for approval");
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("expected an approval interruption");
    };
    assert_eq!(arguments.calls(), 1, "the pre-approval pass ran");
    assert_eq!(tool.calls(), 0);
    assert_eq!(interrupted.tool_input_guardrail_results().len(), 1);

    let mut state = interrupted.state().clone();
    state.approve(&items[0], false).expect("host approval");
    let second = ScriptedModel::new(vec![ModelResponse::new(vec![message("m2", "done")])]);
    let resumed = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .input(arguments.clone())
            .pre_approval()
            .resuming(state)
            .request(&second),
    )
    .await
    .expect("the approved checkpoint resumes");

    assert_eq!(resumed.final_text(), "done");
    assert_eq!(tool.calls(), 1);
    assert_eq!(
        arguments.calls(),
        2,
        "the approval settles the question of permission, not of the arguments"
    );
    // Both passes are recorded, under the same identity and the same call. What tells them apart
    // is that there are two of them.
    assert_eq!(resumed.tool_input_guardrail_results().len(), 2);
}

/// Switched off, the host is asked first and the arguments are checked once — after the answer.
#[tokio::test]
async fn without_the_pre_approval_pass_the_arguments_are_checked_once() {
    let arguments = Recording::allowing("secrets_in_arguments");
    let mut options = declaring(&["secrets_in_arguments"], &[]);
    options = options.with_approval(ToolApprovalPolicy::Always);
    let tool = CountingTool::new(options);
    let first = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call("c1", "call-1")])]);

    let interrupted = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .input(arguments.clone())
            .request(&first),
    )
    .await
    .expect("the first segment stops for approval");
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("expected an approval interruption");
    };
    assert_eq!(arguments.calls(), 0);

    let mut state = interrupted.state().clone();
    state.approve(&items[0], false).expect("host approval");
    let second = ScriptedModel::new(vec![ModelResponse::new(vec![message("m2", "done")])]);
    let resumed = Runner::run(
        Setup::default()
            .tool(tool.clone())
            .input(arguments.clone())
            .resuming(state)
            .request(&second),
    )
    .await
    .expect("the approved checkpoint resumes");

    assert_eq!(resumed.final_text(), "done");
    assert_eq!(
        arguments.calls(),
        1,
        "the check after the answer is not optional"
    );
    assert_eq!(resumed.tool_input_guardrail_results().len(), 1);
}

/// Both boundaries stop at the first refusal or failure, after recording preceding allows.
#[tokio::test]
async fn tool_guardrails_short_circuit_in_declaration_order() {
    for output_boundary in [false, true] {
        for behavior in ["reject", "raise", "error"] {
            let allowed = Recording::allowing("allowed");
            let first = match behavior {
                "reject" => Recording::rejecting("first", "choose another path"),
                "raise" => Recording::raising("first"),
                _ => Recording::failing("first"),
            };
            let later = Recording::raising("later");
            let ids = ["allowed", "first", "later"];
            let options = if output_boundary {
                declaring(&[], &ids)
            } else {
                declaring(&ids, &[])
            };
            let tool = CountingTool::new(options);
            let model = ScriptedModel::new(two_turns(tool_call("c1", "call-1")));
            // Installation order differs from declaration order deliberately.
            let setup = Setup::default().tool(tool.clone());
            let setup = if output_boundary {
                setup
                    .output(later.clone())
                    .output(first.clone())
                    .output(allowed.clone())
            } else {
                setup
                    .input(later.clone())
                    .input(first.clone())
                    .input(allowed.clone())
            };
            let result = Runner::run(setup.request(&model)).await;
            assert_eq!(allowed.calls(), 1);
            assert_eq!(first.calls(), 1);
            assert_eq!(
                later.calls(),
                0,
                "later checks must not run after {behavior}"
            );
            assert_eq!(tool.calls(), usize::from(output_boundary));
            match behavior {
                "reject" => {
                    let result = result.expect("rejection continues the run");
                    assert_eq!(result.final_text(), "done");
                    assert_eq!(answered_text(&result, "call-1"), "choose another path");
                    let ids: Vec<_> = if output_boundary {
                        result
                            .tool_output_guardrail_results()
                            .iter()
                            .map(|r| r.guardrail().as_str())
                            .collect()
                    } else {
                        result
                            .tool_input_guardrail_results()
                            .iter()
                            .map(|r| r.guardrail().as_str())
                            .collect()
                    };
                    assert_eq!(ids, ["allowed", "first"]);
                }
                "raise" => {
                    let error = result.expect_err("raise ends the run");
                    let evidence = error.guardrail_evidence().expect("completed decisions");
                    let ids: Vec<_> = if output_boundary {
                        evidence
                            .tool_output()
                            .unwrap()
                            .iter()
                            .map(|r| r.guardrail().as_str())
                            .collect()
                    } else {
                        evidence
                            .tool_input()
                            .unwrap()
                            .iter()
                            .map(|r| r.guardrail().as_str())
                            .collect()
                    };
                    assert_eq!(ids, ["allowed", "first"]);
                }
                _ => assert_eq!(
                    result.expect_err("checker failure propagates").code(),
                    "caller"
                ),
            }
        }
    }
}

#[test]
fn core_reduction_preserves_the_first_non_allow_decision() {
    use ra_core::guardrail::{reduce_tool_input_guardrails, reduce_tool_output_guardrails};
    let reducers: [fn(
        Vec<(ToolGuardrailId, ToolGuardrailFunctionOutput)>,
    ) -> Result<ra_core::guardrail::ToolGuardrailVerdict>; 2] =
        [reduce_tool_input_guardrails, reduce_tool_output_guardrails];
    for reduce in reducers {
        let result = reduce(vec![
            (
                guardrail_id("first"),
                ToolGuardrailFunctionOutput::reject_content("retry"),
            ),
            (
                guardrail_id("later"),
                ToolGuardrailFunctionOutput::raise_exception(),
            ),
        ])
        .expect("a later raise cannot override rejection");
        assert!(!result.is_allow());
    }
}
