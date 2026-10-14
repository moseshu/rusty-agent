//! Ports of the reference output-schema cases and Rust decoding adaptations.

use ra_core::{
    agent::{AgentId, AgentSpec},
    error::{Error, Recoverability, Result},
    output::{OutputDecoder, OutputSchema, OutputValue},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
struct Person {
    name: String,
    age: i64,
    nickname: Option<String>,
}

#[test]
fn string_is_plain_text_and_objects_are_not_wrapped() {
    let text = OutputSchema::for_type::<String>().unwrap();
    assert!(text.is_plain_text());
    assert_eq!(
        text.validate_json("hello")
            .unwrap()
            .downcast_ref::<String>()
            .unwrap(),
        "hello"
    );
    let schema = OutputSchema::for_type::<Person>().unwrap();
    assert!(!schema.is_wrapped());
    assert_eq!(schema.strict(), Some(true));
    let wire = schema.json_schema_value().unwrap();
    assert_eq!(wire["additionalProperties"], false);
    assert!(
        wire["required"]
            .as_array()
            .unwrap()
            .contains(&json!("nickname"))
    );
    let value = schema
        .validate_json(r#"{"name":"Alice","age":25,"nickname":null}"#)
        .unwrap();
    assert_eq!(value.downcast_ref::<Person>().unwrap().age, 25);
    assert_eq!(value.value()["name"], "Alice");
}

#[test]
fn strict_rejects_type_coercion_and_loose_preserves_original_schema() {
    let strict = OutputSchema::for_type::<Person>().unwrap();
    let loose = OutputSchema::for_type_with_strict::<Person>(false).unwrap();
    assert_eq!(
        strict
            .validate_json(r#"{"name":"Alice","age":"25"}"#)
            .unwrap_err()
            .code(),
        "output.invalid_final_output"
    );
    assert!(
        loose
            .json_schema_value()
            .unwrap()
            .get("additionalProperties")
            .is_none()
    );
    assert_eq!(
        loose
            .validate_json(r#"{"name":"Alice","age":"25"}"#)
            .unwrap()
            .downcast_ref::<Person>()
            .unwrap()
            .age,
        25
    );
    // The type adapter owns default and unknown-field behavior, even when the provider schema
    // requires every field. Serde's explicit attributes are the Rust counterpart.
    assert!(
        strict
            .validate_json(r#"{"name":"Alice","age":25,"extra":true}"#)
            .is_ok()
    );
}

#[test]
fn lists_scalars_tuples_options_and_unions_use_response_wrapper() {
    let list = OutputSchema::for_type::<Vec<Person>>().unwrap();
    assert!(list.is_wrapped());
    let parsed = list
        .validate_json(r#"{"response":[{"name":"Alice","age":25}]}"#)
        .unwrap();
    assert_eq!(parsed.downcast_ref::<Vec<Person>>().unwrap().len(), 1);
    assert!(list.json_schema_value().unwrap()["definitions"].is_object());
    for invalid in [
        r#"[{"name":"Alice","age":25}]"#,
        r#"{"value":[]}"#,
        r#"{"response":"wrong"}"#,
        "no JSON",
    ] {
        assert_eq!(
            list.validate_json(invalid).unwrap_err().code(),
            "output.invalid_final_output"
        );
    }
    let integer = OutputSchema::for_type::<i64>().unwrap();
    assert_eq!(
        *integer
            .validate_json(r#"{"response":42}"#)
            .unwrap()
            .downcast_ref::<i64>()
            .unwrap(),
        42
    );
    let tuple = OutputSchema::for_type::<(String, bool)>().unwrap();
    assert_eq!(
        tuple
            .validate_json(r#"{"response":["ok",true]}"#)
            .unwrap()
            .downcast_ref::<(String, bool)>()
            .unwrap(),
        &("ok".to_owned(), true)
    );
    let optional = OutputSchema::for_type::<Option<i64>>().unwrap();
    assert_eq!(
        optional
            .validate_json(r#"{"response":null}"#)
            .unwrap()
            .downcast_ref::<Option<i64>>()
            .unwrap(),
        &None
    );
    #[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
    #[serde(untagged)]
    enum Choice {
        Number(i64),
        Text(String),
    }
    let union = OutputSchema::for_type::<Choice>().unwrap();
    assert!(union.is_wrapped());
    assert_eq!(
        union
            .validate_json(r#"{"response":"ok"}"#)
            .unwrap()
            .downcast_ref::<Choice>()
            .unwrap(),
        &Choice::Text("ok".to_owned())
    );
}

#[test]
fn open_maps_are_unwrapped_and_require_loose_mode() {
    assert!(OutputSchema::for_type::<BTreeMap<String, i64>>().is_err());
    let map = OutputSchema::for_type_with_strict::<BTreeMap<String, i64>>(false).unwrap();
    assert!(!map.is_wrapped());
    assert_eq!(
        map.validate_json(r#"{"foo":1}"#)
            .unwrap()
            .downcast_ref::<BTreeMap<String, i64>>()
            .unwrap()["foo"],
        1
    );
    assert!(map.validate_json(r#"{"response":{"foo":1}}"#).is_err());
}

#[test]
fn handwritten_schema_validates_constraints_references_and_extra_keys() {
    let schema = OutputSchema::json_schema(
        "constrained",
        json!({
            "type":"object", "properties":{"answer":{"$ref":"#/definitions/answer"}},
            "required":["answer"], "additionalProperties":false,
            "definitions":{"answer":{"type":"integer","minimum":2,"maximum":4}}
        }),
    );
    schema.validate().unwrap();
    assert_eq!(
        schema.validate_json(r#"{"answer":3}"#).unwrap().value(),
        &json!({"answer":3})
    );
    for invalid in [
        r#"{"answer":"3"}"#,
        r#"{"answer":5}"#,
        r#"{"answer":3,"extra":true}"#,
        "{}",
    ] {
        let error = schema.validate_json(invalid).unwrap_err();
        assert_eq!(error.recoverability(), Recoverability::RetryableWithChange);
        assert_eq!(error.code(), "output.invalid_final_output");
    }
    let invalid =
        OutputSchema::json_schema("broken", json!({"type":"nonsense"})).with_strict(false);
    assert!(
        AgentSpec::builder()
            .id(AgentId::new("agent"))
            .name("Agent")
            .output_schema(invalid)
            .build()
            .is_err()
    );
    let external =
        OutputSchema::json_schema("external", json!({"$ref":"file:///missing-schema.json"}))
            .with_strict(false);
    assert!(external.validate().is_err());
}

struct CustomDecoder(Arc<AtomicUsize>);
impl OutputDecoder for CustomDecoder {
    fn decode(&self, value: Value, _: bool) -> Result<OutputValue> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if value["valid"] == true {
            OutputValue::typed(vec!["some".to_owned(), "output".to_owned()])
        } else {
            Err(Error::invalid_final_output(
                "custom validation rejected output",
            ))
        }
    }
}

#[test]
fn custom_decoder_owns_business_validation_and_is_not_repeated_on_read() {
    let calls = Arc::new(AtomicUsize::new(0));
    let schema = OutputSchema::json_schema("custom", json!({"type":"object"}))
        .with_strict(false)
        .with_decoder(Arc::new(CustomDecoder(calls.clone())));
    let output = schema.validate_json(r#"{"valid":true}"#).unwrap();
    assert_eq!(
        output.downcast_ref::<Vec<String>>().unwrap(),
        &["some", "output"]
    );
    assert_eq!(
        output.clone().downcast_ref::<Vec<String>>().unwrap().len(),
        2
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        schema
            .validate_json(r#"{"valid":false}"#)
            .unwrap_err()
            .code(),
        "output.invalid_final_output"
    );
}

#[test]
fn handler_formatting_wraps_once_and_derived_builders_preserve_decoders() {
    let schema = OutputSchema::for_type::<Vec<i64>>().unwrap();
    assert_eq!(
        schema.format_output(&vec![1, 2]).unwrap(),
        r#"{"response":[1,2]}"#
    );
    assert_eq!(
        schema.format_output(&json!({"response":[1,2]})).unwrap(),
        r#"{"response":[1,2]}"#
    );
    let agent = AgentSpec::builder()
        .id(AgentId::new("typed"))
        .name("Typed")
        .output_type::<Person>()
        .unwrap()
        .build()
        .unwrap();
    let derived = agent.to_builder().name("Derived").build().unwrap();
    assert_eq!(derived.output_schema(), agent.output_schema());
    assert!(
        derived
            .output_schema()
            .validate_json(r#"{"name":"A","age":1}"#)
            .unwrap()
            .downcast_ref::<Person>()
            .is_some()
    );
}
