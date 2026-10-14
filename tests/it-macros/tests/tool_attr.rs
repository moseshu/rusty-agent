//! Function decorator contracts, including use through a renamed core dependency.

use std::{sync::Arc, time::Duration};

use core_contract::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::Result,
    item::CallId,
    state::RunId,
    tool::{
        Tool, ToolAvailability, ToolCaller, ToolConcurrency, ToolContext, ToolExposure,
        ToolFailureHandling, ToolGuardrailId, ToolOptions, ToolOutput, ToolTimeoutBehavior,
    },
};
use ra_macros::tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Adds two integers.
#[tool]
fn add(#[tool(description = "First operand.")] a: i64, #[tool(default = 5)] b: i64) -> i64 {
    a + b
}

#[tool]
async fn no_args() -> &'static str {
    "ok"
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct Filter {
    labels: Vec<String>,
    optional: Option<u32>,
}

#[tool(name = "search_catalog", description = "Explicit description.")]
/// This description is overridden.
pub(crate) async fn search(filter: Filter, limit: Option<u32>) -> Result<Value> {
    Ok(json!({"labels": filter.labels, "limit": limit}))
}

#[tool(strict = false)]
fn loose(
    value: Option<String>,
    #[tool(default = None)] fallback: Option<String>,
) -> Option<String> {
    value.or(fallback)
}

#[tool(use_docstring_info = false)]
/// Omitted from the model description.
fn without_docs(#[doc = "Omitted parameter docs."] value: i32) -> i32 {
    value
}

#[tool]
fn sync_context(context: ToolContext<'_>, suffix: String) -> String {
    assert_eq!(context.resume_state(), Some(&json!({"resume": true})));
    assert_eq!(context.arguments()["suffix"], "tail");
    format!(
        "{}:{}:{}",
        context.run().app_context::<String>().unwrap(),
        context.call_id().as_str(),
        suffix
    )
}

#[tool]
async fn run_context(context: &RunContext) -> String {
    context.run_id().to_string()
}

#[tool]
async fn borrowed_context(context: &ToolContext<'_>) -> String {
    context.call_id().to_string()
}

#[tool]
fn native_output() -> ToolOutput {
    ToolOutput::text("native").with_metadata(Default::default())
}

#[tool]
fn blocking(context: &RunContext) -> bool {
    context
        .app_context::<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>()
        .unwrap()
        .lock()
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .is_ok()
}

#[tool(is_enabled = false, needs_approval = true)]
fn disabled() {}

#[tool(
    timeout = 0.5,
    timeout_behavior = "raise_exception",
    tool_input_guardrails = ["check"],
    defer_loading = true,
    allowed_callers = ["direct"]
)]
async fn configured() -> i32 {
    2
}

#[tool(
    options = |options| options.with_concurrency(ToolConcurrency::Exclusive),
    timeout = Duration::from_millis(30)
)]
async fn adjusted() -> i32 {
    3
}

#[tool]
fn synchronous() -> i32 {
    4
}

#[tool]
fn r#type(r#match: String) -> String {
    r#match
}

async fn invoke(tool: &dyn Tool, args: Value) -> Result<ToolOutput> {
    let agent = AgentSpec::builder()
        .id(AgentId::new("test"))
        .name("Test")
        .build()?;
    let run = RunContext::new(RunId::new("test-run"), &agent);
    let call = CallId::new("test-call");
    tool.call(ToolContext::new(&run, tool, &call, &args)).await
}

#[test]
fn metadata_defaults_and_original_function_are_preserved() {
    assert_eq!(add(3, 7), 10);
    let tool = add::tool().unwrap();
    tool.validate().unwrap();
    assert_eq!(tool.origin().name(), "add");
    assert_eq!(tool.schema().description(), Some("Adds two integers."));
    let schema = tool.schema().input_schema();
    assert_eq!(schema["required"], json!(["a", "b"]));
    assert_eq!(schema["properties"]["a"]["description"], "First operand.");
    assert_eq!(schema["properties"]["b"]["default"], 5);
    assert_eq!(schema["additionalProperties"], false);
    for _ in 0..100 {
        assert_eq!(
            add::tool().unwrap().schema().canonical_json().unwrap(),
            tool.schema().canonical_json().unwrap()
        );
    }
    assert_eq!(search::tool().unwrap().schema().name(), "search_catalog");
    assert_eq!(
        search::tool().unwrap().schema().description(),
        Some("Explicit description.")
    );
}

#[tokio::test]
async fn explicit_defaults_work_in_strict_mode_and_extra_fields_are_ignored() {
    let tool = add::tool().unwrap();
    assert_eq!(
        invoke(&tool, json!({"a": 1})).await.unwrap().as_text(),
        Some("6")
    );
    assert_eq!(
        invoke(&tool, json!({"a": 1, "b": 2, "extra": true}))
            .await
            .unwrap()
            .as_text(),
        Some("3")
    );
    assert!(invoke(&tool, json!({})).await.is_err());
    assert!(invoke(&tool, json!({"a": null})).await.is_err());
    assert!(invoke(&tool, json!({"a": "bad"})).await.is_err());
    assert!(invoke(&tool, json!([])).await.is_err());
}

#[tokio::test]
async fn nullable_without_default_is_required_even_in_loose_mode() {
    let tool = loose::tool().unwrap();
    assert!(!tool.schema().strict_json_schema());
    assert_eq!(tool.schema().input_schema()["required"], json!(["value"]));
    assert!(invoke(&tool, json!({})).await.is_err());
    assert_eq!(
        invoke(&tool, json!({"value": null}))
            .await
            .unwrap()
            .as_text(),
        Some("null")
    );
    assert_eq!(
        invoke(&tool, json!({"value": null, "fallback": "hello"}))
            .await
            .unwrap()
            .as_text(),
        Some("hello")
    );
}

#[tokio::test]
async fn nested_values_nullable_and_return_conversion_follow_the_callable() {
    let tool = search::tool().unwrap();
    assert!(tool.schema().strict_json_schema());
    assert_eq!(
        tool.schema().input_schema()["required"],
        json!(["filter", "limit"])
    );
    let value = invoke(
        &tool,
        json!({"filter": {"labels": ["sdk"], "optional": null, "extra": true}, "limit": null}),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(value.as_text().unwrap()).unwrap(),
        json!({"labels": ["sdk"], "limit": null})
    );
    assert!(
        invoke(&tool, json!({"filter": {"labels": [3]}, "limit": null}))
            .await
            .is_err()
    );
    assert!(
        invoke(&tool, json!({"filter": {"labels": []}}))
            .await
            .is_err()
    );
    assert_eq!(
        invoke(&native_output::tool().unwrap(), json!({}))
            .await
            .unwrap()
            .as_text(),
        Some("native")
    );
    assert_eq!(
        invoke(&r#type::tool().unwrap(), json!({"match": "value"}))
            .await
            .unwrap()
            .as_text(),
        Some("value")
    );
}

#[tokio::test]
async fn no_argument_and_context_functions_work_directly_and_as_tools() {
    assert_eq!(no_args().await, "ok");
    assert_eq!(
        invoke(&no_args::tool().unwrap(), json!({"ignored": true}))
            .await
            .unwrap()
            .as_text(),
        Some("ok")
    );
    let tool = sync_context::tool().unwrap();
    assert_eq!(tool.schema().input_schema()["required"], json!(["suffix"]));
    assert!(tool.func_schema().unwrap().inject_tool_context());
    let agent = AgentSpec::builder()
        .id(AgentId::new("test"))
        .name("Test")
        .build()
        .unwrap();
    let run =
        RunContext::new(RunId::new("owned"), &agent).with_app_context(Arc::new("app".to_owned()));
    let call = CallId::new("owned-call");
    let args = json!({"suffix": "tail"});
    let resume = json!({"resume": true});
    let output = tool
        .call(ToolContext::new(&run, &tool, &call, &args).with_resume_state(&resume))
        .await
        .unwrap();
    assert_eq!(output.as_text(), Some("app:owned-call:tail"));
    assert_eq!(
        invoke(&run_context::tool().unwrap(), json!({}))
            .await
            .unwrap()
            .as_text(),
        Some("test-run")
    );
    assert_eq!(
        invoke(&borrowed_context::tool().unwrap(), json!({}))
            .await
            .unwrap()
            .as_text(),
        Some("test-call")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_bodies_do_not_block_the_async_executor() {
    let tool = blocking::tool().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let agent = AgentSpec::builder()
        .id(AgentId::new("thread"))
        .name("Thread")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("thread"), &agent)
        .with_app_context(Arc::new(std::sync::Mutex::new(receiver)));
    let call = CallId::new("blocking");
    let args = json!({});
    let body = tool.call(ToolContext::new(&run, &tool, &call, &args));
    let heartbeat = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        sender.send(()).unwrap();
    };
    let (output, ()) = tokio::join!(body, heartbeat);
    assert_eq!(output.unwrap().as_text(), Some("true"));
}

#[test]
fn docs_and_instance_policies_can_be_selected_explicitly() {
    let tool = without_docs::tool().unwrap();
    assert_eq!(tool.schema().description(), Some(""));
    assert!(
        tool.schema().input_schema()["properties"]["value"]
            .get("description")
            .is_none()
    );
    assert_eq!(
        disabled::tool().unwrap().options().availability(),
        ToolAvailability::Disabled
    );
    assert_eq!(
        configured::tool().unwrap().options().timeout(),
        Some(Duration::from_millis(500))
    );
    assert_eq!(
        configured::tool_with_options(ToolOptions::new())
            .unwrap()
            .options()
            .timeout(),
        None
    );
}

/// Mirrors `function_tool(timeout=...)`: setting one upstream keyword keeps the other defaults.
#[tokio::test]
async fn upstream_execution_keywords_keep_function_tool_defaults() {
    let options = configured::tool().unwrap().options();
    assert_eq!(options.timeout_behavior(), ToolTimeoutBehavior::Propagate);
    assert_eq!(
        options.input_guardrails(),
        [ToolGuardrailId::new("check").unwrap()]
    );
    assert_eq!(options.exposure(), ToolExposure::Deferred);
    assert_eq!(options.allowed_callers(), Some(&[ToolCaller::Direct][..]));
    assert_eq!(options.concurrency(), ToolConcurrency::Parallel);
    assert_eq!(options.failure_handling(), ToolFailureHandling::Custom);
    assert_eq!(options.max_no_progress_streak(), None);

    assert_eq!(adjusted().await, 3);
    let options = adjusted::options().unwrap();
    assert_eq!(options.concurrency(), ToolConcurrency::Exclusive);
    assert_eq!(options.timeout(), Some(Duration::from_millis(30)));
    assert_eq!(options.failure_handling(), ToolFailureHandling::Custom);
    assert_eq!(options.max_no_progress_streak(), None);
}

/// Ports upstream's rejection of `timeout_seconds` on synchronous handlers.
#[test]
fn synchronous_functions_reject_timeouts() {
    assert!(synchronous::tool().is_ok());
    let error = synchronous::tool_with_options(
        synchronous::options()
            .unwrap()
            .with_timeout(Duration::from_secs(1)),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("only supported for async"));
}

fn enabled_sync(context: &RunContext) -> Result<bool> {
    Ok(context.run_id().as_str() == "callbacks")
}
fn approval_sync(context: &ToolContext<'_>) -> Result<bool> {
    Ok(context.call_id().as_str() == "approve")
}
fn failure_sync(
    _: &ToolContext<'_>,
    _: &core_contract::error::Error,
) -> Result<Option<ToolOutput>> {
    Ok(Some(ToolOutput::text("sync error handler")))
}
#[tool(is_enabled = sync(enabled_sync), needs_approval = sync(approval_sync), failure_error_function = sync(failure_sync))]
fn callbacks() -> std::result::Result<(), &'static str> {
    Err("error")
}

#[tokio::test]
async fn synchronous_policy_callbacks_share_the_existing_hook_contracts() {
    let tool = callbacks::tool().unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("callbacks"))
        .name("Callbacks")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("callbacks"), &agent);
    let call = CallId::new("approve");
    let args = json!({});
    let context = ToolContext::new(&run, &tool, &call, &args);
    assert!(tool.is_enabled(&run).await.unwrap());
    assert!(tool.needs_approval(&context).await.unwrap());
    let error = tool.call(context).await.unwrap_err();
    assert_eq!(
        tool.handle_failure(&ToolContext::new(&run, &tool, &call, &args), &error)
            .await
            .unwrap()
            .unwrap()
            .as_text(),
        Some("sync error handler")
    );
    let tool = callbacks::tool_with_options(
        ToolOptions::new().with_availability(ToolAvailability::Disabled),
    )
    .unwrap();
    assert!(!tool.is_enabled(&run).await.unwrap());
}

#[tool]
#[cfg(any())]
fn excluded(value: UnknownType) {}

mod public_api {
    #[ra_macros::tool]
    #[cfg_attr(all(), inline)]
    pub fn unit() {}

    #[ra_macros::tool]
    pub const fn constant() -> u32 {
        7
    }
}

#[tokio::test]
async fn public_companion_modules_preserve_visibility_and_function_only_attributes() {
    public_api::unit();
    assert_eq!(
        invoke(&public_api::constant::tool().unwrap(), json!({}))
            .await
            .unwrap()
            .as_text(),
        Some("7")
    );
    assert_eq!(
        invoke(&public_api::unit::tool().unwrap(), json!({}))
            .await
            .unwrap()
            .as_text(),
        Some("null")
    );
}

#[tool]
#[cfg_attr(all(), cfg(any()))]
fn conditionally_excluded(value: UnknownType) {}

#[tool(name = "", description = "")]
/// Empty overrides fall back to the declaration metadata.
fn empty_overrides() -> &'static str {
    "fallback"
}

#[test]
fn empty_overrides_follow_upstream_fallback_semantics() {
    let tool = empty_overrides::tool().unwrap();
    assert_eq!(tool.schema().name(), "empty_overrides");
    assert_eq!(
        tool.schema().description(),
        Some("Empty overrides fall back to the declaration metadata.")
    );
}
