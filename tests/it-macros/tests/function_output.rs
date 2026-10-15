//! Output options and inference work through generated constructors, including renamed core.

use core_contract::{
    agent::AgentSpec,
    context::RunContext,
    error::Result,
    item::CallId,
    state::RunId,
    tool::{Tool, ToolContext, ToolOptions, ToolOutput},
};
use ra_macros::tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize, JsonSchema)]
struct Inventory {
    sku: String,
    count: u32,
}

#[tool(allowed_callers = ["direct", "programmatic"])]
fn inferred() -> Inventory {
    Inventory {
        sku: "A".to_owned(),
        count: 2,
    }
}

#[tool(allowed_callers = ["programmatic"])]
async fn inferred_fallible() -> Result<Inventory> {
    Ok(inferred())
}

#[tool]
fn direct() -> Inventory {
    inferred()
}

#[tool(output_type = Inventory)]
fn overridden() -> Value {
    json!({"sku":"A","count":3})
}

#[tool(output_type = Inventory)]
fn invalid() -> Value {
    json!({"sku":"A","count":"many"})
}

#[tool(output_json_schema = json!({"type":"object","properties":{"count":{"type":"integer"}}}))]
fn raw_schema() -> Value {
    json!({"arbitrary":"object"})
}

#[tool(allowed_callers = ["programmatic"])]
fn untyped() -> Value {
    json!([1, 2])
}

#[tool(allowed_callers = ["programmatic"])]
fn plain() -> &'static str {
    "ok"
}

#[tool(allowed_callers = ["programmatic"])]
fn scalar() -> u32 {
    1
}

#[tool(output_type = String)]
fn non_object() -> String {
    "text".to_owned()
}

async fn invoke(tool: &dyn Tool) -> Result<ToolOutput> {
    let agent = AgentSpec::builder()
        .id(core_contract::agent::AgentId::new("test"))
        .name("Test")
        .build()?;
    let run = RunContext::new(RunId::new("run"), &agent);
    tool.call(ToolContext::new(
        &run,
        tool,
        &CallId::new("call"),
        &json!({}),
    ))
    .await
}

#[tokio::test]
async fn explicit_and_inferred_types_validate_outputs_and_preserve_plain_function_calls() {
    assert_eq!(inferred().count, 2);
    for tool in [
        Box::new(inferred::tool().unwrap()) as Box<dyn Tool>,
        Box::new(inferred_fallible::tool().unwrap()),
        Box::new(overridden::tool().unwrap()),
    ] {
        assert_eq!(
            tool.output_schema().unwrap().json_schema_value()["required"],
            json!(["count", "sku"])
        );
        assert!(tool.model_definition().output_json_schema().is_some());
        assert!(
            invoke(tool.as_ref())
                .await
                .unwrap()
                .as_text()
                .unwrap()
                .contains("\"sku\":\"A\"")
        );
    }
    assert!(invoke(&invalid::tool().unwrap()).await.is_err());
    assert!(direct::tool().unwrap().output_schema().is_none());
    assert!(untyped::tool().unwrap().output_schema().is_none());
    assert!(plain::tool().unwrap().output_schema().is_none());
    assert!(
        scalar::tool()
            .err()
            .unwrap()
            .to_string()
            .contains("return annotation")
    );
    assert!(
        non_object::tool()
            .err()
            .unwrap()
            .to_string()
            .contains("output_type")
    );
    assert!(
        inferred::tool_with_options(ToolOptions::new())
            .unwrap()
            .output_schema()
            .is_none()
    );
}

#[tokio::test]
async fn low_level_schema_is_normalized_and_does_not_add_property_validation() {
    let tool = raw_schema::tool().unwrap();
    assert_eq!(
        tool.output_schema().unwrap().json_schema_value()["additionalProperties"],
        false
    );
    assert_eq!(
        invoke(&tool).await.unwrap().as_text(),
        Some(r#"{"arbitrary":"object"}"#)
    );
}

#[tool(output_type = None, output_json_schema = None, timeout_error_function = None, custom_data_extractor = None)]
fn absent_options() -> Value {
    json!({"plain":true})
}

#[tool(strict = false, output_json_schema = json!({"type":"object"}))]
fn native_schema_output() -> ToolOutput {
    ToolOutput::text(r#"{"native":true}"#)
}

#[tokio::test]
async fn absent_options_and_native_low_level_results_keep_existing_output_conversion() {
    let tool = absent_options::tool().unwrap();
    assert!(tool.output_schema().is_none());
    assert_eq!(
        invoke(&tool).await.unwrap().as_text(),
        Some(r#"{"plain":true}"#)
    );
    let tool = native_schema_output::tool().unwrap();
    assert!(!tool.model_definition().strict());
    assert_eq!(
        tool.output_schema().unwrap().json_schema_value()["additionalProperties"],
        false
    );
    assert_eq!(
        invoke(&tool).await.unwrap().as_text(),
        Some(r#"{"native":true}"#)
    );
}
