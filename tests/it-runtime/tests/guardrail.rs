//! When a run's input and output guardrails run, and what a tripwire does to the run.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec, ToolUseBehavior},
    cancel::{CancelReason, CancelScope},
    context::RunContext,
    error::{Error, Result},
    finish::FinishReason,
    guardrail::{GuardrailFinalOutput, GuardrailFunctionOutput, InputGuardrail, OutputGuardrail},
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
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner},
};
use serde_json::json;
use tokio::{sync::Notify, time::timeout};

// ---------------------------------------------------------------------------
// guardrail fixtures
// ---------------------------------------------------------------------------

/// What one input guardrail was handed, and what it answered.
struct RecordingInput {
    id: String,
    verdict: GuardrailFunctionOutput,
    /// The text of every input item the check was shown, once per call.
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    /// Held until the model call releases it, for the case that proves the two overlap.
    waits_for: Option<Arc<Notify>>,
    fails: bool,
    /// `false` puts this check in the half awaited before the first request.
    parallel: bool,
}

impl RecordingInput {
    fn new(name: &str) -> Self {
        Self {
            id: String::from(name),
            verdict: GuardrailFunctionOutput::pass(),
            seen: Arc::new(Mutex::new(Vec::new())),
            waits_for: None,
            fails: false,
            parallel: true,
        }
    }

    /// The same check, awaited before the first request instead of raced against it.
    fn blocking(mut self) -> Self {
        self.parallel = false;
        self
    }

    fn tripping(name: &str) -> Self {
        Self {
            verdict: GuardrailFunctionOutput::tripwire().with_output_info(json!({"rule": "deny"})),
            ..Self::new(name)
        }
    }

    fn failing(name: &str) -> Self {
        Self {
            fails: true,
            ..Self::new(name)
        }
    }

    fn waiting_for(name: &str, notify: Arc<Notify>) -> Self {
        Self {
            waits_for: Some(notify),
            ..Self::new(name)
        }
    }

    /// Trips, but only once the model call it races is under way.
    fn tripping_after(name: &str, notify: Arc<Notify>) -> Self {
        Self {
            waits_for: Some(notify),
            ..Self::tripping(name)
        }
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl InputGuardrail for RecordingInput {
    fn name(&self) -> &str {
        &self.id
    }

    fn run_in_parallel(&self) -> bool {
        self.parallel
    }

    async fn check(
        &self,
        _context: &RunContext,
        input: &[ModelInputItem],
    ) -> Result<GuardrailFunctionOutput> {
        if let Some(notify) = &self.waits_for {
            notify.notified().await;
        }
        self.seen.lock().unwrap().push(
            input
                .iter()
                .map(|item| match item {
                    ModelInputItem::Message(message) => message.text_content(),
                    other => format!("{other:?}"),
                })
                .collect(),
        );
        if self.fails {
            return Err(Error::caller("the classifier is unavailable"));
        }
        Ok(self.verdict.clone())
    }
}

/// What one output guardrail was handed, and what it answered.
struct RecordingOutput {
    id: String,
    verdict: GuardrailFunctionOutput,
    /// `(delivered text, whether a message was handed over, finish reason)`, once per call.
    seen: Arc<Mutex<Vec<(String, bool, FinishReason)>>>,
}

impl RecordingOutput {
    fn new(name: &str) -> Self {
        Self {
            id: String::from(name),
            verdict: GuardrailFunctionOutput::pass(),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn tripping(name: &str) -> Self {
        Self {
            verdict: GuardrailFunctionOutput::tripwire().with_output_info(json!({"pii": "email"})),
            ..Self::new(name)
        }
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl OutputGuardrail for RecordingOutput {
    fn name(&self) -> &str {
        &self.id
    }

    async fn check(
        &self,
        _context: &RunContext,
        output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        self.seen.lock().unwrap().push((
            output.text(),
            output.message().is_some(),
            output.finish_reason(),
        ));
        Ok(self.verdict.clone())
    }
}

// ---------------------------------------------------------------------------
// model and tool fixtures
// ---------------------------------------------------------------------------

/// Replays one response per turn, and can be told to release a waiting guardrail as it does.
struct ScriptedModel {
    script: Mutex<Vec<ModelResponse>>,
    calls: AtomicUsize,
    releases: Option<Arc<Notify>>,
    interrupts: Option<CancelScope>,
    hangs: bool,
}

impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            calls: AtomicUsize::new(0),
            releases: None,
            interrupts: None,
            hangs: false,
        })
    }

    /// Answers only after letting a guardrail that is waiting on `notify` proceed.
    fn releasing(script: Vec<ModelResponse>, notify: Arc<Notify>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            calls: AtomicUsize::new(0),
            releases: Some(notify),
            interrupts: None,
            hangs: false,
        })
    }

    /// Stops the whole run from outside as soon as the call it races has started, and then never
    /// answers. It is the model rather than a timer so the interrupt lands at a known moment.
    fn interrupting(cancel: CancelScope) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            releases: None,
            interrupts: Some(cancel),
            hangs: true,
        })
    }

    /// Never answers, so only a cancellation can end the turn it belongs to. It still releases the
    /// waiting guardrail first, so the refusal lands on a call that is genuinely in flight.
    fn hanging(notify: Arc<Notify>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            releases: Some(notify),
            interrupts: None,
            hangs: true,
        })
    }

    /// Counts the call and lets a guardrail waiting on this call proceed.
    fn start_call(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(notify) = &self.releases {
            notify.notify_one();
        }
        if let Some(cancel) = &self.interrupts {
            cancel.cancel(CancelReason::UserInterrupt);
        }
    }

    fn next_response(&self) -> Result<ModelResponse> {
        self.start_call();
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
        if self.hangs {
            self.start_call();
            std::future::pending::<()>().await;
        }
        self.next_response()
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        if self.hangs {
            self.start_call();
            return stream::pending().boxed();
        }
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

struct ScriptedTool {
    output: String,
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    calls: Arc<AtomicUsize>,
}

impl ScriptedTool {
    fn new(name: &str) -> Self {
        Self {
            output: "done".to_owned(),
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
            options: ToolOptions::new(),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_options(mut self, options: ToolOptions) -> Self {
        self.options = options;
        self
    }
}

#[async_trait]
impl Tool for ScriptedTool {
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
        Ok(ToolOutput::text(self.output.clone()))
    }
}

// ---------------------------------------------------------------------------
// request construction
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Setup {
    tools: Vec<Arc<dyn Tool>>,
    tool_use_behavior: Option<ToolUseBehavior>,
    agent_input: Vec<Arc<dyn InputGuardrail>>,
    agent_output: Vec<Arc<dyn OutputGuardrail>>,
    run_input: Vec<Arc<dyn InputGuardrail>>,
    run_output: Vec<Arc<dyn OutputGuardrail>>,
    max_turns: Option<u32>,
    state: Option<RunState>,
    cancel: Option<CancelScope>,
}

impl Setup {
    fn agent_input(mut self, guardrail: Arc<dyn InputGuardrail>) -> Self {
        self.agent_input.push(guardrail);
        self
    }

    fn agent_output(mut self, guardrail: Arc<dyn OutputGuardrail>) -> Self {
        self.agent_output.push(guardrail);
        self
    }

    fn run_input(mut self, guardrail: Arc<dyn InputGuardrail>) -> Self {
        self.run_input.push(guardrail);
        self
    }

    fn run_output(mut self, guardrail: Arc<dyn OutputGuardrail>) -> Self {
        self.run_output.push(guardrail);
        self
    }

    fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    fn tool_use_behavior(mut self, behavior: ToolUseBehavior) -> Self {
        self.tool_use_behavior = Some(behavior);
        self
    }

    fn max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    fn resuming(mut self, state: RunState) -> Self {
        self.state = Some(state);
        self
    }

    /// Runs under a scope the caller can stop, rather than under one of its own.
    fn under(mut self, cancel: CancelScope) -> Self {
        self.cancel = Some(cancel);
        self
    }

    fn request(self, model: &Arc<ScriptedModel>) -> RunRequest {
        let agent = AgentSpec::builder()
            .id(AgentId::new("coder"))
            .name("Coder")
            .instructions("do the thing")
            .tools(self.tools)
            .tool_use_behavior(self.tool_use_behavior.unwrap_or_default())
            .input_guardrails(self.agent_input)
            .output_guardrails(self.agent_output)
            .build()
            .expect("the agent declaration must be valid");
        let mut config = RunConfig::new();
        for guardrail in self.run_input {
            config = config.with_input_guardrail(guardrail);
        }
        for guardrail in self.run_output {
            config = config.with_output_guardrail(guardrail);
        }
        if let Some(max_turns) = self.max_turns {
            config = config.with_max_turns(max_turns);
        }
        // A resumed segment carries no input of its own, so it takes the checkpoint's projection —
        // which is the shape an input guardrail must not be handed a second time.
        let input = if self.state.is_some() {
            Vec::new()
        } else {
            vec![ModelInputItem::Message(Message::user("帮我改一下文件"))]
        };
        let request = RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(FixedResolver {
                model: Arc::clone(model),
            }),
            RunId::new("run-guardrail"),
            self.cancel.unwrap_or_else(CancelScope::root),
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

fn tool_call(id: &str, call_id: &str, name: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, json!({}))),
    )
}

/// The verdicts a finished run reports, as `(identity, tripped)` pairs.
fn input_verdicts(result: &RunResult) -> Vec<(String, bool)> {
    result
        .input_guardrail_results()
        .iter()
        .map(|verdict| {
            (
                verdict.guardrail().to_string(),
                verdict.tripwire_triggered(),
            )
        })
        .collect()
}

fn output_verdicts(result: &RunResult) -> Vec<(String, bool)> {
    result
        .output_guardrail_results()
        .iter()
        .map(|verdict| {
            (
                verdict.guardrail().to_string(),
                verdict.tripwire_triggered(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// the input stage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn input_guardrails_examine_the_callers_input_and_their_verdicts_are_recorded() {
    let declared = Arc::new(RecordingInput::new("prompt_injection"));
    let added = Arc::new(RecordingInput::new("pii"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "done")])]);

    let result = Runner::run(
        Setup::default()
            .agent_input(declared.clone())
            .run_input(added.clone())
            .request(&model),
    )
    .await
    .expect("a passing guardrail must not stop the run");

    assert_eq!(result.final_text(), "done");
    // Declaration order, the agent's first: this is the order the verdicts are recorded in, and
    // two runs of the same agent have to be comparable line by line.
    assert_eq!(
        input_verdicts(&result),
        vec![
            ("prompt_injection".to_owned(), false),
            ("pii".to_owned(), false)
        ]
    );
    // What the check was shown is the caller's own input, not the model request the turn built
    // from it — no system prompt, no tool table, no reminder.
    assert_eq!(declared.seen.lock().unwrap()[0], vec!["帮我改一下文件"]);
    assert_eq!(added.calls(), 1);

    // The verdicts belong to the run, so they travel in the checkpoint rather than only in the
    // result of the segment that produced them.
    let restored: RunState =
        serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
    assert_eq!(
        restored.input_guardrail_results(),
        result.state().input_guardrail_results()
    );
}

/// The whole point of racing them: a guardrail that cannot finish until the model call has started
/// still finishes. Run before the model call instead, this deadlocks.
#[tokio::test]
async fn input_guardrails_run_alongside_the_first_model_call() {
    let gate = Arc::new(Notify::new());
    let guardrail = Arc::new(RecordingInput::waiting_for("slow_check", gate.clone()));
    let model = ScriptedModel::releasing(
        vec![ModelResponse::new(vec![message("m1", "done")])],
        gate.clone(),
    );

    let result = timeout(
        Duration::from_secs(5),
        Runner::run(
            Setup::default()
                .agent_input(guardrail.clone())
                .request(&model),
        ),
    )
    .await
    .expect("a guardrail waiting on the model call it races must not deadlock the run")
    .expect("the guardrail passed");

    assert_eq!(result.final_text(), "done");
    assert_eq!(guardrail.calls(), 1);
}

/// A refused input stops the turn it was racing. The model here never answers, so the run can only
/// end at all if the tripwire cancelled it — a version that merely waited for the turn would hang.
#[tokio::test]
async fn a_tripped_input_guardrail_cancels_the_turn_it_was_racing() {
    let gate = Arc::new(Notify::new());
    let guardrail = Arc::new(RecordingInput::tripping_after(
        "prompt_injection",
        gate.clone(),
    ));
    let model = ScriptedModel::hanging(gate.clone());

    let error = timeout(
        Duration::from_secs(5),
        Runner::run(
            Setup::default()
                .agent_input(guardrail.clone())
                .request(&model),
        ),
    )
    .await
    .expect("a tripwire has to stop the turn it was racing, not wait for it")
    .expect_err("a tripwire fails the run");

    assert_eq!(error.code(), "guardrail.input");
    assert!(error.to_string().contains("prompt_injection"), "{error}");
    // The evidence rides along: the run produced no result, so the error is the only record.
    assert!(error.to_string().contains("deny"), "{error}");
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "the refusal has to land on a model call that was actually in flight"
    );
}

/// A stop from above is not a guardrail's doing. The check never answers here, so the run can only
/// end through the interrupt — and it has to end as one, because `is_cancelled` is the single test
/// the cancellation contract permits and a run relabelled as a refusal would fail every reader of
/// it.
#[tokio::test]
async fn an_interrupt_while_the_input_stage_is_waiting_is_still_a_cancellation() {
    let never = Arc::new(Notify::new());
    let guardrail = Arc::new(RecordingInput::waiting_for("slow_check", never));
    let cancel = CancelScope::root();
    let model = ScriptedModel::interrupting(cancel.clone());

    let error = timeout(
        Duration::from_secs(5),
        Runner::run(
            Setup::default()
                .agent_input(guardrail.clone())
                .under(cancel)
                .request(&model),
        ),
    )
    .await
    .expect("an interrupt has to reach a guardrail that is still waiting")
    .expect_err("an interrupted run fails");

    assert!(error.is_cancelled(), "{error}");
    assert_eq!(error.code(), "cancelled");
    assert_eq!(guardrail.calls(), 0, "the check never reached a verdict");
}

/// A check that could not reach a verdict has not decided anything. Reporting it as a refusal would
/// blame the input for the checker being down, and `guardrail.input` is what a report counts.
#[tokio::test]
async fn a_guardrail_that_fails_is_not_reported_as_a_refusal() {
    let guardrail = Arc::new(RecordingInput::failing("prompt_injection"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "done")])]);

    let error = Runner::run(
        Setup::default()
            .agent_input(guardrail.clone())
            .request(&model),
    )
    .await
    .expect_err("a guardrail error fails the run");

    assert_eq!(error.code(), "caller");
    assert!(error.to_string().contains("classifier"), "{error}");
}

/// Two checks under one name both run, even when the scheduling puts them in different halves.
///
/// A name is a display string, not an identity, so nothing here dedupes on it. What makes this the
/// case worth its own test is the split: one check is awaited before the first request and the
/// other races it, so the two verdicts arrive from different stages — and both have to be recorded
/// rather than the second one being taken for a repeat of the first.
#[tokio::test]
async fn one_name_may_stand_for_two_checks_in_different_halves_of_the_stage() {
    let blocking = Arc::new(RecordingInput::new("pii").blocking());
    let racing = Arc::new(RecordingInput::new("pii"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "done")])]);

    let result = Runner::run(
        Setup::default()
            .agent_input(blocking.clone())
            .run_input(racing.clone())
            .request(&model),
    )
    .await
    .expect("two checks sharing a name is a configuration, not an error");

    assert_eq!(blocking.calls(), 1, "the blocking half ran");
    assert_eq!(racing.calls(), 1, "so did the raced half");
    assert_eq!(
        input_verdicts(&result),
        vec![("pii".to_owned(), false), ("pii".to_owned(), false)],
        "both verdicts are recorded; dropping one as a duplicate would silently report a check \
         that never happened as having passed"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

/// A blocking check is decided before anything is sent, which is what a host gives up the racing
/// default to get.
#[tokio::test]
async fn a_blocking_input_guardrail_refuses_the_run_before_any_model_call() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "done")])]);

    let error = Runner::run(
        Setup::default()
            .agent_input(Arc::new(RecordingInput::tripping("pii").blocking()))
            .request(&model),
    )
    .await
    .expect_err("a blocking tripwire refuses the run");

    assert_eq!(error.code(), "guardrail.input");
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        0,
        "nothing was sent: that is the whole trade a blocking check makes"
    );
}

/// A refused stage reports what it saw — the tripping verdict, and the ones that finished first.
///
/// A refused run returns no result, so the error is the only thing a host receives. The verdicts
/// ride on it with their `output_info` intact; recorded anywhere else they would be evidence the
/// caller cannot reach. Both halves of the stage are covered: the blocking check passes and the
/// raced one trips, and both verdicts come back.
#[tokio::test]
async fn a_refused_input_stage_carries_its_verdicts_on_the_refusal() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "done")])]);

    let error = Runner::run(
        Setup::default()
            .agent_input(Arc::new(RecordingInput::new("topic").blocking()))
            .run_input(Arc::new(RecordingInput::tripping("pii")))
            .request(&model),
    )
    .await
    .expect_err("a tripwire refuses the run");

    assert_eq!(error.code(), "guardrail.input");
    let evidence = error
        .guardrail_evidence()
        .expect("a refusal carries the verdicts it was argued from");
    let verdicts = evidence
        .input()
        .expect("an input refusal carries input verdicts");

    let mut seen: Vec<(String, bool)> = verdicts
        .iter()
        .map(|verdict| (verdict.guardrail().to_owned(), verdict.tripwire_triggered()))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![("pii".to_owned(), true), ("topic".to_owned(), false)],
        "the tripping verdict and the sibling that had already passed both survive"
    );

    let tripped = verdicts
        .iter()
        .find(|verdict| verdict.tripwire_triggered())
        .expect("the tripping verdict is among them");
    assert_eq!(
        tripped.output().output_info(),
        &json!({"rule": "deny"}),
        "structured evidence, not a string inside the message: a host reads the rule that matched"
    );
}

/// The output stage carries its verdicts the same way.
#[tokio::test]
async fn a_refused_output_stage_carries_its_verdicts_on_the_refusal() {
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "an address")])]);

    let error = Runner::run(
        Setup::default()
            .agent_output(Arc::new(RecordingOutput::new("topic")))
            .run_output(Arc::new(RecordingOutput::tripping("pii")))
            .request(&model),
    )
    .await
    .expect_err("a tripwire refuses the delivery");

    assert_eq!(error.code(), "guardrail.output");
    let verdicts = error
        .guardrail_evidence()
        .and_then(ra_core::guardrail::GuardrailEvidence::output)
        .expect("an output refusal carries output verdicts");
    assert!(
        verdicts.iter().any(|verdict| verdict.tripwire_triggered()),
        "the tripping verdict is on the refusal"
    );
}

/// An input guardrail examines the caller's input, and a continuation carries that same input. Two
/// things follow: the check is not paid for twice, and a verdict that is a model call of its own
/// cannot answer differently the second time and fail a run that started legitimately.
#[tokio::test]
async fn input_guardrails_are_not_re_run_when_a_run_resumes() {
    let guardrail = Arc::new(RecordingInput::new("prompt_injection"));
    let tool = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let first = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c1",
        "call-1",
        "write_file",
    )])]);

    let interrupted = Runner::run(
        Setup::default()
            .agent_input(guardrail.clone())
            .tool(tool.clone())
            .request(&first),
    )
    .await
    .expect("the first segment stops for approval");
    let RunOutcome::Interrupted { items } = interrupted.outcome() else {
        panic!("expected an approval interruption");
    };
    assert_eq!(guardrail.calls(), 1);
    assert_eq!(input_verdicts(&interrupted).len(), 1);

    let mut state = interrupted.state().clone();
    state.approve(&items[0], false).expect("host approval");
    let second = ScriptedModel::new(vec![ModelResponse::new(vec![message("m2", "done")])]);
    let resumed = Runner::run(
        Setup::default()
            .agent_input(guardrail.clone())
            .tool(tool.clone())
            .resuming(state)
            .request(&second),
    )
    .await
    .expect("the approved checkpoint resumes");

    assert_eq!(resumed.final_text(), "done");
    assert_eq!(guardrail.calls(), 1, "the input was already examined");
    // Not empty, even though this segment ran no check: the verdict belongs to the run, and it is
    // read back from the checkpoint that carried it.
    assert_eq!(
        input_verdicts(&resumed),
        vec![("prompt_injection".to_owned(), false)]
    );
}

/// A model failure must propagate even if the concurrent checker never answers.
#[tokio::test]
async fn a_failed_turn_drops_a_pending_input_guardrail() {
    let guardrail = Arc::new(RecordingInput::waiting_for(
        "pending",
        Arc::new(Notify::new()),
    ));
    let model = ScriptedModel::new(vec![]);
    let error = timeout(
        Duration::from_secs(1),
        Runner::run(Setup::default().agent_input(guardrail).request(&model)),
    )
    .await
    .expect("the failed model call must not wait for the checker")
    .expect_err("the model error must propagate");
    assert!(
        error
            .to_string()
            .contains("scripted model ran out of responses")
    );
}

/// A stage cut short does not run again on the continuation.
///
/// The checks that never finished are not retried, and that is the reference implementation's
/// semantics: the stage is gated on being the run's first turn, not on which of its checks
/// produced a verdict. The trade is deliberate — a slow check racing a run that interrupts often
/// would otherwise be paid for on every resume, and its subject, the opening input, has not
/// changed since the verdict it never reached.
///
/// It is decided by an explicit mark rather than by comparing recorded verdicts, which is what
/// makes it hold here: this check reaches no verdict at all, so there is nothing for a name or a
/// count to be compared against.
#[tokio::test]
async fn a_stage_the_run_never_finished_is_not_re_entered_on_resume() {
    let release = Arc::new(Notify::new());
    let guardrail = Arc::new(RecordingInput::waiting_for("pending", release.clone()));
    let first_model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "first")])]);
    let first = Runner::run(
        Setup::default()
            .agent_input(guardrail.clone())
            .request(&first_model)
            .with_config(
                RunConfig::new()
                    .with_deadline(ra_core::cancel::Deadline::after(Duration::from_millis(50))),
            ),
    )
    .await
    .expect("budget exhaustion yields a checkpoint");
    assert_eq!(
        first.outcome().finish_reason(),
        Some(FinishReason::BudgetExhausted)
    );
    assert!(
        first.input_guardrail_results().is_empty(),
        "the check never returned, so there is no verdict"
    );
    assert_eq!(guardrail.calls(), 0);

    let state: ra_core::state::RunState =
        serde_json::from_value(serde_json::to_value(first.state()).unwrap()).unwrap();
    assert!(
        state.input_guardrails_started(),
        "the mark survives the checkpoint: it is what a resume reads"
    );

    release.notify_one();
    let second_model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m2", "second")])]);
    let resumed = Runner::run(
        Setup::default()
            .agent_input(guardrail.clone())
            .resuming(state)
            .request(&second_model),
    )
    .await
    .expect("the continuation is not blocked by a stage that already happened");

    assert_eq!(resumed.final_text(), "second");
    assert_eq!(
        guardrail.calls(),
        0,
        "the stage is entered once per run, and this run entered it before the deadline stopped it"
    );
    assert!(resumed.input_guardrail_results().is_empty());
}

/// A checkpoint written before the guardrail fields existed still loads, and reads as a run that
/// has not entered the stage.
///
/// The three fields are `serde(default)`, so an older record deserialises with no verdicts and an
/// unentered stage — which is what such a run was. It means an old checkpoint resumed under a
/// newly installed guardrail gets checked rather than silently skipped.
#[test]
fn a_checkpoint_written_before_guardrails_existed_still_loads() {
    let mut record = serde_json::to_value(RunState::start(RunId::new("run-old"))).unwrap();
    let object = record.as_object_mut().unwrap();
    object.remove("input_guardrail_results");
    object.remove("output_guardrail_results");
    object.remove("input_guardrails_started");

    let restored: RunState = serde_json::from_value(record).expect("an older record still loads");
    assert!(restored.input_guardrail_results().is_empty());
    assert!(restored.output_guardrail_results().is_empty());
    assert!(
        !restored.input_guardrails_started(),
        "a record that predates the mark has not entered the stage, so a resume runs it"
    );
}

/// A deadline that expires during a blocking check is the same budget stop it is anywhere else.
///
/// The blocking half runs before the turns and the raced half runs beside them, but a run does not
/// change how it reports an expired wall clock based on which half a check happened to be in. Both
/// yield a checkpoint the caller can resume from.
#[tokio::test]
async fn a_deadline_during_a_blocking_check_is_a_budget_stop_like_any_other() {
    let never = Arc::new(Notify::new());
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "done")])]);

    let result = Runner::run(
        Setup::default()
            .agent_input(Arc::new(
                RecordingInput::waiting_for("slow", never.clone()).blocking(),
            ))
            .request(&model)
            .with_config(
                RunConfig::new()
                    .with_deadline(ra_core::cancel::Deadline::after(Duration::from_millis(50))),
            ),
    )
    .await
    .expect("an expired deadline is a budget stop, not a bare cancellation");

    assert_eq!(
        result.outcome().finish_reason(),
        Some(FinishReason::BudgetExhausted),
        "the blocking half goes through the same wall-clock translation the turns do"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

/// The same deadline in the raced half reports the same way, which is what makes the pair a rule.
#[tokio::test]
async fn a_deadline_during_a_raced_check_reports_the_same_budget_stop() {
    let never = Arc::new(Notify::new());
    let model = ScriptedModel::hanging(never.clone());

    let result = Runner::run(
        Setup::default()
            .agent_input(Arc::new(RecordingInput::waiting_for("slow", never.clone())))
            .request(&model)
            .with_config(
                RunConfig::new()
                    .with_deadline(ra_core::cancel::Deadline::after(Duration::from_millis(50))),
            ),
    )
    .await
    .expect("an expired deadline is a budget stop");

    assert_eq!(
        result.outcome().finish_reason(),
        Some(FinishReason::BudgetExhausted)
    );
}

// ---------------------------------------------------------------------------
// the output stage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn output_guardrails_examine_the_message_the_run_delivers() {
    let declared = Arc::new(RecordingOutput::new("pii"));
    let added = Arc::new(RecordingOutput::new("tone"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "here it is")])]);

    let result = Runner::run(
        Setup::default()
            .agent_output(declared.clone())
            .run_output(added.clone())
            .request(&model),
    )
    .await
    .expect("a passing guardrail must not stop the delivery");

    assert_eq!(result.final_text(), "here it is");
    assert_eq!(
        output_verdicts(&result),
        vec![("pii".to_owned(), false), ("tone".to_owned(), false)]
    );
    assert_eq!(
        declared.seen.lock().unwrap()[0],
        ("here it is".to_owned(), true, FinishReason::Final)
    );
    assert_eq!(added.calls(), 1);
}

#[tokio::test]
async fn a_tripped_output_guardrail_stops_the_run_before_it_is_delivered() {
    let guardrail = Arc::new(RecordingOutput::tripping("pii"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message(
        "m1",
        "someone@example.com",
    )])]);

    let error = Runner::run(
        Setup::default()
            .agent_output(guardrail.clone())
            .request(&model),
    )
    .await
    .expect_err("a tripwire fails the run rather than delivering the answer");

    assert_eq!(error.code(), "guardrail.output");
    assert!(error.to_string().contains("pii"), "{error}");
    assert!(error.to_string().contains("email"), "{error}");
    assert_eq!(guardrail.calls(), 1);
}

/// A run promoted a tool result to its answer, and the check sees that result.
///
/// There is no assistant message to hand over, and handing over an empty string instead would be
/// worse than not asking: the check runs, reports a pass, and has examined nothing. So the
/// concluding turn's tool outputs go with the delivery, and `text()` reads them when the run
/// produced no message of its own.
#[tokio::test]
async fn an_output_guardrail_is_asked_even_when_a_tool_result_became_the_answer() {
    let guardrail = Arc::new(RecordingOutput::new("pii"));
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c1",
        "call-1",
        "write_file",
    )])]);

    let result = Runner::run(
        Setup::default()
            .agent_output(guardrail.clone())
            .tool(tool.clone())
            .tool_use_behavior(ToolUseBehavior::StopOnFirstTool)
            .request(&model),
    )
    .await
    .expect("a promoted tool result still finishes the run");

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::ToolStop
        }
    ));
    let (text, has_message, reason) = guardrail.seen.lock().unwrap()[0].clone();
    assert!(
        !has_message,
        "a promoted tool result carries no assistant message"
    );
    assert_eq!(reason, FinishReason::ToolStop, "and the check is told why");
    assert_eq!(
        text, "done",
        "the answer is the tool's own result, and that is what the check examines — an empty \
         string here would be a guardrail passing something it never saw"
    );
}

/// A check on a promoted tool result sees the concluding turn's results, not the whole run's.
///
/// A run that looked something up on the first turn and finished on the second settled results on
/// both. Only the second turn's are candidates for the answer: handing over the first turn's too
/// would let an intermediate lookup decide the verdict on an output it is not part of, and would
/// make the verdict depend on whether the run had been resumed.
#[tokio::test]
async fn a_promoted_tool_result_is_read_from_the_concluding_turn_alone() {
    let guardrail = Arc::new(RecordingOutput::new("pii"));
    let lookup = Arc::new(ScriptedTool::new("lookup"));
    let finish = Arc::new(ScriptedTool::new("finish"));
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![tool_call("c1", "call-1", "lookup")]),
        ModelResponse::new(vec![tool_call("c2", "call-2", "finish")]),
    ]);

    let result = Runner::run(
        Setup::default()
            .agent_output(guardrail.clone())
            .tool(lookup)
            .tool(finish)
            .tool_use_behavior(ToolUseBehavior::StopAtTools {
                names: ["finish".to_owned()].into_iter().collect(),
            })
            .request(&model),
    )
    .await
    .expect("the second turn's tool result finishes the run");

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::ToolStop
        }
    ));
    let (text, _, _) = guardrail.seen.lock().unwrap()[0].clone();
    assert_eq!(
        text, "done",
        "one result, from the turn that concluded — the first turn's lookup is history, not the \
         answer being delivered"
    );
}

/// A run stopped from outside has no answer the agent chose. Asking an output guardrail about work
/// in progress would produce a verdict on something nobody is about to deliver.
#[tokio::test]
async fn output_guardrails_are_not_asked_when_the_run_was_stopped_from_outside() {
    let input = Arc::new(RecordingInput::new("prompt_injection"));
    let output = Arc::new(RecordingOutput::new("pii"));
    let tool = Arc::new(ScriptedTool::new("write_file"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c1",
        "call-1",
        "write_file",
    )])]);

    let result = Runner::run(
        Setup::default()
            .agent_input(input.clone())
            .agent_output(output.clone())
            .tool(tool.clone())
            .max_turns(1)
            .request(&model),
    )
    .await
    .expect("an exhausted turn cap ends the run softly");

    assert!(matches!(
        result.outcome(),
        RunOutcome::Completed {
            reason: FinishReason::MaxTurns
        }
    ));
    assert_eq!(input.calls(), 1, "the input was still examined");
    assert_eq!(output.calls(), 0);
    assert!(result.output_guardrail_results().is_empty());
}

/// Nothing about the two stages is coupled: the input check ran and passed, so the output check is
/// reached, and the run carries both verdicts.
#[tokio::test]
async fn both_stages_record_their_verdicts_on_one_run() {
    let input = Arc::new(RecordingInput::new("prompt_injection"));
    let output = Arc::new(RecordingOutput::new("pii"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m1", "done")])]);

    let result = Runner::run(
        Setup::default()
            .agent_input(input.clone())
            .agent_output(output.clone())
            .request(&model),
    )
    .await
    .expect("both stages passed");

    assert_eq!(input_verdicts(&result).len(), 1);
    assert_eq!(output_verdicts(&result).len(), 1);
    let restored: RunState =
        serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
    assert_eq!(restored.output_guardrail_results().len(), 1);
}

#[tokio::test]
async fn resumed_tool_stop_checks_the_current_segments_output() {
    let mut lookup = ScriptedTool::new("lookup");
    lookup.output = "old result".to_owned();
    let first_model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c1", "call1", "lookup",
    )])]);
    let first = Runner::run(
        Setup::default()
            .tool(Arc::new(lookup))
            .max_turns(1)
            .request(&first_model),
    )
    .await
    .unwrap();
    assert_eq!(
        first.outcome().finish_reason(),
        Some(FinishReason::MaxTurns)
    );
    let state = serde_json::from_value(serde_json::to_value(first.state()).unwrap()).unwrap();
    let check = Arc::new(RecordingOutput::new("check"));
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![tool_call(
        "c2", "call2", "finish",
    )])]);
    let result = Runner::run(
        Setup::default()
            .resuming(state)
            .agent_output(check.clone())
            .tool(Arc::new(ScriptedTool::new("finish")))
            .tool_use_behavior(ToolUseBehavior::StopOnFirstTool)
            .request(&model),
    )
    .await
    .unwrap();
    assert_eq!(
        result.outcome().finish_reason(),
        Some(FinishReason::ToolStop)
    );
    assert_eq!(check.seen.lock().unwrap()[0].0, "done");
}

#[tokio::test]
async fn blocking_checks_must_finish_after_a_checkpoint_before_the_model_runs() {
    for trips in [false, true] {
        let gate = Arc::new(Notify::new());
        let blocking = if trips {
            RecordingInput::tripping_after("blocking", gate.clone())
        } else {
            RecordingInput::waiting_for("blocking", gate.clone())
        };
        let check = Arc::new(blocking.blocking());
        let racing = Arc::new(RecordingInput::new("racing"));
        let model = ScriptedModel::new(vec![ModelResponse::new(vec![message("m", "done")])]);
        let first = Runner::run(
            Setup::default()
                .agent_input(check.clone())
                .agent_input(racing.clone())
                .request(&model)
                .with_config(
                    RunConfig::new()
                        .with_deadline(ra_core::cancel::Deadline::after(Duration::from_millis(20))),
                ),
        )
        .await
        .unwrap();
        assert_eq!(
            first.outcome().finish_reason(),
            Some(FinishReason::BudgetExhausted)
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        assert!(!first.state().input_guardrails_started());
        assert_eq!(racing.calls(), 0);
        let state = serde_json::from_value(serde_json::to_value(first.state()).unwrap()).unwrap();
        gate.notify_one();
        let resumed = Runner::run(
            Setup::default()
                .agent_input(check.clone())
                .agent_input(racing.clone())
                .resuming(state)
                .request(&model),
        )
        .await;
        assert_eq!(check.calls(), 1);
        if trips {
            assert_eq!(resumed.unwrap_err().code(), "guardrail.input");
            assert_eq!(model.calls.load(Ordering::SeqCst), 0);
            assert_eq!(racing.calls(), 0);
        } else {
            let result = resumed.unwrap();
            assert_eq!(result.final_text(), "done");
            assert!(result.state().input_guardrails_started());
            assert_eq!(result.input_guardrail_results().len(), 2);
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
            assert_eq!(racing.calls(), 1);
        }
    }
}
