//! Structured input for agent tools, ported from the reference `test_agent_tool_input.py`.

use std::sync::Arc;

use ra_core::{
    error::Result,
    item::{Message, ModelInputItem},
};
use ra_runtime::agent::tool::{
    AgentAsToolInput, STRUCTURED_INPUT_PREAMBLE, StructuredInputSchemaInfo,
    StructuredToolInputBuilder, StructuredToolInputBuilderOptions, StructuredToolInputResult,
    is_agent_tool_input, resolve_agent_tool_input,
};
use serde_json::{Value, json};

fn text(result: StructuredToolInputResult) -> String {
    match result {
        StructuredToolInputResult::Text(text) => text,
        other => panic!("expected text input, got {other:?}"),
    }
}

fn summary_of(schema: &Value) -> Option<String> {
    StructuredInputSchemaInfo::from_params_schema(Some(schema), false)
        .summary()
        .map(str::to_owned)
}

#[test]
fn agent_as_tool_input_schema_accepts_string() {
    let input: AgentAsToolInput = serde_json::from_value(json!({"input": "hi"})).unwrap();
    assert_eq!(input, AgentAsToolInput::new("hi"));
    assert!(serde_json::from_value::<AgentAsToolInput>(json!({"input": []})).is_err());
    assert!(is_agent_tool_input(&json!({"input": "hi"})));
    assert!(!is_agent_tool_input(&json!({"input": []})));

    let schema = AgentAsToolInput::json_schema();
    assert_eq!(schema["required"], json!(["input"]));
    assert_eq!(schema["additionalProperties"], json!(false));
}

#[tokio::test]
async fn resolve_agent_tool_input_returns_string_input() {
    let result = resolve_agent_tool_input(&json!({"input": "hello"}), None, None)
        .await
        .unwrap();
    assert_eq!(text(result), "hello");
}

#[tokio::test]
async fn resolve_agent_tool_input_falls_back_to_json() {
    let result = resolve_agent_tool_input(&json!({"foo": "bar"}), None, None)
        .await
        .unwrap();
    // `json.dumps` spacing, which is what the nested model reads in the reference.
    assert_eq!(text(result), r#"{"foo": "bar"}"#);
}

#[tokio::test]
async fn resolve_agent_tool_input_preserves_input_with_extra_fields() {
    let result =
        resolve_agent_tool_input(&json!({"input": "hello", "target": "world"}), None, None)
            .await
            .unwrap();
    assert_eq!(text(result), r#"{"input": "hello", "target": "world"}"#);
}

#[tokio::test]
async fn resolve_agent_tool_input_uses_default_builder_when_schema_info_exists() {
    let info = StructuredInputSchemaInfo::new().with_summary("Summary");
    let result = resolve_agent_tool_input(&json!({"foo": "bar"}), Some(&info), None)
        .await
        .unwrap();
    let text = text(result);
    assert!(text.starts_with(STRUCTURED_INPUT_PREAMBLE));
    assert!(text.contains("## Structured Input Data:"));
    assert!(text.contains("\"foo\": \"bar\""));
    assert!(text.contains("Input Schema Summary:"));
    assert!(text.contains("Summary"));
}

#[tokio::test]
async fn resolve_agent_tool_input_prefers_the_full_schema_over_the_summary() {
    let info = StructuredInputSchemaInfo::new()
        .with_summary("Summary")
        .with_json_schema(json!({"type": "object"}));
    let text = text(
        resolve_agent_tool_input(&json!({"foo": "bar"}), Some(&info), None)
            .await
            .unwrap(),
    );
    assert!(text.contains("## Input JSON Schema:"));
    assert!(!text.contains("Input Schema Summary:"));
}

#[tokio::test]
async fn resolve_agent_tool_input_returns_builder_items() {
    let items = vec![ModelInputItem::Message(Message::user("custom input"))];
    let expected = items.clone();
    let builder: Arc<dyn StructuredToolInputBuilder> = Arc::new(
        move |_options: StructuredToolInputBuilderOptions<'_>| -> Result<StructuredToolInputResult> {
            Ok(StructuredToolInputResult::Items(items.clone()))
        },
    );
    let result = resolve_agent_tool_input(&json!({"input": "ignored"}), None, Some(&*builder))
        .await
        .unwrap();
    assert_eq!(result, StructuredToolInputResult::Items(expected));
}

#[test]
fn build_structured_input_schema_info_handles_empty_schema() {
    let info = StructuredInputSchemaInfo::from_params_schema(None, false);
    assert_eq!(info.summary(), None);
    assert_eq!(info.json_schema(), None);
}

#[test]
fn build_structured_input_schema_info_generates_summary_for_simple_fields() {
    let schema = json!({
        "type": "object",
        "description": "Tool arguments.",
        "properties": {
            "mode": {"enum": ["fast", "safe"], "description": "Execution mode."},
            "status": {"const": "ok", "description": "Status marker."},
            "count": {"type": ["integer", "null"], "description": "Optional count."},
            "enabled": {"type": "boolean", "description": "Feature toggle."},
        },
        "required": ["mode", "status"],
    });

    let info = StructuredInputSchemaInfo::from_params_schema(Some(&schema), true);

    let summary = info.summary().unwrap();
    assert!(summary.contains("Description: Tool arguments."));
    assert!(summary.contains(r#"- mode (enum("fast" | "safe"), required) - Execution mode."#));
    assert!(summary.contains(r#"- status (literal("ok"), required) - Status marker."#));
    assert!(summary.contains("- count (integer | null, optional) - Optional count."));
    assert!(summary.contains("- enabled (boolean, optional) - Feature toggle."));
    assert_eq!(info.json_schema(), Some(&schema));
}

#[test]
fn schema_summary_returns_none_for_unsupported_shapes() {
    assert_eq!(summary_of(&json!({"type": "array"})), None);
    assert_eq!(
        summary_of(&json!({"type": "object", "properties": []})),
        None
    );
    assert_eq!(
        summary_of(&json!({
            "type": "object",
            "properties": {
                "nested": {"type": "object", "properties": {"x": {"type": "string"}}},
            },
        })),
        None
    );
    // Simple fields with no description anywhere are not worth summarizing.
    assert_eq!(
        summary_of(&json!({"type": "object", "properties": {"x": {"type": "string"}}})),
        None
    );
}

#[test]
fn schema_summary_field_edge_cases() {
    let with_field = |field: Value| {
        summary_of(&json!({
            "type": "object",
            "description": "Arguments.",
            "properties": {"field": field},
        }))
    };
    // Unsupported field shapes abandon the whole summary rather than describing it wrongly.
    assert_eq!(with_field(json!("not-a-dict")), None);
    assert_eq!(with_field(json!({"type": ["integer", "string"]})), None);
    assert_eq!(with_field(json!({"type": "array"})), None);
    assert_eq!(with_field(json!({})), None);

    assert!(
        with_field(json!({"enum": []}))
            .unwrap()
            .contains("- field (enum, optional)")
    );
    assert!(
        with_field(json!({"enum": [1, 2, 3, 4, 5, 6]}))
            .unwrap()
            .contains("enum(1 | 2 | 3 | 4 | 5 | ...)")
    );
}
