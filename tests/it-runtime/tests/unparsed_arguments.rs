//! A tool call whose argument text is not JSON, under the framework's default semantics, as the
//! reference SDK treats arguments it cannot parse: the call still passes PreToolUse and the
//! permission stage, where an approval is asked without consulting the tool's dynamic answer; once
//! admitted, its input guardrails see the text and the tool's failure policy answers it. The tool's
//! decoder, approval answer, prepared state and body never run, and PostToolUse is not reached. The
//! lifecycle narrates the admitted call as an invocation that started and failed, as the reference
//! parses arguments inside the invocation.

use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    guardrail::{ToolGuardrailFunctionOutput, ToolInputGuardrail, ToolInputGuardrailData},
    hook::{HookDecision, HookEvent, HookEventName, UserHook, UserHookContext},
    item::{CallId, ItemId, ModelResponse, RunItem, RunItemKind, ToolCall},
    lifecycle::{LifecycleHook, LifecycleScope, ToolEndInput, ToolStartInput},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ModelStream,
        ModelStreamEvent, ProviderKey, ResolvedModel,
    },
    permission::{PermissionDecision, PermissionRule},
    state::{RunId, RunState},
    tool::{
        DecodedToolInput, Tool, ToolApprovalPolicy, ToolContext, ToolFailureHandling,
        ToolGuardrailId, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    hook::UserHookRegistration,
    permission::{ActionClassPolicy, PermissionEngine},
    runner::{RunConfig, RunOutcome, RunRequest, RunResult, Runner},
};
use serde_json::{Value, json};

const TEXT: &str = "{bad";

struct ScriptedModel(Mutex<Vec<ModelResponse>>);

impl ScriptedModel {
    fn next(&self) -> Result<ModelResponse> {
        let mut script = self.0.lock().unwrap();
        if script.is_empty() {
            return Err(Error::caller("scripted model ran out of responses"));
        }
        Ok(script.remove(0))
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.next()
    }
    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::iter(vec![
            self.next()
                .map(|response| ModelStreamEvent::Completed(Box::new(response))),
        ])
        .boxed()
    }
}

struct Resolver(Arc<ScriptedModel>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// Everything that ran, in order, across the tool, the hooks, the guardrail and the lifecycle.
type Journal = Arc<Mutex<Vec<String>>>;

fn entry(name: &str, arguments: &Value, unparsed: bool) -> String {
    if unparsed {
        format!("{name}(unparsed {arguments})")
    } else {
        format!("{name}({arguments})")
    }
}

struct Probe {
    origin: ToolOrigin,
    schema: ToolSchema,
    approval: ToolApprovalPolicy,
    failure: ToolFailureHandling,
    repeat_limit: Option<NonZeroU32>,
    journal: Journal,
}

#[async_trait]
impl Tool for Probe {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        let options = ToolOptions::new()
            .with_approval(self.approval)
            .with_failure_handling(self.failure)
            .with_input_guardrail(ToolGuardrailId::new("check").unwrap());
        match self.repeat_limit {
            Some(limit) => options.with_max_repeat_streak(limit),
            None => options,
        }
    }
    fn decode_input(&self, arguments: &Value) -> Result<Option<DecodedToolInput>> {
        self.journal
            .lock()
            .unwrap()
            .push(format!("decode({arguments})"));
        Ok(None)
    }
    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        self.journal
            .lock()
            .unwrap()
            .push("needs_approval".to_owned());
        Ok(true)
    }
    async fn approval_state(&self, _context: &ToolContext<'_>) -> Result<Option<Value>> {
        self.journal
            .lock()
            .unwrap()
            .push("approval_state".to_owned());
        Ok(None)
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.journal
            .lock()
            .unwrap()
            .push(format!("call({})", context.arguments()));
        Ok(ToolOutput::text("ran"))
    }
    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        _error: &Error,
    ) -> Result<Option<ToolOutput>> {
        self.journal
            .lock()
            .unwrap()
            .push("handle_failure".to_owned());
        Ok(Some(ToolOutput::text(
            "the probe could not read its arguments",
        )))
    }
}

/// Records PreToolUse, PermissionRequest and PostToolUse; PreToolUse answers as scripted.
struct Hooks {
    journal: Journal,
    pre: Option<HookDecision>,
}

#[async_trait]
impl UserHook for Hooks {
    fn name(&self) -> &str {
        "recorder"
    }
    async fn call(
        &self,
        _context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        let (name, call) = match event {
            HookEvent::PreToolUse(call) => ("pre", call),
            HookEvent::PermissionRequest(call) => ("permission", call),
            HookEvent::PostToolUse { call, .. } => ("post", call),
            _ => return Ok(HookDecision::Continue),
        };
        self.journal.lock().unwrap().push(entry(
            name,
            call.arguments(),
            call.has_unparsed_arguments(),
        ));
        Ok(match (name, &self.pre) {
            ("pre", Some(decision)) => decision.clone(),
            _ => HookDecision::Continue,
        })
    }
}

struct Check {
    id: ToolGuardrailId,
    journal: Journal,
    refuses: bool,
}

#[async_trait]
impl ToolInputGuardrail for Check {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }
    async fn check(
        &self,
        data: &ToolInputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        self.journal.lock().unwrap().push(entry(
            "guardrail",
            data.arguments(),
            data.has_unparsed_arguments(),
        ));
        Ok(if self.refuses {
            ToolGuardrailFunctionOutput::reject_content("refused by the check")
        } else {
            ToolGuardrailFunctionOutput::allow()
        })
    }
}

struct Narration(Journal);

#[async_trait]
impl LifecycleHook for Narration {
    fn name(&self) -> &str {
        "narration"
    }
    async fn on_tool_start(&self, _: LifecycleScope, input: &ToolStartInput<'_>) -> Result<()> {
        self.0.lock().unwrap().push(entry(
            "start",
            input.arguments(),
            input.has_unparsed_arguments(),
        ));
        Ok(())
    }
    async fn on_tool_end(&self, _: LifecycleScope, input: &ToolEndInput<'_>) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("end({})", input.failure_code().unwrap_or("ok")));
        Ok(())
    }
}

/// One scenario: the probe's policies, the hooks' and guardrail's answers, and the run's settings.
struct Case {
    approval: ToolApprovalPolicy,
    failure: ToolFailureHandling,
    pre: Option<HookDecision>,
    guardrail_refuses: bool,
    rules: Vec<PermissionRule>,
    prompts: bool,
    pre_approval_guardrails: bool,
    reported_first: bool,
    restricted: bool,
    repeat_limit: Option<NonZeroU32>,
    journal: Journal,
}

impl Case {
    fn new(approval: ToolApprovalPolicy) -> Self {
        Self {
            approval,
            failure: ToolFailureHandling::Custom,
            pre: None,
            guardrail_refuses: false,
            rules: Vec::new(),
            prompts: true,
            pre_approval_guardrails: false,
            reported_first: false,
            restricted: false,
            repeat_limit: None,
            journal: Journal::default(),
        }
    }
    fn failure(mut self, failure: ToolFailureHandling) -> Self {
        self.failure = failure;
        self
    }
    fn pre(mut self, decision: HookDecision) -> Self {
        self.pre = Some(decision);
        self
    }
    fn guardrail_refuses(mut self) -> Self {
        self.guardrail_refuses = true;
        self
    }
    fn rule(mut self, decision: PermissionDecision) -> Self {
        self.rules
            .push(PermissionRule::new(decision).with_tool_name("probe"));
        self
    }
    fn without_prompts(mut self) -> Self {
        self.prompts = false;
        self
    }
    fn pre_approval_guardrails(mut self) -> Self {
        self.pre_approval_guardrails = true;
        self
    }
    /// The product's choice to report such calls before any approval.
    fn reported_first(mut self) -> Self {
        self.reported_first = true;
        self
    }
    /// Restricts actions to a class the probe does not declare, which denies it.
    fn restricted(mut self) -> Self {
        self.restricted = true;
        self
    }
    fn repeat_limit(mut self, limit: u32) -> Self {
        self.repeat_limit = NonZeroU32::new(limit);
        self
    }

    fn request(&self, script: Vec<ModelResponse>) -> RunRequest {
        let probe = Probe {
            origin: ToolOrigin::new("probe").unwrap(),
            schema: ToolSchema::new(
                "probe",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            approval: self.approval,
            failure: self.failure,
            repeat_limit: self.repeat_limit,
            journal: Arc::clone(&self.journal),
        };
        let agent = AgentSpec::builder()
            .id(AgentId::new("coder"))
            .name("Coder")
            .tool(Arc::new(probe))
            .build()
            .unwrap();
        let hooks = Arc::new(Hooks {
            journal: Arc::clone(&self.journal),
            pre: self.pre.clone(),
        });
        let mut engine = PermissionEngine::default();
        if self.restricted {
            engine = engine.with_policy(ActionClassPolicy::default().with_allow_only(["read"]));
        }
        let engine = engine
            .with_prompts(self.prompts)
            .with_unparsed_arguments_reported_before_approval(self.reported_first);
        let mut config = RunConfig::new()
            .with_permission_engine(engine)
            .with_permission_rules(self.rules.clone())
            .with_pre_approval_tool_input_guardrails(self.pre_approval_guardrails)
            .with_tool_input_guardrail(Arc::new(Check {
                id: ToolGuardrailId::new("check").unwrap(),
                journal: Arc::clone(&self.journal),
                refuses: self.guardrail_refuses,
            }))
            .with_lifecycle_hook(Arc::new(Narration(Arc::clone(&self.journal))));
        for event in [
            HookEventName::PreToolUse,
            HookEventName::PermissionRequest,
            HookEventName::PostToolUse,
        ] {
            config = config.with_user_hook(UserHookRegistration::new(
                event,
                Arc::clone(&hooks) as Arc<dyn UserHook>,
            ));
        }
        RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(Resolver(Arc::new(ScriptedModel(Mutex::new(script))))),
            RunId::new("run-unparsed"),
            CancelScope::root(),
            Vec::new(),
        )
        .with_config(config)
    }

    async fn run(&self) -> Result<RunResult> {
        Runner::run(self.request(vec![calls_probe(), finishes()])).await
    }

    fn journal(&self) -> Vec<String> {
        self.journal.lock().unwrap().clone()
    }
}

fn calls_probe() -> ModelResponse {
    calls_probe_as("call-1", Err(TEXT))
}

/// A response that calls the probe as `call_id`, with parsed arguments or with text that is not
/// JSON.
fn calls_probe_as(call_id: &str, arguments: std::result::Result<Value, &str>) -> ModelResponse {
    let call = match arguments {
        Ok(arguments) => ToolCall::new(CallId::new(call_id), "probe", arguments),
        Err(text) => ToolCall::unparsed(CallId::new(call_id), "probe", text),
    };
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("item-{call_id}")),
        RunItemKind::ToolCall(call),
    )])
}

fn finishes() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("m-1"),
        RunItemKind::Message(ra_core::item::Message::assistant(
            "done",
            ra_core::item::OutputPhase::Final,
        )),
    )])
}

/// The answer the call received: its text and whether it was marked as an error.
fn answer(items: &[RunItem]) -> (String, bool) {
    answer_of(items, "call-1")
}

fn answer_of(items: &[RunItem], call_id: &str) -> (String, bool) {
    items
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == call_id => {
                let text = match ToolOutput::from_stored(output.output()) {
                    Ok(Some(stored)) => stored.as_text().unwrap_or_default().to_owned(),
                    _ => output.output().to_string(),
                };
                Some((text, output.is_error()))
            }
            _ => None,
        })
        .expect("the call was answered")
}

fn pre() -> String {
    entry("pre", &json!(TEXT), true)
}
fn permission() -> String {
    entry("permission", &json!(TEXT), true)
}
fn guardrail() -> String {
    entry("guardrail", &json!(TEXT), true)
}
/// The lifecycle of an admitted call: it starts with the text, marked, and ends with the error of
/// reading it.
fn started() -> String {
    entry("start", &json!(TEXT), true)
}
fn ended() -> String {
    "end(tool.invalid_input)".to_owned()
}
fn handled() -> String {
    "handle_failure".to_owned()
}

/// Runs a case to its approval and returns the interrupted result.
async fn interrupted(case: &Case) -> RunResult {
    let first = Runner::run(case.request(vec![calls_probe()]))
        .await
        .unwrap();
    assert!(
        matches!(first.outcome(), RunOutcome::Interrupted { .. }),
        "{:?}",
        first.outcome()
    );
    first
}

/// Answers the interruption of `first` through a checkpoint written and read back, and resumes.
async fn resumed(case: &Case, first: &RunResult, approve: bool) -> Result<RunResult> {
    let RunOutcome::Interrupted { items } = first.outcome() else {
        panic!("expected an approval");
    };
    let mut checkpoint: RunState =
        serde_json::from_str(&serde_json::to_string(first.state()).unwrap()).unwrap();
    if approve {
        checkpoint.approve(&items[0], false).unwrap();
    } else {
        checkpoint.reject(&items[0], false).unwrap();
    }
    Runner::run(case.request(vec![finishes()]).with_state(checkpoint)).await
}

/// Item 6 and 8 (`Custom`): admitted without asking, the call passes PreToolUse with its text, then
/// its input guardrail, and the tool's failure handler answers; nothing else about the tool runs.
#[tokio::test]
async fn an_admitted_unparsed_call_is_answered_by_the_failure_handler_alone() {
    let case = Case::new(ToolApprovalPolicy::Never);
    let result = case.run().await.unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(
        case.journal(),
        [pre(), guardrail(), started(), ended(), handled()]
    );
    let (text, is_error) = answer(result.new_items());
    assert_eq!(text, "the probe could not read its arguments");
    assert!(is_error);
}

/// Item 8: `ModelVisible` answers with the generic failure object, as an error, and the run
/// continues; `Propagate` fails the run.
#[tokio::test]
async fn the_other_failure_policies_answer_or_raise() {
    let case = Case::new(ToolApprovalPolicy::Never).failure(ToolFailureHandling::ModelVisible);
    let result = case.run().await.unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(case.journal(), [pre(), guardrail(), started(), ended()]);
    let (text, is_error) = answer(result.new_items());
    assert!(text.contains("tool.invalid_input"), "{text}");
    assert!(is_error);

    let case = Case::new(ToolApprovalPolicy::Never).failure(ToolFailureHandling::Propagate);
    let error = case.run().await.unwrap_err();
    assert_eq!(error.code(), "tool.invalid_input", "{error}");
    // The invocation's end is narrated before its error leaves, as for any other failed call.
    assert_eq!(case.journal(), [pre(), guardrail(), started(), ended()]);
}

/// Item 7: the input guardrail sees the text and its refusal is the answer; no argument error is
/// reported.
#[tokio::test]
async fn an_input_guardrail_refusal_answers_instead_of_the_argument_error() {
    let case = Case::new(ToolApprovalPolicy::Never).guardrail_refuses();
    let result = case.run().await.unwrap();
    assert_eq!(case.journal(), [pre(), guardrail()]);
    let (text, _) = answer(result.new_items());
    assert!(text.contains("refused by the check"), "{text}");
}

/// Item 9: PreToolUse that rewrites the input into JSON sends the call down the ordinary path,
/// with the rewritten arguments and no unparsed mark.
#[tokio::test]
async fn a_pre_tool_use_rewrite_into_json_runs_the_call() {
    let case = Case::new(ToolApprovalPolicy::Never).pre(HookDecision::UpdateInput {
        input: json!({"fixed": true}),
    });
    let result = case.run().await.unwrap();
    let fixed = json!({"fixed": true});
    assert_eq!(
        case.journal(),
        [
            pre(),
            format!("decode({fixed})"),
            entry("guardrail", &fixed, false),
            entry("start", &fixed, false),
            format!("call({fixed})"),
            "end(ok)".to_owned(),
            entry("post", &fixed, false),
        ]
    );
    assert_eq!(answer(result.new_items()), ("ran".to_owned(), false));
}

/// Item 6a: `Dynamic` and `Always` both ask, without the tool's dynamic answer or prepared state,
/// and the permission hook sees the text; `Never` and a fixed allow ask nobody; a fixed deny
/// refuses before PreToolUse, without the guardrail or the failure handler.
#[tokio::test]
async fn the_permission_stage_asks_without_consulting_the_tool() {
    for approval in [ToolApprovalPolicy::Dynamic, ToolApprovalPolicy::Always] {
        let case = Case::new(approval);
        interrupted(&case).await;
        assert_eq!(case.journal(), [pre(), permission()], "{approval:?}");
    }

    let case = Case::new(ToolApprovalPolicy::Always).rule(PermissionDecision::Allow);
    case.run().await.unwrap();
    assert_eq!(
        case.journal(),
        [pre(), guardrail(), started(), ended(), handled()]
    );

    let case = Case::new(ToolApprovalPolicy::Never).rule(PermissionDecision::Deny);
    let result = case.run().await.unwrap();
    assert!(case.journal().is_empty(), "{:?}", case.journal());
    assert!(answer(result.new_items()).1);
}

/// Items 6a and 6d: answered through a checkpoint written and read back, an approved call passes
/// its input guardrail with the text still marked and then its failure policy; a rejected one is
/// answered by the rejection alone, even under `Propagate`.
#[tokio::test]
async fn an_answered_approval_resumes_as_the_unparsed_call() {
    let case = Case::new(ToolApprovalPolicy::Always);
    let first = interrupted(&case).await;
    let result = resumed(&case, &first, true).await.unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(
        case.journal(),
        [
            pre(),
            permission(),
            guardrail(),
            started(),
            ended(),
            handled()
        ]
    );
    assert!(answer(result.new_items()).1);

    let case = Case::new(ToolApprovalPolicy::Always).failure(ToolFailureHandling::Propagate);
    let first = interrupted(&case).await;
    let error = resumed(&case, &first, true).await.unwrap_err();
    assert_eq!(error.code(), "tool.invalid_input", "{error}");
    assert_eq!(
        case.journal(),
        [pre(), permission(), guardrail(), started(), ended()]
    );

    let case = Case::new(ToolApprovalPolicy::Always).failure(ToolFailureHandling::Propagate);
    let first = interrupted(&case).await;
    let result = resumed(&case, &first, false).await.unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(case.journal(), [pre(), permission()]);
    assert!(answer(result.new_items()).1);
}

/// Item 6d: a call PreToolUse rewrote before it was asked about is approved as rewritten, and runs
/// as rewritten after the checkpoint is read back.
#[tokio::test]
async fn a_rewritten_call_is_asked_and_resumed_as_rewritten() {
    let fixed = json!({"fixed": true});
    let case = Case::new(ToolApprovalPolicy::Always).pre(HookDecision::UpdateInput {
        input: fixed.clone(),
    });
    let first = interrupted(&case).await;
    let RunOutcome::Interrupted { items } = first.outcome() else {
        unreachable!()
    };
    let RunItemKind::ToolApproval(approval) = items[0].kind() else {
        panic!("expected a tool approval");
    };
    assert_eq!(approval.arguments(), &fixed);
    assert_eq!(approval.unparsed_arguments(), None);
    let result = resumed(&case, &first, true).await.unwrap();
    assert_eq!(answer(result.new_items()), ("ran".to_owned(), false));
    assert!(case.journal().contains(&format!("call({fixed})")));
    assert!(!case.journal().contains(&handled()));
}

/// Item 6c: with prompts off, a question becomes a refusal; with pre-approval guardrails on, the
/// guardrail sees the text before the host is asked and again after the approval.
#[tokio::test]
async fn the_existing_permission_chain_still_applies() {
    let case = Case::new(ToolApprovalPolicy::Always).without_prompts();
    let result = case.run().await.unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(case.journal(), [pre()]);
    assert!(answer(result.new_items()).1);

    let case = Case::new(ToolApprovalPolicy::Always).pre_approval_guardrails();
    let first = interrupted(&case).await;
    assert_eq!(case.journal(), [pre(), guardrail(), permission()]);
    resumed(&case, &first, true).await.unwrap();
    assert_eq!(
        case.journal(),
        [
            pre(),
            guardrail(),
            permission(),
            guardrail(),
            started(),
            ended(),
            handled()
        ]
    );
}

/// Under the product's choice, nobody is asked, including by a fixed `ask` rule that prompts being
/// off would otherwise turn into a refusal: the call is answered with its argument error. A `deny`
/// rule and a class restriction still refuse it before PreToolUse.
#[tokio::test]
async fn reported_first_skips_a_question_that_prompts_off_would_refuse() {
    for case in [
        Case::new(ToolApprovalPolicy::Never)
            .rule(PermissionDecision::Ask)
            .without_prompts()
            .reported_first(),
        Case::new(ToolApprovalPolicy::Always)
            .without_prompts()
            .reported_first(),
        Case::new(ToolApprovalPolicy::Dynamic)
            .rule(PermissionDecision::Ask)
            .reported_first(),
    ] {
        let result = case.run().await.unwrap();
        assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
        assert_eq!(
            case.journal(),
            [pre(), guardrail(), started(), ended(), handled()]
        );
        assert_eq!(
            answer(result.new_items()),
            ("the probe could not read its arguments".to_owned(), true)
        );
    }

    for case in [
        Case::new(ToolApprovalPolicy::Never)
            .rule(PermissionDecision::Deny)
            .without_prompts()
            .reported_first(),
        Case::new(ToolApprovalPolicy::Never)
            .restricted()
            .reported_first(),
        Case::new(ToolApprovalPolicy::Never)
            .rule(PermissionDecision::Allow)
            .restricted()
            .reported_first(),
    ] {
        let result = case.run().await.unwrap();
        assert!(case.journal().is_empty(), "{:?}", case.journal());
        let (text, is_error) = answer(result.new_items());
        assert!(text.contains("tool.permission_denied"), "{text}");
        assert!(is_error);
    }
}

/// The repeat breaker tells argument text that was not JSON from every parsed value: a model that
/// corrects `{bad` into a real `null` has changed its call and is not refused as repeating it,
/// while the same text sent again is a repeat.
#[tokio::test]
async fn the_repeat_breaker_tells_unparsed_text_from_a_parsed_null() {
    let case = Case::new(ToolApprovalPolicy::Never).repeat_limit(2);
    let result = Runner::run(case.request(vec![
        calls_probe_as("call-1", Err(TEXT)),
        calls_probe_as("call-2", Ok(Value::Null)),
        finishes(),
    ]))
    .await
    .unwrap();
    assert_eq!(
        answer_of(result.new_items(), "call-2"),
        ("ran".to_owned(), false)
    );
    assert!(case.journal().contains(&"call(null)".to_owned()));

    // The JSON string with the same characters is a different call too.
    let case = Case::new(ToolApprovalPolicy::Never).repeat_limit(2);
    let result = Runner::run(case.request(vec![
        calls_probe_as("call-1", Err(TEXT)),
        calls_probe_as("call-2", Ok(json!(TEXT))),
        finishes(),
    ]))
    .await
    .unwrap();
    assert_eq!(
        answer_of(result.new_items(), "call-2"),
        ("ran".to_owned(), false)
    );

    let case = Case::new(ToolApprovalPolicy::Never).repeat_limit(2);
    let result = Runner::run(case.request(vec![
        calls_probe_as("call-1", Err(TEXT)),
        calls_probe_as("call-2", Err(TEXT)),
        finishes(),
    ]))
    .await
    .unwrap();
    let (text, is_error) = answer_of(result.new_items(), "call-2");
    assert!(text.contains("tool.repeated_call"), "{text}");
    assert!(is_error);

    // Different text is a different call.
    let case = Case::new(ToolApprovalPolicy::Never).repeat_limit(2);
    let result = Runner::run(case.request(vec![
        calls_probe_as("call-1", Err(TEXT)),
        calls_probe_as("call-2", Err("{worse")),
        finishes(),
    ]))
    .await
    .unwrap();
    let (text, _) = answer_of(result.new_items(), "call-2");
    assert_eq!(text, "the probe could not read its arguments");
}

/// An exclusive tool that holds the batch for a while once it is running.
struct Holding {
    origin: ToolOrigin,
    schema: ToolSchema,
    entered: Arc<tokio::sync::Notify>,
    journal: Journal,
}

#[async_trait]
impl Tool for Holding {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.journal.lock().unwrap().push("holding body".to_owned());
        self.entered.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        self.journal
            .lock()
            .unwrap()
            .push("holding body done".to_owned());
        Ok(ToolOutput::text("held"))
    }
}

/// Holds the unparsed call's PreToolUse until the exclusive tool is running, so the call reaches
/// the concurrency gate while the batch is held.
struct WaitForHolding(Arc<tokio::sync::Notify>);

#[async_trait]
impl UserHook for WaitForHolding {
    fn name(&self) -> &str {
        "wait"
    }
    async fn call(
        &self,
        _context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        if let HookEvent::PreToolUse(call) = event
            && call.has_unparsed_arguments()
        {
            self.0.notified().await;
        }
        Ok(HookDecision::Continue)
    }
}

/// Names each lifecycle moment by tool.
struct NamedNarration(Journal);

#[async_trait]
impl LifecycleHook for NamedNarration {
    fn name(&self) -> &str {
        "named"
    }
    async fn on_tool_start(&self, _: LifecycleScope, input: &ToolStartInput<'_>) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("start {}", input.origin().name()));
        Ok(())
    }
    async fn on_tool_end(&self, _: LifecycleScope, input: &ToolEndInput<'_>) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("end {}", input.origin().name()));
        Ok(())
    }
}

/// An admitted call whose text was not JSON starts only once the concurrency gate admits it, as
/// any invocation does: while an exclusive tool of the same batch is running, its start waits.
#[tokio::test]
async fn an_unparsed_call_starts_only_after_the_concurrency_gate_admits_it() {
    let journal = Journal::default();
    let entered = Arc::new(tokio::sync::Notify::new());
    let schema = |name: &str| {
        ToolSchema::new(
            name,
            json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
        )
        .unwrap()
    };
    let holding = Holding {
        origin: ToolOrigin::new("holding").unwrap(),
        schema: schema("holding"),
        entered: Arc::clone(&entered),
        journal: Arc::clone(&journal),
    };
    let probe = Probe {
        origin: ToolOrigin::new("probe").unwrap(),
        schema: schema("probe"),
        approval: ToolApprovalPolicy::Never,
        failure: ToolFailureHandling::Custom,
        repeat_limit: None,
        journal: Arc::clone(&journal),
    };
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .tool(Arc::new(holding))
        .tool(Arc::new(probe))
        .build()
        .unwrap();
    let batch = ModelResponse::new(vec![
        RunItem::new(
            ItemId::new("item-hold"),
            RunItemKind::ToolCall(ToolCall::new(CallId::new("hold"), "holding", json!({}))),
        ),
        RunItem::new(
            ItemId::new("item-call-1"),
            RunItemKind::ToolCall(ToolCall::unparsed(CallId::new("call-1"), "probe", TEXT)),
        ),
    ]);
    let config = RunConfig::new()
        .with_tool_input_guardrail(Arc::new(Check {
            id: ToolGuardrailId::new("check").unwrap(),
            journal: Arc::clone(&journal),
            refuses: false,
        }))
        .with_lifecycle_hook(Arc::new(NamedNarration(Arc::clone(&journal))))
        .with_user_hook(UserHookRegistration::new(
            HookEventName::PreToolUse,
            Arc::new(WaitForHolding(entered)) as Arc<dyn UserHook>,
        ));
    let result = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(Resolver(Arc::new(ScriptedModel(Mutex::new(vec![
                batch,
                finishes(),
            ]))))),
            RunId::new("run-gate"),
            CancelScope::root(),
            Vec::new(),
        )
        .with_config(config),
    )
    .await
    .unwrap();
    assert!(answer(result.new_items()).1);
    let journal = journal.lock().unwrap().clone();
    let position = |entry: &str| {
        journal
            .iter()
            .position(|seen| seen == entry)
            .unwrap_or_else(|| panic!("{entry} missing from {journal:?}"))
    };
    // The gate is released when the exclusive tool's body returns, before its `on_tool_end`, so
    // the start is compared with the body's end rather than with that callback.
    assert!(
        position("holding body done") < position("start probe"),
        "the unparsed call started while the exclusive tool ran: {journal:?}"
    );
    assert!(position("start probe") < position("end probe"));
}
