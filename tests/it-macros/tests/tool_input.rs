use core_contract::tool::{FuncSchema, ToolInput};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, PartialEq, Deserialize, JsonSchema, ToolInput)]
/// Searches one catalog.
///
/// Results are ordered by relevance.
struct SearchInput {
    /// Query text supplied by the user.
    query: String,
    /// Optional maximum result count.
    limit: Option<u32>,
    /// Nested filters.
    filters: SearchFilters,
}

#[derive(Debug, PartialEq, Deserialize, JsonSchema)]
struct SearchFilters {
    /// Include archived entries when true.
    include_archived: Option<bool>,
    /// Required catalog labels.
    labels: Vec<String>,
}

#[derive(Debug, PartialEq, Deserialize, JsonSchema, ToolInput)]
#[tool_input(strict = false, description = "Explicit description wins.")]
struct LooseInput {
    optional: Option<String>,
}

#[derive(Debug, PartialEq, Deserialize, JsonSchema, ToolInput)]
struct GenericInput<T> {
    value: T,
}

#[test]
fn derive_把类型与字段文档带进_schema() {
    let schema = SearchInput::tool_schema("catalog_search").unwrap();

    assert_eq!(
        schema.description(),
        Some("Searches one catalog.\n\nResults are ordered by relevance.")
    );
    assert_eq!(
        schema.input_schema()["properties"]["query"]["description"],
        "Query text supplied by the user."
    );
    assert!(schema.strict_json_schema());
    assert_eq!(schema.input_schema_hash().len(), 64);
    assert!(schema.input_schema().get("$schema").is_none());
    assert!(schema.input_schema().get("title").is_none());
    assert!(schema.input_schema().get("description").is_none());
}

#[test]
fn strict_schema_关闭额外字段_全字段_required_并保留_option_nullable() {
    let schema = SearchInput::tool_schema("catalog_search").unwrap();
    let root = schema.input_schema();

    assert_eq!(root["additionalProperties"], false);
    assert_eq!(
        root["required"],
        json!(["filters", "limit", "query"])
    );
    assert!(
        contains_null(&root["properties"]["limit"]),
        "{}",
        root["properties"]["limit"]
    );

    // A documented nested field carries sibling keywords, so its `$ref` is inlined and the now
    // unreferenced definition is dropped instead of shipping the same type twice per request.
    let nested = &root["properties"]["filters"];
    assert_eq!(nested["description"], "Nested filters.");
    assert_eq!(nested["additionalProperties"], false);
    assert_eq!(nested["required"], json!(["include_archived", "labels"]));
    assert!(contains_null(&nested["properties"]["include_archived"]));
    assert!(root.get("definitions").is_none(), "{root}");
}

#[derive(Debug, PartialEq, Deserialize, JsonSchema, ToolInput)]
struct ReferencingInput {
    filters: SearchFilters,
    tuple: (u32, String),
}

#[test]
fn 仍被引用的_definition_保留_元组按位置展开() {
    let schema = ReferencingInput::tool_schema("referencing").unwrap();
    let root = schema.input_schema();

    // No sibling keywords on this field, so the reference survives and must keep its definition.
    assert_eq!(root["properties"]["filters"]["$ref"], "#/definitions/SearchFilters");
    assert_eq!(
        root["definitions"]["SearchFilters"]["additionalProperties"],
        false
    );

    // schemars describes a tuple positionally; every entry is a schema node in its own right.
    let items = root["properties"]["tuple"]["items"]
        .as_array()
        .expect("tuple items should stay positional");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "integer");
    assert_eq!(items[1]["type"], "string");

    let decoded = ReferencingInput::func_schema("referencing")
        .unwrap()
        .decode_arguments(
            r#"{"filters":{"include_archived":null,"labels":[]},"tuple":[7,"seven"]}"#,
        );
    assert!(decoded.is_ok(), "{decoded:?}");
    assert!(
        ReferencingInput::func_schema("referencing")
            .unwrap()
            .decode_arguments(r#"{"filters":{"include_archived":null,"labels":[]},"tuple":["seven",7]}"#)
            .is_err(),
        "位置化元组必须逐位校验类型"
    );
}

/// A tool that takes no arguments.
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
struct NoArgsInput {}

#[test]
fn 无参数工具也能生成合法的_strict_schema() {
    // schemars describes an empty struct as bare `{"type":"object"}`. Normalization has to
    // complete it into an explicitly closed empty object, or the result fails the strict
    // verification it is meant to satisfy — and argument-less tools are a common shape.
    let schema = NoArgsInput::tool_schema("no_args").unwrap();

    assert_eq!(
        schema.input_schema(),
        &json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    );
    assert!(schema.strict_json_schema());
    assert!(
        NoArgsInput::func_schema("no_args")
            .unwrap()
            .decode_arguments("{}")
            .is_ok()
    );
}

/// 自引用类型无法表达成 strict schema，必须是错误而不是让进程崩掉。
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
struct RecursiveInput {
    /// 下一个节点。
    next: Box<RecursiveInput>,
}

#[test]
fn 自引用类型报错而不是撑爆调用栈() {
    let error = RecursiveInput::tool_schema("recursive")
        .expect_err("self-referential input must not be expanded forever");
    assert!(
        error.to_string().contains("self-referential"),
        "{error}"
    );
}

#[test]
fn 非_strict_模式保留_schemars_原始可选语义() {
    let schema = LooseInput::tool_schema("loose").unwrap();

    assert!(!schema.strict_json_schema());
    assert_eq!(schema.description(), Some("Explicit description wins."));
    assert!(!schema.input_schema()["required"]
        .as_array()
        .is_some_and(|required| required.iter().any(|name| name == "optional")));
}

#[test]
fn func_schema_的_decoder_与生成_schema_绑定同一输入类型() {
    let schema = SearchInput::func_schema("catalog_search").unwrap();
    let decoded = schema
        .decode_arguments(
            r#"{"query":"rust","limit":3,"filters":{"include_archived":null,"labels":["sdk"]}}"#,
        )
        .unwrap();

    assert!(decoded.type_name().ends_with("SearchInput"));
    assert_eq!(
        decoded.downcast::<SearchInput>().unwrap(),
        SearchInput {
            query: "rust".to_owned(),
            limit: Some(3),
            filters: SearchFilters {
                include_archived: None,
                labels: vec!["sdk".to_owned()],
            },
        }
    );
    assert!(schema.decode_arguments(r#"{"query":3}"#).is_err());
    assert!(schema
        .decode_arguments(
            r#"{"query":"rust","filters":{"include_archived":null,"labels":[]}}"#,
        )
        .is_err());
    assert!(schema
        .decode_arguments(
            r#"{"query":"rust","limit":null,"filters":{"include_archived":null,"labels":[],"unknown":true}}"#,
        )
        .is_err());
}

#[test]
fn func_schema_记录_context_注入与返回类型但不把它们放进模型参数() {
    let schema = FuncSchema::for_input::<SearchInput>("catalog_search")
        .unwrap()
        .with_tool_context(true)
        .with_return_type::<Vec<String>>();

    assert!(schema.inject_tool_context());
    assert_eq!(schema.return_type_name(), Some("alloc::vec::Vec<alloc::string::String>"));
    assert!(!schema.tool_schema().input_schema()["properties"]
        .as_object()
        .unwrap()
        .contains_key("context"));
    let debug = format!("{schema:?}");
    assert!(debug.contains("<schema-bound-decoder>"));
}

#[test]
fn canonical_schema_与_hash_跨重复生成保持字节稳定() {
    let first = GenericInput::<String>::tool_schema("generic")
        .unwrap()
        .canonical_json()
        .unwrap();
    let first_hash = GenericInput::<String>::tool_schema("generic")
        .unwrap()
        .input_schema_hash()
        .to_owned();

    for _ in 0..100 {
        let schema = GenericInput::<String>::tool_schema("generic").unwrap();
        assert_eq!(schema.canonical_json().unwrap(), first);
        assert_eq!(schema.input_schema_hash(), first_hash);
    }
}

#[test]
fn tool_schema_反序列化会验证_hash_且兼容旧记录缺少_hash() {
    let schema = SearchInput::tool_schema("catalog_search").unwrap();
    let mut wire = serde_json::to_value(&schema).unwrap();
    wire["input_schema_hash"] = Value::String("0".repeat(64));
    assert!(serde_json::from_value::<core_contract::tool::ToolSchema>(wire).is_err());

    let mut old_wire = serde_json::to_value(schema).unwrap();
    old_wire.as_object_mut().unwrap().remove("input_schema_hash");
    let restored: core_contract::tool::ToolSchema = serde_json::from_value(old_wire).unwrap();
    assert_eq!(restored.input_schema_hash().len(), 64);
}

fn contains_null(schema: &Value) -> bool {
    match schema {
        Value::Object(object) => {
            object.get("type").is_some_and(|kind| {
                kind == "null"
                    || kind
                        .as_array()
                        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "null"))
            })
                || object.values().any(contains_null)
        }
        Value::Array(values) => values.iter().any(contains_null),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}
