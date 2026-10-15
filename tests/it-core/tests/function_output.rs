//! Function output declarations follow the reference's strict object and type-adapter contracts.

use ra_core::{
    item::{CallId, ItemId, ModelInputItem, RunItem, RunItemKind, ToolCallOutput},
    tool::{FunctionToolOutputSchema, ToolOutput, ToolTimeoutError},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

#[derive(Deserialize, Serialize, JsonSchema)]
struct Inventory {
    sku: String,
    #[serde(rename = "availableUnits")]
    available_units: u32,
    #[serde(default)]
    note: Option<String>,
}

#[test]
fn typed_outputs_are_strict_objects_with_aliases_nullable_fields_and_callable_defaults() {
    let schema = FunctionToolOutputSchema::for_type::<Inventory>().unwrap();
    let advertised = schema.json_schema_value();
    assert_eq!(advertised["additionalProperties"], false);
    assert_eq!(
        advertised["required"],
        json!(["availableUnits", "note", "sku"])
    );
    assert!(advertised["properties"].get("availableUnits").is_some());
    let output = schema
        .validate_output(&ToolOutput::text(
            r#"{"sku":"A","availableUnits":2,"extra":true}"#,
        ))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(output.as_text().unwrap()).unwrap(),
        json!({"sku":"A","availableUnits":2,"note":null})
    );
    assert!(
        schema
            .validate_output(&ToolOutput::text(r#"{"sku":"A","availableUnits":"many"}"#))
            .is_err()
    );
    assert!(
        schema
            .validate_return(Value::String(
                r#"{"sku":"A","availableUnits":2}"#.to_owned()
            ))
            .is_err()
    );
    assert!(FunctionToolOutputSchema::for_type::<String>().is_err());
    assert!(FunctionToolOutputSchema::for_type::<Vec<Inventory>>().is_err());
    assert!(FunctionToolOutputSchema::for_type::<BTreeMap<String, u32>>().is_err());
}

#[test]
fn low_level_schema_normalizes_a_copy_and_only_requires_object_output() {
    let input = json!({"type":["object"],"properties":{"count":{"type":"integer"}}});
    let schema = FunctionToolOutputSchema::json_schema(input.clone()).unwrap();
    assert_eq!(input["type"], json!(["object"]));
    assert!(input.get("additionalProperties").is_none());
    assert_eq!(schema.json_schema_value()["type"], "object");
    assert_eq!(schema.json_schema_value()["required"], json!(["count"]));
    assert_eq!(
        schema
            .validate_output(&ToolOutput::text(r#"{"other":"accepted"}"#))
            .unwrap()
            .as_text(),
        Some(r#"{"other":"accepted"}"#)
    );
    for text in ["text", "[]", "null", "42"] {
        assert!(schema.validate_output(&ToolOutput::text(text)).is_err());
    }
    for invalid in [
        json!({"type":"string"}),
        json!({"type":"object","additionalProperties":true}),
        json!({"type":"object","additionalProperties":{"type":"string"}}),
    ] {
        assert!(FunctionToolOutputSchema::json_schema(invalid).is_err());
    }
}

#[test]
fn output_schema_resolves_escaped_root_references_and_rejects_cycles_and_deep_schemas() {
    let schema = FunctionToolOutputSchema::json_schema(json!({
        "$ref":"#/$defs/a~1b~0c", "$defs":{"a/b~c":{"type":"object","properties":{"ok":{"type":"boolean"}}}}
    })).unwrap();
    assert_eq!(schema.json_schema_value()["required"], json!(["ok"]));
    assert!(
        FunctionToolOutputSchema::json_schema(
            json!({"$ref":"#/$defs/a","$defs":{"a":{"$ref":"#/$defs/a"}}})
        )
        .is_err()
    );
    let mut deep = json!({"type":"object"});
    for _ in 0..110 {
        deep = json!({"type":"object","properties":{"child":deep}});
    }
    let error = FunctionToolOutputSchema::json_schema(deep).unwrap_err();
    assert!(error.to_string().contains("deeply nested"));
}

#[test]
fn sdk_data_survives_storage_and_is_absent_from_model_projection() {
    let output = ToolCallOutput::new(CallId::new("call"), json!("model output"))
        .with_custom_data(json!({"renderer":"chart"}).as_object().unwrap().clone());
    let item = RunItem::new(ItemId::new("output"), RunItemKind::ToolCallOutput(output));
    let restored: RunItem = serde_json::from_value(serde_json::to_value(item).unwrap()).unwrap();
    let RunItemKind::ToolCallOutput(output) = restored.kind() else {
        panic!("tool output")
    };
    assert_eq!(output.custom_data().unwrap()["renderer"], "chart");
    let ModelInputItem::ToolCallOutput(projected) = restored.to_model_input().unwrap() else {
        panic!("tool output")
    };
    assert!(projected.custom_data().is_none());
    assert!(
        serde_json::to_value(projected)
            .unwrap()
            .get("custom_data")
            .is_none()
    );
    let legacy: ToolCallOutput =
        serde_json::from_value(json!({"schema_version":1,"call_id":"call","output":"old"}))
            .unwrap();
    assert!(legacy.custom_data().is_none());
}

#[test]
fn timeout_text_matches_upstream_general_float_formatting() {
    for (seconds, expected) in [
        (1.0, "1"),
        (0.01, "0.01"),
        (0.000001, "1e-06"),
        (1234567.0, "1.23457e+06"),
        (1.2345678, "1.23457"),
    ] {
        let error = ToolTimeoutError::new("lookup", Duration::from_secs_f64(seconds));
        assert_eq!(
            error.to_string(),
            format!("Tool 'lookup' timed out after {expected} seconds.")
        );
        assert_eq!(error.tool_name(), "lookup");
    }
}

#[test]
fn output_advertising_is_counted_and_definition_fields_round_trip() {
    use ra_core::{model::ModelToolDefinition, tool::ToolCaller};
    let plain = ModelToolDefinition::new("lookup", json!({"type":"object"}));
    let output = json!({"type":"object","additionalProperties":false,"properties":{}});
    let configured = plain
        .clone()
        .with_output_json_schema(output.clone())
        .with_allowed_callers(vec![ToolCaller::Programmatic]);
    let restored: ModelToolDefinition =
        serde_json::from_value(serde_json::to_value(&configured).unwrap()).unwrap();
    assert_eq!(restored, configured);
    assert_eq!(
        configured.advertised_bytes().unwrap(),
        plain.advertised_bytes().unwrap() + output.to_string().len() + r#"["programmatic"]"#.len()
    );
    let empty = ToolCallOutput::new(CallId::new("call"), json!("text"))
        .with_custom_data(Default::default());
    assert!(empty.custom_data().is_none());
}
