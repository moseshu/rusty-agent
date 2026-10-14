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
    assert!(output.output().to_string().contains("tool.timeout"));
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
