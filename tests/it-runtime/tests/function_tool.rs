//! Generated function tools use the same runner lifecycle as handwritten tools.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
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
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolConcurrency, ToolContext, ToolFailureHandling, ToolGuardrailId, ToolOptions,
        ToolOutput,
    },
};
use ra_macros::tool;
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunOutcome, RunRequest, Runner},
};
use serde_json::{Value, json};

#[derive(Default)]
struct App {
    bodies: AtomicUsize,
    extractions: AtomicUsize,
    approvals: AtomicUsize,
    enabled: bool,
    barrier: Option<Arc<tokio::sync::Barrier>>,
}

#[tool]
fn sum(context: ToolContext<'_>, a: u32, #[tool(default = 5)] b: u32) -> u32 {
    context
        .run()
        .app_context::<App>()
        .unwrap()
        .bodies
        .fetch_add(1, Ordering::SeqCst);
    a + b
}

async fn enabled(context: &RunContext) -> Result<bool> {
    Ok(context.app_context::<App>().unwrap().enabled)
}

async fn approval(context: &ToolContext<'_>) -> Result<bool> {
    context
        .run()
        .app_context::<App>()
        .unwrap()
        .approvals
        .fetch_add(1, Ordering::SeqCst);
    Ok(context.arguments()["a"] == 7)
}

#[tool(is_enabled = enabled, needs_approval = approval)]
async fn controlled(context: &ToolContext<'_>, a: u32) -> u32 {
    context
        .run()
        .app_context::<App>()
        .unwrap()
        .bodies
        .fetch_add(1, Ordering::SeqCst);
    a
}

#[tool(needs_approval = true)]
fn always_approve() -> &'static str {
    "approved"
}

#[tool]
async fn failing() -> Result<()> {
    Err(Error::tool(
        ToolErrorKind::ExecutionFailed,
        "failing",
        "private host detail",
    ))
}

#[tool(failure_error_function = None)]
async fn propagating() -> Result<()> {
    Err(Error::caller("propagated"))
}

async fn custom_failure(context: &ToolContext<'_>, error: &Error) -> Result<Option<ToolOutput>> {
    Ok(Some(ToolOutput::text(format!(
        "{}:{}",
        context.call_id(),
        error.code()
    ))))
}

#[tool(failure_error_function = custom_failure)]
fn custom_error() -> std::result::Result<(), std::io::Error> {
    Err(std::io::Error::other("private io detail"))
}

#[tool(timeout = Duration::from_millis(10))]
async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_secs(1)).await;
    "late"
}

#[tool(timeout = Duration::from_millis(10), timeout_behavior = "raise_exception")]
async fn slow_propagating() -> &'static str {
    slow().await
}

#[tool]
fn panic_sync() {
    panic!("private panic detail");
}

#[tool(tool_input_guardrails = ["check"], tool_output_guardrails = ["check"])]
async fn guarded(context: &ToolContext<'_>, value: String) -> String {
    context
        .run()
        .app_context::<App>()
        .unwrap()
        .bodies
        .fetch_add(1, Ordering::SeqCst);
    value
}

struct ScriptedModel {
    script: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}
impl ScriptedModel {
    fn new(script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}
#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("script exhausted"))
    }
}
struct Resolver(Arc<ScriptedModel>);
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test"),
                Some("scripted".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}
fn calls(name: &str, arguments: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("call-item"),
        RunItemKind::ToolCall(ToolCall::new(CallId::new("call-1"), name, arguments)),
    )])
}
fn finishes() -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("final"),
        RunItemKind::Message(Message::assistant("done", OutputPhase::Final)),
    )])
}
fn request(tool: Arc<dyn Tool>, model: &Arc<ScriptedModel>, app: &Arc<App>) -> RunRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("function-agent"))
        .name("Functions")
        .tool(tool)
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(model.clone())),
        RunId::new("function-run"),
        CancelScope::root(),
        vec![],
    )
    .with_app_context(app.clone())
}
fn output_text(items: &[RunItem]) -> String {
    items
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => ToolOutput::from_stored(output.output())
                .ok()??
                .as_text()
                .map(str::to_owned),
            _ => None,
        })
        .expect("one tool output")
}

#[tokio::test]
async fn runner_decodes_invokes_and_returns_generated_output_to_the_model() {
    let app = Arc::new(App::default());
    let model = ScriptedModel::new(vec![
        calls("sum", json!({"a": 2, "extra": true})),
        finishes(),
    ]);
    let result = Runner::run(request(Arc::new(sum::tool().unwrap()), &model, &app))
        .await
        .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(output_text(result.new_items()), "7");
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools()[0].name(), "sum");
    assert!(requests[1].input().iter().any(|item| matches!(item, ModelInputItem::ToolCallOutput(output) if output.call_id() == &CallId::new("call-1") && ToolOutput::from_stored(output.output()).unwrap().unwrap().as_text() == Some("7"))));
}

#[tokio::test]
async fn invalid_input_uses_the_default_sanitized_failure_and_never_runs_the_body() {
    let app = Arc::new(App::default());
    let model = ScriptedModel::new(vec![calls("sum", json!({"a": "bad"})), finishes()]);
    let result = Runner::run(request(Arc::new(sum::tool().unwrap()), &model, &app))
        .await
        .unwrap();
    assert_eq!(app.bodies.load(Ordering::SeqCst), 0);
    assert_eq!(
        output_text(result.new_items()),
        "An error occurred while running the tool. Please try again."
    );
}

#[tokio::test]
async fn enabled_callback_controls_advertising_and_dispatch() {
    let app = Arc::new(App::default());
    let model = ScriptedModel::new(vec![calls("controlled", json!({"a": 1})), finishes()]);
    let result = Runner::run(request(Arc::new(controlled::tool().unwrap()), &model, &app))
        .await
        .unwrap();
    assert_eq!(app.bodies.load(Ordering::SeqCst), 0);
    assert!(model.requests.lock().unwrap()[0].tools().is_empty());
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
}

#[tokio::test]
async fn approval_round_trips_a_checkpoint_and_invokes_the_body_once() {
    let app = Arc::new(App {
        enabled: true,
        ..App::default()
    });
    let tool: Arc<dyn Tool> = Arc::new(controlled::tool().unwrap());
    let model = ScriptedModel::new(vec![calls("controlled", json!({"a": 7}))]);
    let result = Runner::run(request(tool.clone(), &model, &app))
        .await
        .unwrap();
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("approval required")
    };
    assert_eq!(app.bodies.load(Ordering::SeqCst), 0);
    let mut state: RunState =
        serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
    state.approve(&items[0], false).unwrap();
    let model = ScriptedModel::new(vec![finishes()]);
    let result = Runner::run(request(tool, &model, &app).with_state(state))
        .await
        .unwrap();
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
    assert_eq!(output_text(result.new_items()), "7");
    assert_eq!(result.final_text(), "done");

    let model = ScriptedModel::new(vec![calls("always_approve", json!({}))]);
    let result = Runner::run(request(
        Arc::new(always_approve::tool().unwrap()),
        &model,
        &app,
    ))
    .await
    .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Interrupted { .. }));
}

#[tokio::test]
async fn invalid_typed_arguments_wait_for_approval_before_failure_handling() {
    for arguments in [json!({"a": "bad"}), json!({"a": -1})] {
        for approve in [false, true] {
            let app = Arc::new(App {
                enabled: true,
                ..App::default()
            });
            let lifecycle = Arc::new(EndAnswers::default());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let guardrail = Arc::new(Check {
                id: ToolGuardrailId::new("check").unwrap(),
                seen: seen.clone(),
                reject: false,
            });
            let config = RunConfig::new()
                .with_lifecycle_hook(lifecycle.clone())
                .with_tool_input_guardrail(guardrail);
            let options = controlled::options()
                .unwrap()
                .with_input_guardrail(ToolGuardrailId::new("check").unwrap());
            let tool: Arc<dyn Tool> = Arc::new(controlled::tool_with_options(options).unwrap());
            let model = ScriptedModel::new(vec![calls("controlled", arguments.clone())]);
            let paused =
                Runner::run(request(tool.clone(), &model, &app).with_config(config.clone()))
                    .await
                    .unwrap();
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("uninspectable arguments require approval");
            };
            let RunItemKind::ToolApproval(approval) = items[0].kind() else {
                panic!("expected a tool approval");
            };
            assert_eq!(approval.arguments(), &arguments);
            assert!(approval.resume().is_none());
            assert_eq!(app.approvals.load(Ordering::SeqCst), 0);
            assert!(lifecycle.0.lock().unwrap().is_empty());
            assert_eq!(lifecycle.1.load(Ordering::SeqCst), 0);
            assert!(seen.lock().unwrap().is_empty());
            let mut state: RunState =
                serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
            if approve {
                state.approve(&items[0], false).unwrap();
            } else {
                state.reject(&items[0], false).unwrap();
            }
            let model = ScriptedModel::new(vec![finishes()]);
            let result = Runner::run(
                request(tool, &model, &app)
                    .with_config(config)
                    .with_state(state),
            )
            .await
            .unwrap();
            let answer = the_answer(result.new_items());
            let ends = lifecycle.0.lock().unwrap();
            assert_eq!(lifecycle.1.load(Ordering::SeqCst), usize::from(approve));
            if approve {
                assert_eq!(*seen.lock().unwrap(), ["input"]);
                assert_eq!(*ends, [answer]);
                assert_eq!(
                    output_text(result.new_items()),
                    "An error occurred while running the tool. Please try again."
                );
            } else {
                assert!(seen.lock().unwrap().is_empty());
                assert!(ends.is_empty());
                assert!(answer.is_error());
            }
            assert_eq!(app.approvals.load(Ordering::SeqCst), 0);
            assert_eq!(app.bodies.load(Ordering::SeqCst), 0);
        }
    }
}

static DEFAULT_CALLS: AtomicUsize = AtomicUsize::new(0);

fn default_protected_number() -> u32 {
    DEFAULT_CALLS.fetch_add(1, Ordering::SeqCst);
    7
}

#[tool(needs_approval = approval)]
fn with_approval_default(#[tool(default = default_protected_number())] a: u32) -> u32 {
    a
}

#[tokio::test]
async fn dynamic_approval_checks_explicit_defaulted_fields_without_running_the_default() {
    for (arguments, checks, interrupted) in [
        (json!({}), 0, true),
        (json!({"a": 7}), 1, true),
        (json!({"a": 1}), 1, false),
    ] {
        let app = Arc::new(App::default());
        let model = ScriptedModel::new(vec![calls("with_approval_default", arguments), finishes()]);
        let tool = Arc::new(with_approval_default::tool().unwrap());
        let defaults_before_dispatch = DEFAULT_CALLS.load(Ordering::SeqCst);
        let result = Runner::run(request(tool, &model, &app)).await.unwrap();
        assert_eq!(
            matches!(result.outcome(), RunOutcome::Interrupted { .. }),
            interrupted
        );
        assert_eq!(app.approvals.load(Ordering::SeqCst), checks);
        assert_eq!(
            DEFAULT_CALLS.load(Ordering::SeqCst),
            defaults_before_dispatch
        );
        if !interrupted {
            assert_eq!(output_text(result.new_items()), "1");
        }
    }
}

#[tool(needs_approval = approval)]
fn double_for_approval(value: f64) -> f64 {
    value
}

#[tool(needs_approval = approval)]
fn float_for_approval(value: f32) -> f32 {
    value
}

#[tool(strict = false, needs_approval = approval)]
fn nested_for_approval(value: std::collections::BTreeMap<String, Vec<Option<f64>>>) -> usize {
    value.len()
}

#[tool(strict = false, needs_approval = approval)]
fn numeric_keys_for_approval(value: std::collections::HashMap<i32, u32>) -> usize {
    value.len()
}

#[tokio::test]
async fn dynamic_approval_requires_arguments_that_bind_without_changes() {
    let double: Arc<dyn Tool> = Arc::new(double_for_approval::tool().unwrap());
    let float: Arc<dyn Tool> = Arc::new(float_for_approval::tool().unwrap());
    let nested: Arc<dyn Tool> = Arc::new(nested_for_approval::tool().unwrap());
    let numeric_keys: Arc<dyn Tool> = Arc::new(numeric_keys_for_approval::tool().unwrap());
    let integer: Arc<dyn Tool> = Arc::new(controlled::tool().unwrap());
    for (tool, arguments, interrupted) in [
        (double.clone(), json!({"value": 1}), true),
        (
            double.clone(),
            json!({"value": 9_007_199_254_740_993_u64}),
            true,
        ),
        (double, json!({"value": 1.0}), false),
        (float.clone(), json!({"value": 0.1}), true),
        (float, json!({"value": 0.5}), false),
        (nested.clone(), json!({"value": {"field": [null, 1]}}), true),
        (nested, json!({"value": {"field": [null, 1.0]}}), false),
        (numeric_keys, json!({"value": {"1": 1, "01": 2}}), true),
        (integer.clone(), json!({"a": "1"}), true),
        (integer.clone(), json!({"a": true}), true),
        (integer, json!({"a": 1, "extra": 2}), true),
    ] {
        let app = Arc::new(App {
            enabled: true,
            ..App::default()
        });
        let model = ScriptedModel::new(vec![calls(tool.schema().name(), arguments), finishes()]);
        let result = Runner::run(request(tool, &model, &app)).await.unwrap();
        assert_eq!(
            matches!(result.outcome(), RunOutcome::Interrupted { .. }),
            interrupted
        );
        assert_eq!(
            app.approvals.load(Ordering::SeqCst),
            usize::from(!interrupted)
        );
        if !interrupted {
            assert_eq!(result.final_text(), "done");
        }
    }
}

static APPLICATION_DECODES: AtomicUsize = AtomicUsize::new(0);

#[derive(schemars::JsonSchema)]
struct ApplicationNumber(u32);

impl<'de> serde::Deserialize<'de> for ApplicationNumber {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> std::result::Result<Self, D::Error> {
        APPLICATION_DECODES.fetch_add(1, Ordering::SeqCst);
        let number = <u32 as serde::Deserialize>::deserialize(decoder)?;
        if number == 13 {
            return Err(serde::de::Error::custom(
                "application validation rejected the number",
            ));
        }
        Ok(Self(number))
    }
}

#[tool(needs_approval = approval)]
fn application_validated(a: ApplicationNumber) -> u32 {
    a.0
}

#[tokio::test]
async fn application_deserialization_runs_only_after_approval() {
    let app = Arc::new(App::default());
    let tool: Arc<dyn Tool> = Arc::new(application_validated::tool().unwrap());
    let model = ScriptedModel::new(vec![calls("application_validated", json!({"a": 13}))]);
    let paused = Runner::run(request(tool.clone(), &model, &app))
        .await
        .unwrap();
    let RunOutcome::Interrupted { items } = paused.outcome() else {
        panic!("application deserialization requires approval");
    };
    assert_eq!(APPLICATION_DECODES.load(Ordering::SeqCst), 0);
    assert_eq!(app.approvals.load(Ordering::SeqCst), 0);
    let mut state = paused.state().clone();
    state.approve(&items[0], false).unwrap();
    let model = ScriptedModel::new(vec![finishes()]);
    let result = Runner::run(request(tool, &model, &app).with_state(state))
        .await
        .unwrap();
    assert_eq!(APPLICATION_DECODES.load(Ordering::SeqCst), 1);
    assert_eq!(app.approvals.load(Ordering::SeqCst), 0);
    assert_eq!(
        output_text(result.new_items()),
        "An error occurred while running the tool. Please try again."
    );
}

#[tokio::test]
async fn failure_defaults_custom_handlers_and_propagation_use_common_dispatch() {
    let app = Arc::new(App::default());
    for (name, tool, expected) in [
        (
            "failing",
            Arc::new(failing::tool().unwrap()) as Arc<dyn Tool>,
            "An error occurred while running the tool. Please try again.",
        ),
        (
            "custom_error",
            Arc::new(custom_error::tool().unwrap()),
            "call-1:tool.execution_failed",
        ),
        (
            "panic_sync",
            Arc::new(panic_sync::tool().unwrap()),
            "An error occurred while running the tool. Please try again.",
        ),
    ] {
        let model = ScriptedModel::new(vec![calls(name, json!({})), finishes()]);
        let result = Runner::run(request(tool, &model, &app)).await.unwrap();
        assert_eq!(output_text(result.new_items()), expected);
    }
    let model = ScriptedModel::new(vec![calls("propagating", json!({}))]);
    let error = Runner::run(request(
        Arc::new(propagating::tool().unwrap()),
        &model,
        &app,
    ))
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("propagated"));
}

#[tokio::test]
async fn timeout_behavior_stays_under_runner_supervision() {
    let app = Arc::new(App::default());
    let model = ScriptedModel::new(vec![calls("slow", json!({})), finishes()]);
    let result = Runner::run(request(Arc::new(slow::tool().unwrap()), &model, &app))
        .await
        .unwrap();
    let output = result
        .new_items()
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => Some(output),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        ToolOutput::from_stored(output.output())
            .unwrap()
            .unwrap()
            .as_text(),
        Some("Tool 'slow' timed out after 0.01 seconds.")
    );
    let options = slow::tool().unwrap().options();
    assert_eq!(options.concurrency(), ToolConcurrency::Parallel);
    assert_eq!(options.failure_handling(), ToolFailureHandling::Custom);
    assert_eq!(options.max_no_progress_streak(), None);
    let model = ScriptedModel::new(vec![calls("slow_propagating", json!({}))]);
    let error = Runner::run(request(
        Arc::new(slow_propagating::tool().unwrap()),
        &model,
        &app,
    ))
    .await
    .err()
    .unwrap();
    assert_eq!(error.code(), "tool.timeout");
}

struct Check {
    id: ToolGuardrailId,
    seen: Arc<Mutex<Vec<&'static str>>>,
    reject: bool,
}
#[async_trait]
impl ToolInputGuardrail for Check {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }
    async fn check(&self, _: &ToolInputGuardrailData<'_>) -> Result<ToolGuardrailFunctionOutput> {
        self.seen.lock().unwrap().push("input");
        Ok(if self.reject {
            ToolGuardrailFunctionOutput::reject_content("blocked")
        } else {
            ToolGuardrailFunctionOutput::allow()
        })
    }
}
#[async_trait]
impl ToolOutputGuardrail for Check {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }
    async fn check(
        &self,
        data: &ToolOutputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        assert_eq!(data.output().as_text(), Some("hello"));
        self.seen.lock().unwrap().push("output");
        Ok(ToolGuardrailFunctionOutput::reject_content("filtered"))
    }
}
#[tokio::test]
async fn input_and_output_guardrails_apply_to_generated_functions() {
    for reject in [true, false] {
        let app = Arc::new(App::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let check = Arc::new(Check {
            id: ToolGuardrailId::new("check").unwrap(),
            seen: seen.clone(),
            reject,
        });
        let model = ScriptedModel::new(vec![
            calls("guarded", json!({"value": "hello"})),
            finishes(),
        ]);
        let config = RunConfig::new()
            .with_tool_input_guardrail(check.clone())
            .with_tool_output_guardrail(check);
        let result = Runner::run(
            request(Arc::new(guarded::tool().unwrap()), &model, &app).with_config(config),
        )
        .await
        .unwrap();
        assert_eq!(app.bodies.load(Ordering::SeqCst), usize::from(!reject));
        assert_eq!(
            *seen.lock().unwrap(),
            if reject {
                vec!["input"]
            } else {
                vec!["input", "output"]
            }
        );
        assert!(output_text(result.new_items()).contains(if reject {
            "blocked"
        } else {
            "filtered"
        }));
    }
}

#[tool(options = |options| options.with_timeout(Duration::from_millis(10)))]
fn sync_timeout() {}

/// Ports upstream's `timeout_seconds is only supported for async @function_tool handlers`: a
/// timeout cannot stop a running blocking body, so it would release the execution gate early.
#[test]
fn synchronous_function_timeouts_are_rejected_before_registration() {
    let error = sync_timeout::tool().err().unwrap();
    assert!(error.to_string().contains("only supported for async"));
    assert!(
        sync_timeout::tool_with_options(ToolOptions::new())
            .unwrap()
            .options()
            .timeout()
            .is_none()
    );
}

#[tokio::test]
async fn a_call_can_pass_dynamic_approval_and_rejected_calls_never_run() {
    let app = Arc::new(App {
        enabled: true,
        ..App::default()
    });
    let tool: Arc<dyn Tool> = Arc::new(controlled::tool().unwrap());
    let model = ScriptedModel::new(vec![calls("controlled", json!({"a": 1})), finishes()]);
    let result = Runner::run(request(tool.clone(), &model, &app))
        .await
        .unwrap();
    assert_eq!(output_text(result.new_items()), "1");
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
    let model = ScriptedModel::new(vec![calls("controlled", json!({"a": 7}))]);
    let result = Runner::run(request(tool.clone(), &model, &app))
        .await
        .unwrap();
    let RunOutcome::Interrupted { items } = result.outcome() else {
        panic!("approval required")
    };
    let mut state = result.state().clone();
    state.reject(&items[0], false).unwrap();
    let model = ScriptedModel::new(vec![finishes()]);
    let result = Runner::run(request(tool, &model, &app).with_state(state))
        .await
        .unwrap();
    assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
}

#[tool]
async fn parallel_probe(context: &ToolContext<'_>, value: u32) -> u32 {
    let app = context.run().app_context::<App>().unwrap();
    app.barrier.as_ref().unwrap().wait().await;
    app.bodies.fetch_add(1, Ordering::SeqCst);
    value
}

#[tool]
async fn retryable_failure(context: &ToolContext<'_>) -> Result<()> {
    context
        .run()
        .app_context::<App>()
        .unwrap()
        .bodies
        .fetch_add(1, Ordering::SeqCst);
    Err(Error::caller("still waiting"))
}

#[tokio::test]
async fn function_defaults_allow_parallel_calls_through_the_common_execution_gate() {
    let app = Arc::new(App {
        barrier: Some(Arc::new(tokio::sync::Barrier::new(2))),
        ..App::default()
    });
    let calls = (0..2)
        .map(|index| {
            RunItem::new(
                ItemId::new(format!("call-item-{index}")),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new(format!("call-{index}")),
                    "parallel_probe",
                    json!({"value": index}),
                )),
            )
        })
        .collect();
    let model = ScriptedModel::new(vec![ModelResponse::new(calls), finishes()]);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        Runner::run(request(
            Arc::new(parallel_probe::tool().unwrap()),
            &model,
            &app,
        )),
    )
    .await
    .expect("function calls must overlap")
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(app.bodies.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn function_defaults_do_not_install_a_product_no_progress_breaker() {
    let app = Arc::new(App::default());
    let mut script: Vec<_> = (0..5)
        .map(|index| {
            ModelResponse::new(vec![RunItem::new(
                ItemId::new(format!("call-item-{index}")),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new(format!("call-{index}")),
                    "retryable_failure",
                    json!({}),
                )),
            )])
        })
        .collect();
    script.push(finishes());
    let model = ScriptedModel::new(script);
    let result = Runner::run(request(
        Arc::new(retryable_failure::tool().unwrap()),
        &model,
        &app,
    ))
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(app.bodies.load(Ordering::SeqCst), 5);
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
struct InventoryOutput {
    sku: String,
    #[serde(rename = "availableUnits")]
    available_units: u32,
}

fn timeout_message(
    context: &ToolContext<'_>,
    error: &ra_core::tool::ToolTimeoutError,
) -> Result<ToolOutput> {
    assert_eq!(error.tool_name(), context.origin().name());
    assert_eq!(error.timeout_seconds(), 0.01);
    Ok(ToolOutput::text(format!(
        "custom_timeout:{}:{}",
        error.tool_name(),
        context.call_id()
    )))
}

#[tool(timeout = 0.01, timeout_error_function = sync(timeout_message))]
async fn custom_timeout() -> String {
    tokio::time::sleep(Duration::from_secs(1)).await;
    "late".to_owned()
}

async fn async_timeout_message(
    context: &ToolContext<'_>,
    error: &ra_core::tool::ToolTimeoutError,
) -> Result<ToolOutput> {
    timeout_message(context, error)
}

#[tool(timeout = 0.01, timeout_error_function = async_timeout_message)]
async fn async_custom_timeout() -> String {
    custom_timeout().await
}

#[tool(timeout = 0.01, timeout_behavior = "raise_exception", timeout_error_function = sync(timeout_message))]
async fn custom_timeout_propagating() -> String {
    custom_timeout().await
}

#[tool(timeout = 0.01, timeout_error_function = None, output_type = InventoryOutput)]
async fn typed_timeout() -> InventoryOutput {
    tokio::time::sleep(Duration::from_secs(1)).await;
    InventoryOutput {
        sku: "A".to_owned(),
        available_units: 2,
    }
}

#[tool(timeout = 0.01, timeout_error_function = sync(timeout_message), output_type = InventoryOutput)]
async fn invalid_timeout_output() -> InventoryOutput {
    typed_timeout().await
}

fn timeout_object(_: &ToolContext<'_>, _: &ra_core::tool::ToolTimeoutError) -> Result<ToolOutput> {
    Ok(ToolOutput::text(r#"{"sku":"ERROR","availableUnits":0}"#))
}

#[tool(timeout = 0.01, timeout_error_function = sync(timeout_object), output_type = InventoryOutput)]
async fn valid_timeout_output() -> InventoryOutput {
    typed_timeout().await
}

#[tool(timeout = 1, failure_error_function = None)]
async fn internal_timeout() -> Result<()> {
    Err(Error::tool(
        ToolErrorKind::ExecutionFailed,
        "internal_timeout",
        "tool_internal_timeout",
    )
    .with_source(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "tool_internal_timeout",
    )))
}

fn extract_data(
    context: &mut ra_core::tool::FunctionToolCustomDataContext<'_>,
) -> Result<Option<serde_json::Map<String, Value>>> {
    assert_eq!(context.tool().origin(), context.tool_context().origin());
    let data = json!({"call_id":context.raw_item().call_id().as_str(), "output":context.output().as_text(), "renderer":"chart"});
    *context.raw_item_mut() = context
        .raw_item()
        .clone()
        .with_output(json!("mutated clone"));
    Ok(Some(data.as_object().unwrap().clone()))
}

async fn extract_data_async(
    context: &mut ra_core::tool::FunctionToolCustomDataContext<'_>,
) -> Result<Option<serde_json::Map<String, Value>>> {
    extract_data(context)
}

#[tool(custom_data_extractor = sync(extract_data))]
fn with_data() -> &'static str {
    "tool_result"
}

#[tool(custom_data_extractor = extract_data_async, output_type = InventoryOutput)]
async fn with_typed_data() -> Value {
    json!({"sku":"A","availableUnits":2})
}

#[tool(custom_data_extractor = sync(extract_data))]
fn failing_with_data() -> Result<()> {
    Err(Error::caller("private failure"))
}

fn failed_extractor(
    _: &mut ra_core::tool::FunctionToolCustomDataContext<'_>,
) -> Result<Option<serde_json::Map<String, Value>>> {
    Err(Error::caller("extractor failed"))
}

#[tool(custom_data_extractor = sync(failed_extractor))]
fn bad_extraction(context: &RunContext) -> &'static str {
    context
        .app_context::<App>()
        .unwrap()
        .bodies
        .fetch_add(1, Ordering::SeqCst);
    "complete"
}

#[tool(output_type = InventoryOutput)]
async fn typed_failure() -> Result<InventoryOutput> {
    Err(Error::caller("private failure"))
}

#[tool(output_type = InventoryOutput, failure_error_function = custom_failure)]
fn invalid_error_output() -> Result<InventoryOutput> {
    Err(Error::caller("private failure"))
}

async fn object_failure(_: &ToolContext<'_>, _: &Error) -> Result<Option<ToolOutput>> {
    Ok(Some(ToolOutput::text(
        r#"{"sku":"ERROR","availableUnits":0}"#,
    )))
}

#[tool(output_type = InventoryOutput, failure_error_function = object_failure)]
fn valid_error_output() -> Result<InventoryOutput> {
    Err(Error::caller("private failure"))
}

#[tool(output_type = InventoryOutput, failure_error_function = None)]
fn invalid_typed_output() -> Value {
    json!({"sku":"A","availableUnits":"many"})
}

#[tool(output_type = InventoryOutput, custom_data_extractor = sync(extract_data), tool_output_guardrails = ["check"])]
fn guarded_typed_output() -> InventoryOutput {
    InventoryOutput {
        sku: "A".to_owned(),
        available_units: 2,
    }
}

#[tokio::test]
async fn timeout_formatters_support_sync_async_and_propagation() {
    for (tool, name) in [
        (
            Arc::new(custom_timeout::tool().unwrap()) as Arc<dyn Tool>,
            "custom_timeout",
        ),
        (
            Arc::new(async_custom_timeout::tool().unwrap()),
            "async_custom_timeout",
        ),
    ] {
        let model = ScriptedModel::new(vec![calls(name, json!({})), finishes()]);
        let result = Runner::run(request(tool, &model, &Arc::new(App::default())))
            .await
            .unwrap();
        assert_eq!(
            output_text(result.new_items()),
            format!("custom_timeout:{name}:call-1")
        );
    }
    let model = ScriptedModel::new(vec![calls("custom_timeout_propagating", json!({}))]);
    let error = Runner::run(request(
        Arc::new(custom_timeout_propagating::tool().unwrap()),
        &model,
        &Arc::new(App::default()),
    ))
    .await
    .err()
    .unwrap();
    assert_eq!(error.code(), "tool.timeout");
    assert!(
        std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<ra_core::tool::ToolTimeoutError>()
            .is_some()
    );
    let model = ScriptedModel::new(vec![calls("internal_timeout", json!({}))]);
    let error = Runner::run(request(
        Arc::new(internal_timeout::tool().unwrap()),
        &model,
        &Arc::new(App::default()),
    ))
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("tool_internal_timeout"));
    assert_ne!(error.code(), "tool.timeout");
}

#[tokio::test]
async fn typed_outputs_bypass_default_errors_but_validate_custom_formatters() {
    for (tool, name, expected) in [
        (
            Arc::new(typed_timeout::tool().unwrap()) as Arc<dyn Tool>,
            "typed_timeout",
            "Tool 'typed_timeout' timed out after 0.01 seconds.",
        ),
        (
            Arc::new(typed_failure::tool().unwrap()),
            "typed_failure",
            "An error occurred while running the tool. Please try again.",
        ),
        (
            Arc::new(valid_timeout_output::tool().unwrap()),
            "valid_timeout_output",
            r#"{"availableUnits":0,"sku":"ERROR"}"#,
        ),
        (
            Arc::new(valid_error_output::tool().unwrap()),
            "valid_error_output",
            r#"{"availableUnits":0,"sku":"ERROR"}"#,
        ),
    ] {
        let model = ScriptedModel::new(vec![calls(name, json!({})), finishes()]);
        let result = Runner::run(request(tool, &model, &Arc::new(App::default())))
            .await
            .unwrap();
        assert_eq!(output_text(result.new_items()), expected);
    }
    for (tool, name) in [
        (
            Arc::new(invalid_timeout_output::tool().unwrap()) as Arc<dyn Tool>,
            "invalid_timeout_output",
        ),
        (
            Arc::new(invalid_error_output::tool().unwrap()),
            "invalid_error_output",
        ),
        (
            Arc::new(invalid_typed_output::tool().unwrap()),
            "invalid_typed_output",
        ),
    ] {
        let model = ScriptedModel::new(vec![calls(name, json!({}))]);
        assert!(
            Runner::run(request(tool, &model, &Arc::new(App::default())))
                .await
                .is_err()
        );
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn custom_data_is_stored_without_replaying_it_or_mutating_raw_output() {
    for (tool, name) in [
        (
            Arc::new(with_data::tool().unwrap()) as Arc<dyn Tool>,
            "with_data",
        ),
        (
            Arc::new(with_typed_data::tool().unwrap()),
            "with_typed_data",
        ),
        (
            Arc::new(failing_with_data::tool().unwrap()),
            "failing_with_data",
        ),
    ] {
        let model = ScriptedModel::new(vec![calls(name, json!({})), finishes()]);
        let result = Runner::run(request(tool, &model, &Arc::new(App::default())))
            .await
            .unwrap();
        let output = result
            .new_items()
            .iter()
            .find_map(|item| match item.kind() {
                RunItemKind::ToolCallOutput(output) => Some(output),
                _ => None,
            })
            .unwrap();
        let data = output.custom_data().unwrap();
        assert_eq!(data["call_id"], "call-1");
        assert_eq!(data["renderer"], "chart");
        assert_eq!(data["output"], output_text(result.new_items()));
        assert!(!output.output().to_string().contains("mutated clone"));
        let restored: RunState =
            serde_json::from_value(serde_json::to_value(result.state()).unwrap()).unwrap();
        assert!(
            serde_json::to_value(restored)
                .unwrap()
                .to_string()
                .contains("\"custom_data\"")
        );
        let requests = model.requests.lock().unwrap();
        let replay = requests[1]
            .input()
            .iter()
            .find_map(|item| match item {
                ModelInputItem::ToolCallOutput(output) => Some(output),
                _ => None,
            })
            .unwrap();
        assert!(replay.custom_data().is_none());
        assert!(
            !serde_json::to_value(requests[1].input())
                .unwrap()
                .to_string()
                .contains("chart")
        );
    }
    let model = ScriptedModel::new(vec![calls("bad_extraction", json!({}))]);
    let app = Arc::new(App::default());
    let error = Runner::run(request(
        Arc::new(bad_extraction::tool().unwrap()),
        &model,
        &app,
    ))
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("extractor failed"));
    let state: RunState =
        serde_json::from_value(serde_json::to_value(error.run_state().unwrap()).unwrap()).unwrap();
    assert_eq!(output_text(state.generated_items()), "complete");
    let model = ScriptedModel::new(vec![finishes()]);
    Runner::run(request(Arc::new(bad_extraction::tool().unwrap()), &model, &app).with_state(state))
        .await
        .unwrap();
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
}

struct RejectStructuredOutput {
    id: ToolGuardrailId,
}
#[async_trait]
impl ToolOutputGuardrail for RejectStructuredOutput {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }
    async fn check(
        &self,
        data: &ToolOutputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        assert!(data.output().as_text().unwrap().contains("availableUnits"));
        Ok(ToolGuardrailFunctionOutput::reject_content("blocked"))
    }
}

#[tokio::test]
async fn guardrail_rejection_bypasses_output_schema_and_extractor_sees_the_replacement() {
    let model = ScriptedModel::new(vec![calls("guarded_typed_output", json!({})), finishes()]);
    let check = Arc::new(RejectStructuredOutput {
        id: ToolGuardrailId::new("check").unwrap(),
    });
    let result = Runner::run(
        request(
            Arc::new(guarded_typed_output::tool().unwrap()),
            &model,
            &Arc::new(App::default()),
        )
        .with_config(RunConfig::new().with_tool_output_guardrail(check)),
    )
    .await
    .unwrap();
    assert_eq!(output_text(result.new_items()), "blocked");
    let output = result
        .new_items()
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) => Some(output),
            _ => None,
        })
        .unwrap();
    assert_eq!(output.custom_data().unwrap()["output"], "blocked");
}

#[tokio::test]
async fn programmatic_schema_errors_propagate_and_default_timeout_is_an_error_object() {
    use ra_core::tool::ToolCaller;
    use ra_runtime::tool::dispatch::{
        CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool,
    };
    for (tool, timeout) in [
        (
            Arc::new(typed_failure::tool().unwrap()) as Arc<dyn Tool>,
            false,
        ),
        (Arc::new(typed_timeout::tool().unwrap()), true),
    ] {
        let tool: Arc<dyn Tool> = if timeout {
            Arc::new(
                typed_timeout::tool_with_options(
                    typed_timeout::options()
                        .unwrap()
                        .with_allowed_callers([ToolCaller::Programmatic]),
                )
                .unwrap(),
            )
        } else {
            tool
        };
        let agent = AgentSpec::builder()
            .id(AgentId::new("test"))
            .name("Test")
            .build()
            .unwrap();
        let run = Arc::new(RunContext::new(RunId::new("programmatic-run"), &agent));
        let result = dispatch_tool(
            ToolDispatchRequest::new(
                tool,
                CallId::new("call"),
                json!({}),
                run,
                CancelScope::root(),
                CallHistory::default(),
                Default::default(),
            )
            .with_caller(ToolCaller::Programmatic),
        )
        .await;
        if timeout {
            let ToolDispatch::Observed(observation) = result.unwrap().into_parts().0 else {
                panic!("observed timeout")
            };
            let text = ToolOutput::from_stored(observation.output().output())
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(text.as_text().unwrap()).unwrap(),
                json!({"error":"Tool 'typed_timeout' timed out after 0.01 seconds."})
            );
        } else {
            assert!(result.is_err());
        }
    }
}

#[tool(output_type = InventoryOutput, tool_input_guardrails = ["check"])]
fn rejected_programmatic_input() -> InventoryOutput {
    panic!("the input guardrail must prevent invocation")
}

#[tokio::test]
async fn programmatic_guardrail_rejections_bypass_the_schema_as_error_objects() {
    use ra_core::tool::ToolCaller;
    use ra_runtime::tool::{
        dispatch::{CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool},
        guardrail::ToolGuardrails,
    };
    for input in [true, false] {
        let tool: Arc<dyn Tool> = if input {
            Arc::new(rejected_programmatic_input::tool().unwrap())
        } else {
            Arc::new(guarded_typed_output::tool().unwrap())
        };
        let guards = if input {
            ToolGuardrails::install(
                [Arc::new(Check {
                    id: ToolGuardrailId::new("check").unwrap(),
                    seen: Arc::new(Mutex::new(Vec::new())),
                    reject: true,
                }) as Arc<dyn ToolInputGuardrail>],
                [],
                false,
            )
            .unwrap()
        } else {
            ToolGuardrails::install(
                [],
                [Arc::new(RejectStructuredOutput {
                    id: ToolGuardrailId::new("check").unwrap(),
                }) as Arc<dyn ToolOutputGuardrail>],
                false,
            )
            .unwrap()
        };
        let agent = AgentSpec::builder()
            .id(AgentId::new("test"))
            .name("Test")
            .build()
            .unwrap();
        let run = Arc::new(RunContext::new(
            RunId::new("programmatic-guardrail-run"),
            &agent,
        ));
        let outcome = dispatch_tool(
            ToolDispatchRequest::new(
                tool,
                CallId::new("call"),
                json!({}),
                run,
                CancelScope::root(),
                CallHistory::default(),
                Default::default(),
            )
            .with_caller(ToolCaller::Programmatic)
            .with_tool_guardrails(guards),
        )
        .await
        .unwrap();
        let output = match outcome.into_parts().0 {
            ToolDispatch::Refused(refusal) => {
                assert!(input);
                refusal.into_output()
            }
            ToolDispatch::Observed(observation) => {
                assert!(!input);
                observation.into_output()
            }
            _ => panic!("guardrail output"),
        };
        assert!(output.is_error());
        let output = ToolOutput::from_stored(output.output()).unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(output.as_text().unwrap()).unwrap(),
            json!({"error":"blocked"})
        );
    }
}

fn failing_after_approval(
    context: &mut ra_core::tool::FunctionToolCustomDataContext<'_>,
) -> Result<Option<serde_json::Map<String, Value>>> {
    context
        .tool_context()
        .run()
        .app_context::<App>()
        .unwrap()
        .extractions
        .fetch_add(1, Ordering::SeqCst);
    Err(Error::caller("extractor failed after approval"))
}

#[tool(needs_approval = true, custom_data_extractor = sync(failing_after_approval))]
fn approved_charge(context: &RunContext) -> &'static str {
    context
        .app_context::<App>()
        .unwrap()
        .bodies
        .fetch_add(1, Ordering::SeqCst);
    "receipt"
}

#[tokio::test]
async fn approved_output_is_committed_before_a_failed_extraction() {
    use ra_core::agent::ToolUseBehavior;
    for stop in [false, true] {
        let app = Arc::new(App::default());
        let tool: Arc<dyn Tool> = Arc::new(approved_charge::tool().unwrap());
        let request = |model: &Arc<ScriptedModel>| {
            let agent = AgentSpec::builder()
                .id(AgentId::new("function-agent"))
                .name("Functions")
                .tool(tool.clone())
                .tool_use_behavior(if stop {
                    ToolUseBehavior::StopOnFirstTool
                } else {
                    ToolUseBehavior::RunLlmAgain
                })
                .build()
                .unwrap();
            RunRequest::new(
                AgentBinding::direct(agent),
                Arc::new(Resolver(model.clone())),
                RunId::new("function-run"),
                CancelScope::root(),
                vec![],
            )
            .with_app_context(app.clone())
        };
        let model = ScriptedModel::new(vec![calls("approved_charge", json!({}))]);
        let paused = Runner::run(request(&model)).await.unwrap();
        let RunOutcome::Interrupted { items } = paused.outcome() else {
            panic!("approval required")
        };
        let mut state: RunState =
            serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
        state.approve(&items[0], false).unwrap();

        let error = Runner::run(request(&ScriptedModel::new(vec![])).with_state(state))
            .await
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("extractor failed after approval")
        );
        let state: RunState =
            serde_json::from_value(serde_json::to_value(error.run_state().unwrap()).unwrap())
                .unwrap();
        assert_eq!(output_text(state.generated_items()), "receipt");

        // The retry finalizes from the committed output: neither the approved callable nor its
        // extractor runs again, and the model is shown the output exactly once.
        let model = ScriptedModel::new(vec![finishes()]);
        let result = Runner::run(request(&model).with_state(state))
            .await
            .unwrap();
        assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
        assert_eq!(app.extractions.load(Ordering::SeqCst), 1);
        if stop {
            assert_eq!(result.final_output().unwrap().text_content(), "receipt");
            assert!(model.requests.lock().unwrap().is_empty());
        } else {
            assert_eq!(result.final_text(), "done");
            let requests = model.requests.lock().unwrap();
            let outputs = requests[0]
                .input()
                .iter()
                .filter(|item| {
                    matches!(item, ModelInputItem::ToolCallOutput(output)
                        if output.call_id() == &CallId::new("call-1"))
                })
                .count();
            assert_eq!(outputs, 1);
        }
    }
}

#[tool(needs_approval = true)]
fn approved_receipt(context: &RunContext) -> &'static str {
    context
        .app_context::<App>()
        .unwrap()
        .bodies
        .fetch_add(1, Ordering::SeqCst);
    "receipt"
}

fn receipt_data(
    context: &mut ra_core::tool::FunctionToolCustomDataContext<'_>,
) -> Result<Option<serde_json::Map<String, Value>>> {
    context
        .tool_context()
        .run()
        .app_context::<App>()
        .unwrap()
        .extractions
        .fetch_add(1, Ordering::SeqCst);
    Ok(Some(json!({"receipt": "r-7"}).as_object().unwrap().clone()))
}

#[tool(needs_approval = true, custom_data_extractor = sync(receipt_data))]
fn approved_receipt_with_data(context: &RunContext) -> &'static str {
    context
        .app_context::<App>()
        .unwrap()
        .bodies
        .fetch_add(1, Ordering::SeqCst);
    "receipt"
}

/// An approval-gated tool whose body always fails and whose failures the model is shown as the
/// framework's stable failure object.
struct ApprovedFailure;

#[async_trait]
impl Tool for ApprovedFailure {
    fn origin(&self) -> &ra_core::tool::ToolOrigin {
        static ORIGIN: std::sync::LazyLock<ra_core::tool::ToolOrigin> =
            std::sync::LazyLock::new(|| {
                ra_core::tool::ToolOrigin::new("approved_failure").unwrap()
            });
        &ORIGIN
    }

    fn schema(&self) -> &ra_core::tool::ToolSchema {
        static SCHEMA: std::sync::LazyLock<ra_core::tool::ToolSchema> =
            std::sync::LazyLock::new(|| {
                ra_core::tool::ToolSchema::loose("approved_failure", json!({"type": "object"}))
                    .unwrap()
            });
        &SCHEMA
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ra_core::tool::ToolApprovalPolicy::Always)
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        context
            .run()
            .app_context::<App>()
            .unwrap()
            .bodies
            .fetch_add(1, Ordering::SeqCst);
        Err(Error::tool(
            ToolErrorKind::ExecutionFailed,
            "approved_failure",
            "the charge was declined",
        ))
    }
}

/// Narration whose tool end always fails, as a host whose collector is down.
struct FailingToolEnd;

#[async_trait]
impl ra_core::lifecycle::LifecycleHook for FailingToolEnd {
    fn name(&self) -> &str {
        "failing-end"
    }

    async fn on_tool_end(
        &self,
        _scope: ra_core::lifecycle::LifecycleScope,
        _input: &ra_core::lifecycle::ToolEndInput<'_>,
    ) -> Result<()> {
        Err(Error::caller("the end callback failed"))
    }
}

/// Approves the one call `tool` was asked for, resumes it under a tool end that fails, and returns
/// the checkpoint that failure left.
async fn approved_then_end_fails(tool: Arc<dyn Tool>, name: &str, app: &Arc<App>) -> RunState {
    let model = ScriptedModel::new(vec![calls(name, json!({}))]);
    let paused = Runner::run(request(tool.clone(), &model, app))
        .await
        .unwrap();
    let RunOutcome::Interrupted { items } = paused.outcome() else {
        panic!("approval required")
    };
    let mut state: RunState =
        serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
    state.approve(&items[0], false).unwrap();

    let config = RunConfig::new().with_lifecycle_hook(Arc::new(FailingToolEnd));
    let error = Runner::run(
        request(tool, &ScriptedModel::new(vec![]), app)
            .with_config(config)
            .with_state(state),
    )
    .await
    .err()
    .unwrap();
    assert!(
        error.to_string().contains("the end callback failed"),
        "{error}"
    );
    serde_json::from_value(serde_json::to_value(error.run_state().unwrap()).unwrap()).unwrap()
}

/// The one answer to `call-1` among `items`.
fn the_answer(items: &[RunItem]) -> ra_core::item::ToolCallOutput {
    let answers: Vec<_> = items
        .iter()
        .filter_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) if output.call_id() == &CallId::new("call-1") => {
                Some(output.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(answers.len(), 1, "{answers:?}");
    answers.into_iter().next().unwrap()
}

/// Resumes `state` without the failing tool end and returns how many answers to `call-1` the model
/// was shown.
async fn retried(tool: Arc<dyn Tool>, state: RunState, app: &Arc<App>) -> usize {
    let model = ScriptedModel::new(vec![finishes()]);
    let result = Runner::run(request(tool, &model, app).with_state(state))
        .await
        .unwrap();
    assert_eq!(result.final_text(), "done");
    let requests = model.requests.lock().unwrap();
    requests[0]
        .input()
        .iter()
        .filter(|item| {
            matches!(item, ModelInputItem::ToolCallOutput(output)
                if output.call_id() == &CallId::new("call-1"))
        })
        .count()
}

/// `on_tool_end` runs after the approved call's output is committed, so its failure leaves that
/// output in the checkpoint and the retry finalizes from it without running the tool again, as in
/// the reference's committed-output retry after a failing end hook.
#[tokio::test]
async fn approved_output_is_committed_before_a_failed_tool_end() {
    let app = Arc::new(App::default());
    let tool: Arc<dyn Tool> = Arc::new(approved_receipt::tool().unwrap());
    let state = approved_then_end_fails(tool.clone(), "approved_receipt", &app).await;
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
    assert_eq!(output_text(state.generated_items()), "receipt");

    assert_eq!(retried(tool, state, &app).await, 1);
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
}

/// The committed answer carries what the extractor added to it, as the reference sets custom data
/// on the item it committed: a run that fails in `on_tool_end` resumes with that data and does not
/// run the extractor again.
#[tokio::test]
async fn the_committed_answer_keeps_its_custom_data_when_the_tool_end_fails() {
    let app = Arc::new(App::default());
    let tool: Arc<dyn Tool> = Arc::new(approved_receipt_with_data::tool().unwrap());
    let state = approved_then_end_fails(tool.clone(), "approved_receipt_with_data", &app).await;
    assert_eq!(app.extractions.load(Ordering::SeqCst), 1);
    let committed = the_answer(state.generated_items());
    assert_eq!(
        committed.custom_data().and_then(|data| data.get("receipt")),
        Some(&json!("r-7"))
    );

    let model = ScriptedModel::new(vec![finishes()]);
    let result = Runner::run(request(tool, &model, &app).with_state(state))
        .await
        .unwrap();
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
    assert_eq!(app.extractions.load(Ordering::SeqCst), 1);
    assert_eq!(
        the_answer(result.state().generated_items())
            .custom_data()
            .and_then(|data| data.get("receipt")),
        Some(&json!("r-7"))
    );
}

/// A failure the model is shown as the framework's failure object is committed like any answer, so
/// a failing `on_tool_end` does not run the approved tool a second time.
#[tokio::test]
async fn an_approved_model_visible_failure_is_committed_before_a_failed_tool_end() {
    let app = Arc::new(App::default());
    let tool: Arc<dyn Tool> = Arc::new(ApprovedFailure);
    let state = approved_then_end_fails(tool.clone(), "approved_failure", &app).await;
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
    let committed = the_answer(state.generated_items());
    assert!(committed.is_error());
    assert_eq!(
        committed.output(),
        &json!({"error": {"code": "tool.execution_failed", "tool": "approved_failure"}})
    );

    assert_eq!(retried(tool, state, &app).await, 1);
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
}

/// A custom (free-form input) tool that echoes what it was sent.
struct CustomEcho;

#[async_trait]
impl Tool for CustomEcho {
    fn origin(&self) -> &ra_core::tool::ToolOrigin {
        static ORIGIN: std::sync::LazyLock<ra_core::tool::ToolOrigin> =
            std::sync::LazyLock::new(|| ra_core::tool::ToolOrigin::new("custom_echo").unwrap());
        &ORIGIN
    }

    fn schema(&self) -> &ra_core::tool::ToolSchema {
        static SCHEMA: std::sync::LazyLock<ra_core::tool::ToolSchema> =
            std::sync::LazyLock::new(|| {
                ra_core::tool::ToolSchema::custom("custom_echo", None).unwrap()
            });
        &SCHEMA
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("echoed"))
    }
}

/// Narration that keeps the answer every tool end was told about.
#[derive(Default)]
struct EndAnswers(Mutex<Vec<ra_core::item::ToolCallOutput>>, AtomicUsize);

#[async_trait]
impl ra_core::lifecycle::LifecycleHook for EndAnswers {
    fn name(&self) -> &str {
        "end-answers"
    }

    async fn on_tool_start(
        &self,
        _scope: ra_core::lifecycle::LifecycleScope,
        _input: &ra_core::lifecycle::ToolStartInput<'_>,
    ) -> Result<()> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn on_tool_end(
        &self,
        _scope: ra_core::lifecycle::LifecycleScope,
        input: &ra_core::lifecycle::ToolEndInput<'_>,
    ) -> Result<()> {
        self.0.lock().unwrap().push(input.output().clone());
        Ok(())
    }
}

/// The end is told the answer exactly as history records it, including the kind of call it answers.
#[tokio::test]
async fn a_custom_call_ends_with_the_answer_history_records() {
    let app = Arc::new(App::default());
    let answers = Arc::new(EndAnswers::default());
    let model = ScriptedModel::new(vec![
        ModelResponse::new(vec![RunItem::new(
            ItemId::new("call-item"),
            RunItemKind::ToolCall(ToolCall::custom(
                CallId::new("call-1"),
                "custom_echo",
                "raw input",
            )),
        )]),
        finishes(),
    ]);
    let config = RunConfig::new().with_lifecycle_hook(answers.clone());
    let result = Runner::run(request(Arc::new(CustomEcho), &model, &app).with_config(config))
        .await
        .unwrap();
    assert_eq!(result.final_text(), "done");
    let recorded = the_answer(result.new_items());
    assert_eq!(recorded.kind(), ra_core::item::ToolCallKind::Custom);
    assert_eq!(*answers.0.lock().unwrap(), [recorded]);
}

struct CancelOnReturn(CancelScope);

#[tool]
async fn cancels_after_completing(context: &RunContext) -> &'static str {
    context
        .app_context::<CancelOnReturn>()
        .unwrap()
        .0
        .cancel(ra_core::cancel::CancelReason::UserInterrupt);
    "completed"
}

#[tokio::test]
async fn a_tool_without_an_extractor_keeps_its_completed_output_when_cancelled_afterwards() {
    use ra_runtime::tool::dispatch::{
        CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool,
    };
    let scope = CancelScope::root();
    let agent = AgentSpec::builder()
        .id(AgentId::new("test"))
        .name("Test")
        .build()
        .unwrap();
    let run = Arc::new(
        RunContext::new(RunId::new("cancel-after-run"), &agent)
            .with_app_context(Arc::new(CancelOnReturn(scope.clone()))),
    );
    let outcome = dispatch_tool(ToolDispatchRequest::new(
        Arc::new(cancels_after_completing::tool().unwrap()),
        CallId::new("call"),
        json!({}),
        run,
        scope.clone(),
        CallHistory::default(),
        Default::default(),
    ))
    .await
    .unwrap();
    assert!(scope.is_cancelled());
    let ToolDispatch::Observed(observation) = outcome.into_parts().0 else {
        panic!("the completed output is observed")
    };
    assert_eq!(
        ToolOutput::from_stored(observation.output().output())
            .unwrap()
            .unwrap()
            .as_text(),
        Some("completed")
    );
}

#[tool(output_type = InventoryOutput)]
async fn typed_lookup(sku: String) -> InventoryOutput {
    InventoryOutput {
        sku,
        available_units: 1,
    }
}

#[tokio::test]
async fn direct_argument_errors_keep_the_default_formatter_for_schema_backed_tools() {
    let model = ScriptedModel::new(vec![calls("typed_lookup", json!({"sku": 7})), finishes()]);
    let result = Runner::run(request(
        Arc::new(typed_lookup::tool().unwrap()),
        &model,
        &Arc::new(App::default()),
    ))
    .await
    .unwrap();
    assert_eq!(
        output_text(result.new_items()),
        "An error occurred while running the tool. Please try again."
    );
    assert_eq!(result.final_text(), "done");
}

#[tool]
async fn slow_sibling(context: &RunContext) -> &'static str {
    tokio::time::sleep(Duration::from_millis(300)).await;
    context
        .app_context::<App>()
        .unwrap()
        .bodies
        .fetch_add(1, Ordering::SeqCst);
    "slept"
}

#[tokio::test]
async fn a_fresh_turn_with_an_unanswered_sibling_leaves_no_resumable_checkpoint() {
    let app = Arc::new(App::default());
    let agent = AgentSpec::builder()
        .id(AgentId::new("function-agent"))
        .name("Functions")
        .tool(Arc::new(bad_extraction::tool().unwrap()))
        .tool(Arc::new(slow_sibling::tool().unwrap()))
        .build()
        .unwrap();
    let call = |id: &str, call_id: &str, name: &str| {
        RunItem::new(
            ItemId::new(id),
            RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, json!({}))),
        )
    };
    let model = ScriptedModel::new(vec![ModelResponse::new(vec![
        call("c1", "call-1", "bad_extraction"),
        call("c2", "call-2", "slow_sibling"),
    ])]);
    let error = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(Resolver(model)),
            RunId::new("function-run"),
            CancelScope::root(),
            vec![],
        )
        .with_app_context(app.clone()),
    )
    .await
    .err()
    .unwrap();
    // The extractor failure cancelled the sibling before it produced an output. As in the
    // reference, the failed fresh turn returns its error without a checkpoint that would replay
    // `call-2` unanswered.
    assert!(error.to_string().contains("extractor failed"));
    assert!(error.run_state().is_none());
    assert_eq!(app.bodies.load(Ordering::SeqCst), 1);
}
