//! `on_tool_end` is raised with the answer a started call settles into, after everything that
//! shapes it, as the reference raises it with `final_result`; a call that leaves as an error raises
//! only its start.

use std::{
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result, ToolErrorKind},
    guardrail::{
        ToolGuardrailFunctionOutput, ToolInputGuardrail, ToolInputGuardrailData,
        ToolOutputGuardrail, ToolOutputGuardrailData,
    },
    item::{CallId, ToolCallKind, ToolCallOutput},
    lifecycle::{LifecycleHook, LifecycleScope, ToolEndInput, ToolStartInput},
    permission::{PermissionDecision, PermissionRule},
    state::RunId,
    tool::{
        DecodedToolInput, FuncSchema, FunctionToolCustomDataContext, Tool, ToolApprovalPolicy,
        ToolContext, ToolFailureHandling, ToolGuardrailId, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema, ToolTimeoutBehavior, ToolTimeoutError,
    },
};
use ra_runtime::{
    lifecycle::LifecycleHooks,
    permission::PermissionEngine,
    tool::{
        dispatch::{CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool},
        guardrail::ToolGuardrails,
    },
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// How the probe's body ends.
#[derive(Clone, Copy)]
enum Body {
    Succeeds,
    Fails,
    Hangs,
    /// Reports a cancellation of its own while its call's scope is live.
    Cancels,
    /// Cancels its call's scope, then reports the cancellation in the same poll.
    CancelsItsScope,
}

#[derive(serde::Deserialize, schemars::JsonSchema, ra_macros::ToolInput)]
struct ProbeInput {}

/// A tool whose body, failure text, timeout text and extractor are each chosen per case, and which
/// writes into the same log as the narration so their order is observable.
struct Probe {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    body: Body,
    failure_text: Option<&'static str>,
    timeout_text: Option<&'static str>,
    extractor: Option<Result<&'static str>>,
    decode_error: bool,
    scope: Option<CancelScope>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Probe {
    fn new(body: Body, options: ToolOptions, log: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            origin: ToolOrigin::new("probe").expect("a tool identity"),
            schema: ToolSchema::new(
                "probe",
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .expect("a tool schema"),
            options,
            body,
            failure_text: None,
            timeout_text: None,
            extractor: None,
            decode_error: false,
            scope: None,
            log: Arc::clone(log),
        }
    }

    fn write(&self, entry: impl Into<String>) {
        self.log.lock().expect("no poisoned log").push(entry.into());
    }
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
        self.options.clone()
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        static SCHEMA: LazyLock<FuncSchema> =
            LazyLock::new(|| FuncSchema::for_input::<ProbeInput>("probe").unwrap());
        self.decode_error.then(|| &*SCHEMA)
    }

    fn approval_arguments_inspectable(&self, _: &Value) -> bool {
        !self.decode_error
    }

    async fn needs_approval(&self, _: &ToolContext<'_>) -> Result<bool> {
        self.write("approval policy");
        Ok(false)
    }

    async fn approval_state(&self, _: &ToolContext<'_>) -> Result<Option<Value>> {
        self.write("approval state");
        Err(Error::tool(
            ToolErrorKind::InvalidInput,
            "probe",
            "the approval input could not be prepared",
        ))
    }

    fn decode_input(&self, _: &Value) -> Result<Option<DecodedToolInput>> {
        if self.decode_error {
            self.write("decode");
            return Err(Error::tool(
                ToolErrorKind::InvalidInput,
                "probe",
                "the arguments could not be decoded",
            ));
        }
        Ok(None)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.write("body");
        match self.body {
            Body::Succeeds => Ok(ToolOutput::text("the tool ran")),
            Body::Fails => Err(Error::tool(
                ToolErrorKind::ExecutionFailed,
                "probe",
                "the tool could not finish",
            )),
            Body::Hangs => std::future::pending().await,
            Body::Cancels => Err(Error::cancelled("the tool stopped itself")),
            Body::CancelsItsScope => {
                self.scope
                    .as_ref()
                    .expect("the call's scope")
                    .cancel(ra_core::cancel::CancelReason::UserInterrupt);
                Err(Error::cancelled("the tool saw its scope stop"))
            }
        }
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        _error: &Error,
    ) -> Result<Option<ToolOutput>> {
        self.write("failure handler");
        Ok(self.failure_text.map(ToolOutput::text))
    }

    async fn handle_timeout(
        &self,
        _context: &ToolContext<'_>,
        _error: &ToolTimeoutError,
    ) -> Result<Option<ToolOutput>> {
        self.write("timeout handler");
        Ok(self.timeout_text.map(ToolOutput::text))
    }

    async fn extract_custom_data(
        &self,
        _context: &mut FunctionToolCustomDataContext<'_>,
    ) -> Result<Option<serde_json::Map<String, Value>>> {
        let Some(extractor) = &self.extractor else {
            return Ok(None);
        };
        self.write("extractor");
        match extractor {
            Ok(value) => Ok(Some(
                json!({ "extracted": value })
                    .as_object()
                    .expect("an object")
                    .clone(),
            )),
            Err(_) => Err(Error::caller("the extractor failed")),
        }
    }
}

/// Narration that writes both moments into the shared log and keeps every end it was told about.
struct Narration {
    log: Arc<Mutex<Vec<String>>>,
    ends: Mutex<Vec<(ToolCallOutput, Option<&'static str>)>>,
}

impl Narration {
    fn new(log: &Arc<Mutex<Vec<String>>>) -> Arc<Self> {
        Arc::new(Self {
            log: Arc::clone(log),
            ends: Mutex::new(Vec::new()),
        })
    }

    fn ends(&self) -> Vec<(ToolCallOutput, Option<&'static str>)> {
        self.ends.lock().expect("no poisoned log").clone()
    }
}

#[async_trait]
impl LifecycleHook for Narration {
    fn name(&self) -> &str {
        "narration"
    }

    async fn on_tool_start(&self, _: LifecycleScope, _: &ToolStartInput<'_>) -> Result<()> {
        self.log
            .lock()
            .expect("no poisoned log")
            .push("start".into());
        Ok(())
    }

    async fn on_tool_end(&self, _: LifecycleScope, input: &ToolEndInput<'_>) -> Result<()> {
        self.log
            .lock()
            .expect("no poisoned log")
            .push(format!("end/{}", input.failure_code().unwrap_or("ok")));
        self.ends
            .lock()
            .expect("no poisoned log")
            .push((input.output().clone(), input.failure_code()));
        Ok(())
    }
}

/// A guardrail with a fixed verdict that records the boundary where it ran.
struct GuardrailCheck {
    id: ToolGuardrailId,
    verdict: ToolGuardrailFunctionOutput,
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ToolInputGuardrail for GuardrailCheck {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }

    async fn check(&self, _: &ToolInputGuardrailData<'_>) -> Result<ToolGuardrailFunctionOutput> {
        self.log
            .lock()
            .expect("no poisoned log")
            .push("input guardrail".into());
        Ok(self.verdict.clone())
    }
}

#[async_trait]
impl ToolOutputGuardrail for GuardrailCheck {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }

    async fn check(
        &self,
        _data: &ToolOutputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        self.log
            .lock()
            .expect("no poisoned log")
            .push("output guardrail".into());
        Ok(self.verdict.clone())
    }
}

fn check_id() -> ToolGuardrailId {
    ToolGuardrailId::new("output_check").expect("a guardrail identity")
}

fn guarded(verdict: ToolGuardrailFunctionOutput, log: &Arc<Mutex<Vec<String>>>) -> ToolGuardrails {
    ToolGuardrails::install(
        Vec::new(),
        vec![Arc::new(GuardrailCheck {
            id: check_id(),
            verdict,
            log: Arc::clone(log),
        }) as Arc<dyn ToolOutputGuardrail>],
        false,
    )
    .expect("a registered output guardrail")
}

fn request(probe: Probe, narration: &Arc<Narration>) -> ToolDispatchRequest {
    request_in(probe, narration, CancelScope::root())
}

fn request_in(probe: Probe, narration: &Arc<Narration>, scope: CancelScope) -> ToolDispatchRequest {
    request_with_permission(probe, narration, scope, PermissionEngine::default())
}

fn request_with_permission(
    probe: Probe,
    narration: &Arc<Narration>,
    scope: CancelScope,
    permission: PermissionEngine,
) -> ToolDispatchRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("settlement-agent"))
        .name("Settlement agent")
        .build()
        .expect("an agent");
    let hooks: Vec<Arc<dyn LifecycleHook>> = vec![narration.clone()];
    ToolDispatchRequest::new(
        Arc::new(probe),
        CallId::new("call-1"),
        json!({}),
        Arc::new(RunContext::new(RunId::new("run-settlement"), &agent)),
        scope,
        CallHistory::default(),
        permission,
    )
    .with_lifecycle_hooks(LifecycleHooks::installed(&hooks, &[]))
}

fn seen(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().expect("no poisoned log").clone()
}

/// The answer the dispatch returned, which the end must have been told about verbatim.
fn answer(dispatch: &ToolDispatch) -> &ToolCallOutput {
    match dispatch {
        ToolDispatch::Observed(observation) => observation.output(),
        ToolDispatch::Refused(refusal) => refusal.output(),
        other => panic!("a settled call, not {other:?}"),
    }
}

fn text(output: &ToolCallOutput) -> String {
    ToolOutput::from_stored(output.output())
        .expect("a readable tool result")
        .expect("a tool result")
        .as_text()
        .expect("text")
        .to_owned()
}

// ---------------------------------------------------------------------------
// failures the policy answers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_model_visible_failure_ends_with_the_failure_object_the_model_is_shown() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let probe = Probe::new(Body::Fails, ToolOptions::new(), &log);

    let outcome = dispatch_tool(request(probe, &narration))
        .await
        .expect("a model-visible failure answers the call");

    assert_eq!(seen(&log), ["start", "body", "end/tool.execution_failed"]);
    let ends = narration.ends();
    assert_eq!(ends.len(), 1);
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert_eq!(
        ends[0].0.output(),
        &json!({"error": {"code": "tool.execution_failed", "tool": "probe"}})
    );
    assert!(ends[0].0.is_error());
}

#[tokio::test]
async fn a_failure_the_tool_words_itself_ends_with_that_text() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(
        Body::Fails,
        ToolOptions::new().with_failure_handling(ToolFailureHandling::Custom),
        &log,
    );
    probe.failure_text = Some("two tests failed");

    let outcome = dispatch_tool(request(probe, &narration))
        .await
        .expect("the tool's own text answers the call");

    assert_eq!(
        seen(&log),
        [
            "start",
            "body",
            "failure handler",
            "end/tool.execution_failed"
        ]
    );
    let ends = narration.ends();
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert_eq!(text(&ends[0].0), "two tests failed");
}

#[tokio::test]
async fn a_failure_the_tool_declines_to_word_leaves_with_only_its_start() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let probe = Probe::new(
        Body::Fails,
        ToolOptions::new().with_failure_handling(ToolFailureHandling::Custom),
        &log,
    );

    let error = dispatch_tool(request(probe, &narration))
        .await
        .expect_err("an unhandled failure propagates");

    assert_eq!(error.code(), "tool.execution_failed");
    assert_eq!(seen(&log), ["start", "body", "failure handler"]);
    assert!(narration.ends().is_empty());
}

// ---------------------------------------------------------------------------
// output guardrails
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_output_guardrail_refusal_ends_with_the_message_that_replaced_the_result() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let probe = Probe::new(
        Body::Succeeds,
        ToolOptions::new().with_output_guardrail(check_id()),
        &log,
    );

    let outcome = dispatch_tool(request(probe, &narration).with_tool_guardrails(guarded(
        ToolGuardrailFunctionOutput::reject_content("the result was withheld"),
        &log,
    )))
    .await
    .expect("a refusal answers the call");

    assert_eq!(
        seen(&log),
        [
            "start",
            "body",
            "output guardrail",
            "end/guardrail.tool_output"
        ]
    );
    let ends = narration.ends();
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert_eq!(text(&ends[0].0), "the result was withheld");
}

#[tokio::test]
async fn a_handled_failure_is_checked_before_its_end_and_a_refusal_replaces_it() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(
        Body::Fails,
        ToolOptions::new()
            .with_failure_handling(ToolFailureHandling::Custom)
            .with_output_guardrail(check_id()),
        &log,
    );
    probe.failure_text = Some("internal path /srv/secret leaked");

    let outcome = dispatch_tool(request(probe, &narration).with_tool_guardrails(guarded(
        ToolGuardrailFunctionOutput::reject_content("redacted"),
        &log,
    )))
    .await
    .expect("a refusal answers the call");

    assert_eq!(
        seen(&log),
        [
            "start",
            "body",
            "failure handler",
            "output guardrail",
            "end/guardrail.tool_output"
        ]
    );
    let ends = narration.ends();
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert_eq!(text(&ends[0].0), "redacted");
}

#[tokio::test]
async fn an_output_guardrail_that_raises_leaves_with_only_the_start() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let probe = Probe::new(
        Body::Succeeds,
        ToolOptions::new().with_output_guardrail(check_id()),
        &log,
    );

    dispatch_tool(request(probe, &narration).with_tool_guardrails(guarded(
        ToolGuardrailFunctionOutput::raise_exception(),
        &log,
    )))
    .await
    .expect_err("the tripwire ends the run");

    assert_eq!(seen(&log), ["start", "body", "output guardrail"]);
    assert!(narration.ends().is_empty());
}

// ---------------------------------------------------------------------------
// timeouts
// ---------------------------------------------------------------------------

fn timing_out(behavior: ToolTimeoutBehavior) -> ToolOptions {
    ToolOptions::new()
        .with_timeout(Duration::from_millis(20))
        .with_timeout_behavior(behavior)
}

#[tokio::test]
async fn a_model_visible_timeout_ends_with_the_timeout_message() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let probe = Probe::new(
        Body::Hangs,
        timing_out(ToolTimeoutBehavior::ModelVisible),
        &log,
    );

    let outcome = dispatch_tool(request(probe, &narration))
        .await
        .expect("the timeout answers the call");

    assert_eq!(
        seen(&log),
        ["start", "body", "timeout handler", "end/tool.timeout"]
    );
    let ends = narration.ends();
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert!(text(&ends[0].0).contains("timed out"), "{:?}", ends[0].0);
}

#[tokio::test]
async fn a_timeout_the_tool_words_itself_ends_with_that_text() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(
        Body::Hangs,
        timing_out(ToolTimeoutBehavior::ModelVisible),
        &log,
    );
    probe.timeout_text = Some("still indexing; try a narrower path");

    let outcome = dispatch_tool(request(probe, &narration))
        .await
        .expect("the timeout answers the call");

    let ends = narration.ends();
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert_eq!(text(&ends[0].0), "still indexing; try a narrower path");
    assert_eq!(ends[0].1, Some("tool.timeout"));
}

#[tokio::test]
async fn a_propagated_timeout_leaves_with_only_the_start() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let probe = Probe::new(
        Body::Hangs,
        timing_out(ToolTimeoutBehavior::Propagate),
        &log,
    );

    let error = dispatch_tool(request(probe, &narration))
        .await
        .expect_err("the timeout propagates");

    assert_eq!(error.code(), "tool.timeout");
    assert_eq!(seen(&log), ["start", "body"]);
    assert!(narration.ends().is_empty());
}

// ---------------------------------------------------------------------------
// custom data
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_end_follows_custom_data_extraction_and_carries_the_record_it_produced() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(Body::Succeeds, ToolOptions::new(), &log);
    probe.extractor = Some(Ok("receipt-7"));

    let outcome = dispatch_tool(request(probe, &narration))
        .await
        .expect("the call runs");

    assert_eq!(seen(&log), ["start", "body", "extractor", "end/ok"]);
    let ends = narration.ends();
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert_eq!(text(&ends[0].0), "the tool ran");
    assert_eq!(
        ends[0]
            .0
            .custom_data()
            .and_then(|data| data.get("extracted")),
        Some(&json!("receipt-7"))
    );
}

#[tokio::test]
async fn a_failing_extractor_leaves_with_only_the_start() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(Body::Succeeds, ToolOptions::new(), &log);
    probe.extractor = Some(Err(Error::caller("unused")));

    let error = dispatch_tool(request(probe, &narration))
        .await
        .expect_err("the extractor's failure propagates");

    assert!(
        error.to_string().contains("the extractor failed"),
        "{error}"
    );
    assert_eq!(seen(&log), ["start", "body", "extractor"]);
    assert!(narration.ends().is_empty());
}

#[tokio::test]
async fn permission_denial_precedes_typed_argument_decoding() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(
        Body::Succeeds,
        ToolOptions::new().with_output_guardrail(check_id()),
        &log,
    );
    probe.decode_error = true;
    probe.extractor = Some(Ok("extracted before permission"));
    let permission = PermissionEngine::default()
        .with_rules([PermissionRule::new(PermissionDecision::Deny).with_tool_name("probe")]);
    let request = request_with_permission(probe, &narration, CancelScope::root(), permission)
        .with_tool_guardrails(guarded(ToolGuardrailFunctionOutput::allow(), &log));

    let outcome = dispatch_tool(request)
        .await
        .expect("permission denied the call");
    assert_eq!(seen(&log), Vec::<String>::new());
    let output = answer(outcome.dispatch());
    assert_eq!(
        output.output(),
        &json!({"error": {"code": "tool.permission_denied", "tool": "probe"}})
    );
    assert!(output.custom_data().is_none());
}

#[tokio::test]
async fn uninspectable_typed_arguments_are_asked_about_without_tool_preparation() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(
        Body::Succeeds,
        ToolOptions::new().with_approval(ToolApprovalPolicy::Dynamic),
        &log,
    );
    probe.decode_error = true;
    let outcome = dispatch_tool(request(probe, &narration)).await.unwrap();
    let ToolDispatch::AwaitingApproval(approval) = outcome.dispatch() else {
        panic!("uninspectable arguments require approval");
    };
    assert_eq!(approval.arguments(), &json!({}));
    assert!(approval.resume().is_none());
    assert!(seen(&log).is_empty());
}

#[tokio::test]
async fn the_input_guardrail_can_refuse_before_typed_argument_decoding() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(
        Body::Succeeds,
        ToolOptions::new().with_input_guardrail(check_id()),
        &log,
    );
    probe.decode_error = true;
    let guardrails = ToolGuardrails::install(
        vec![Arc::new(GuardrailCheck {
            id: check_id(),
            verdict: ToolGuardrailFunctionOutput::reject_content("blocked input"),
            log: Arc::clone(&log),
        }) as Arc<dyn ToolInputGuardrail>],
        Vec::new(),
        false,
    )
    .unwrap();
    let outcome = dispatch_tool(request(probe, &narration).with_tool_guardrails(guardrails))
        .await
        .unwrap();
    assert_eq!(seen(&log), ["input guardrail"]);
    assert_eq!(text(answer(outcome.dispatch())), "blocked input");
}

#[tokio::test]
async fn typed_decode_failure_settles_after_start_or_propagates() {
    for handling in [ToolFailureHandling::Custom, ToolFailureHandling::Propagate] {
        let log = Arc::new(Mutex::new(Vec::new()));
        let narration = Narration::new(&log);
        let mut probe = Probe::new(
            Body::Succeeds,
            ToolOptions::new()
                .with_failure_handling(handling)
                .with_output_guardrail(check_id()),
            &log,
        );
        probe.decode_error = true;
        probe.failure_text = Some("correct the argument");
        probe.extractor = Some(Ok("decode failure"));
        let outcome = dispatch_tool(
            request(probe, &narration)
                .with_tool_guardrails(guarded(ToolGuardrailFunctionOutput::allow(), &log)),
        )
        .await;
        if handling == ToolFailureHandling::Propagate {
            assert_eq!(outcome.unwrap_err().code(), "tool.invalid_input");
            assert_eq!(seen(&log), ["start", "decode"]);
        } else {
            let outcome = outcome.unwrap();
            assert_eq!(
                seen(&log),
                [
                    "start",
                    "decode",
                    "failure handler",
                    "output guardrail",
                    "extractor",
                    "end/tool.invalid_input"
                ]
            );
            let output = answer(outcome.dispatch());
            assert_eq!(text(output), "correct the argument");
            assert_eq!(
                narration.ends(),
                [(output.clone(), Some("tool.invalid_input"))]
            );
        }
    }
}

#[tokio::test]
async fn a_model_visible_failure_is_checked_and_extracted_before_its_end() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(
        Body::Fails,
        ToolOptions::new().with_output_guardrail(check_id()),
        &log,
    );
    probe.extractor = Some(Ok("failure-data"));

    let outcome = dispatch_tool(
        request(probe, &narration)
            .with_tool_guardrails(guarded(ToolGuardrailFunctionOutput::allow(), &log)),
    )
    .await
    .expect("a model-visible failure answers the call");

    assert_eq!(
        seen(&log),
        [
            "start",
            "body",
            "output guardrail",
            "extractor",
            "end/tool.execution_failed"
        ]
    );
    let ends = narration.ends();
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert_eq!(
        ends[0].0.output(),
        &json!({"error": {"code": "tool.execution_failed", "tool": "probe"}})
    );
    assert_eq!(
        ends[0]
            .0
            .custom_data()
            .and_then(|data| data.get("extracted")),
        Some(&json!("failure-data"))
    );
}

#[tokio::test]
async fn an_output_guardrail_refusal_replaces_a_model_visible_failure() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let probe = Probe::new(
        Body::Fails,
        ToolOptions::new().with_output_guardrail(check_id()),
        &log,
    );

    let outcome = dispatch_tool(request(probe, &narration).with_tool_guardrails(guarded(
        ToolGuardrailFunctionOutput::reject_content("the failure was withheld"),
        &log,
    )))
    .await
    .expect("a refusal answers the call");

    assert_eq!(
        seen(&log),
        [
            "start",
            "body",
            "output guardrail",
            "end/guardrail.tool_output"
        ]
    );
    let ends = narration.ends();
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert_eq!(text(&ends[0].0), "the failure was withheld");
}

// ---------------------------------------------------------------------------
// cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancellation_the_tool_reports_itself_is_answered_by_its_failure_policy() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let probe = Probe::new(Body::Cancels, ToolOptions::new(), &log);

    let outcome = dispatch_tool(request(probe, &narration))
        .await
        .expect("the scope is live, so the tool's own cancellation is its failure");

    assert_eq!(seen(&log), ["start", "body", "end/cancelled"]);
    let ends = narration.ends();
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
    assert_eq!(
        ends[0].0.output(),
        &json!({"error": {"code": "cancelled", "tool": "probe"}})
    );
}

#[tokio::test]
async fn a_cancellation_the_tool_reports_itself_reaches_its_own_failure_text() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(
        Body::Cancels,
        ToolOptions::new().with_failure_handling(ToolFailureHandling::Custom),
        &log,
    );
    probe.failure_text = Some("stopped before finishing");

    let outcome = dispatch_tool(request(probe, &narration))
        .await
        .expect("the tool's own text answers the call");

    assert_eq!(
        seen(&log),
        ["start", "body", "failure handler", "end/cancelled"]
    );
    assert_eq!(text(answer(outcome.dispatch())), "stopped before finishing");
}

#[tokio::test]
async fn a_cancellation_the_policy_propagates_leaves_with_only_the_start() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let probe = Probe::new(
        Body::Cancels,
        ToolOptions::new().with_failure_handling(ToolFailureHandling::Propagate),
        &log,
    );

    let error = dispatch_tool(request(probe, &narration))
        .await
        .expect_err("a propagated cancellation leaves the call");

    assert!(error.is_cancelled(), "{error:?}");
    assert_eq!(seen(&log), ["start", "body"]);
    assert!(narration.ends().is_empty());
}

#[tokio::test]
async fn a_cancelled_scope_is_never_handed_to_the_failure_policy() {
    // The same report from the tool, but the call's scope was cancelled first: this is a teardown,
    // so neither the failure policy, nor the extractor that would follow it, nor the end runs.
    for handling in [
        ToolFailureHandling::ModelVisible,
        ToolFailureHandling::Custom,
    ] {
        let log = Arc::new(Mutex::new(Vec::new()));
        let narration = Narration::new(&log);
        let mut probe = Probe::new(
            Body::CancelsItsScope,
            ToolOptions::new().with_failure_handling(handling),
            &log,
        );
        probe.failure_text = Some("never shown");
        probe.extractor = Some(Ok("never extracted"));
        let scope = CancelScope::root();
        probe.scope = Some(scope.clone());

        let error = dispatch_tool(request_in(probe, &narration, scope))
            .await
            .expect_err("a teardown leaves the call");

        assert!(error.is_cancelled(), "{handling:?}: {error:?}");
        assert_eq!(seen(&log), ["start", "body"], "{handling:?}");
        assert!(narration.ends().is_empty(), "{handling:?}");
    }
}

// ---------------------------------------------------------------------------
// the kind of call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_custom_tool_ends_under_the_custom_call_kind() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let narration = Narration::new(&log);
    let mut probe = Probe::new(Body::Succeeds, ToolOptions::new(), &log);
    probe.schema = ToolSchema::custom("probe", None).expect("a custom schema");

    let outcome = dispatch_tool(request(probe, &narration))
        .await
        .expect("the call runs");

    let ends = narration.ends();
    assert_eq!(ends[0].0.kind(), ToolCallKind::Custom);
    assert_eq!(&ends[0].0, answer(outcome.dispatch()));
}
