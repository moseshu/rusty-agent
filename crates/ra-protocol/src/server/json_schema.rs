//! How a tool's input schema is read into the JSON Schema subset tool definitions use: Codex's
//! `codex-tools` `json_schema` module, which its app-server uses to check a client's dynamic tools
//! and its tool router uses to define them for the model.
//!
//! A schema is sanitized to fit the subset, its unreachable local definitions are pruned, and an
//! unusually large one is compacted; reading it into the subset then drops the keywords outside
//! it. A schema that cannot be read, or one that is only `null`, is refused.
//!
//! Codex resolves local `$ref`s with the `urlencoding` and `jsonptr` crates; the few rules used
//! here — percent-decoding the fragment and splitting a JSON Pointer into its unescaped tokens —
//! are written out instead of adding the two dependencies.

mod compaction;
mod traversal;
mod types;

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

pub(crate) use self::types::{JsonSchema, JsonSchemaPrimitiveType, JsonSchemaType};
use self::{
    compaction::{DEFAULT_COMPACT_TOOL_SCHEMA_BYTES, compact_large_tool_schema},
    traversal::{DefinitionTraversal, for_each_schema_child},
};

const DEFINITION_TABLE_KEYS: [&str; 2] = ["$defs", "definitions"];
const SCHEMA_CHILD_KEYS: [&str; 4] = ["items", "anyOf", "oneOf", "allOf"];
const COMPOSITION_SCHEMA_KEYS: [&str; 3] = ["anyOf", "oneOf", "allOf"];

/// Reads a tool's input schema into the subset, compacting one over Codex's default size:
/// Codex's `parse_tool_input_schema`.
pub(crate) fn parse_tool_input_schema(
    input_schema: &Value,
) -> Result<JsonSchema, serde_json::Error> {
    parse_tool_input_schema_with_max_bytes(input_schema, DEFAULT_COMPACT_TOOL_SCHEMA_BYTES)
}

/// Reads a tool's input schema into the subset, compacting one over `max_bytes`.
pub(crate) fn parse_tool_input_schema_with_max_bytes(
    input_schema: &Value,
    max_bytes: usize,
) -> Result<JsonSchema, serde_json::Error> {
    let mut input_schema = prepare_tool_input_schema(input_schema);
    compact_large_tool_schema(&mut input_schema, max_bytes);
    deserialize_tool_input_schema(input_schema)
}

/// Reads a trusted tool input schema into the subset without compacting it: Codex's
/// `parse_tool_input_schema_without_compaction`. Codex reads the schemas it trusts this way;
/// nothing here does yet, so only the ported tests reach it.
#[cfg(feature = "test-api")]
pub(crate) fn parse_tool_input_schema_without_compaction(
    input_schema: &Value,
) -> Result<JsonSchema, serde_json::Error> {
    deserialize_tool_input_schema(prepare_tool_input_schema(input_schema))
}

fn prepare_tool_input_schema(input_schema: &Value) -> Value {
    let mut input_schema = input_schema.clone();
    sanitize_json_schema(&mut input_schema);
    prune_unreachable_definitions(&mut input_schema);
    input_schema
}

fn deserialize_tool_input_schema(input_schema: Value) -> Result<JsonSchema, serde_json::Error> {
    let schema: JsonSchema = serde_json::from_value(input_schema)?;
    if matches!(
        schema.schema_type,
        Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Null))
    ) {
        return Err(singleton_null_schema_error());
    }
    Ok(schema)
}

fn has_composition_keyword(map: &Map<String, Value>) -> bool {
    COMPOSITION_SCHEMA_KEYS
        .into_iter()
        .any(|key| map.contains_key(key))
}

/// Sanitizes a schema so it fits the subset: Codex's `sanitize_json_schema`.
///
/// - A boolean schema becomes an accept-all string.
/// - `anyOf`, `oneOf`, `allOf`, `$ref` and reachable local definitions are kept.
/// - `const` becomes a single-value `enum`.
/// - A schema without a `type` gets the one its keywords imply; one with no recognized keyword
///   becomes `{}`.
/// - An object or array type, including in a nullable union, gets permissive default
///   `properties` or `items` when it has none.
fn sanitize_json_schema(value: &mut Value) {
    match value {
        Value::Bool(_) => {
            *value = json!({ "type": "string" });
        }
        Value::Array(values) => {
            for value in values {
                sanitize_json_schema(value);
            }
        }
        Value::Object(map) => {
            if let Some(properties) = map.get_mut("properties").and_then(Value::as_object_mut) {
                for value in properties.values_mut() {
                    sanitize_json_schema(value);
                }
            }
            if let Some(items) = map.get_mut("items") {
                sanitize_json_schema(items);
            }
            if let Some(additional_properties) = map.get_mut("additionalProperties")
                && !matches!(additional_properties, Value::Bool(_))
            {
                sanitize_json_schema(additional_properties);
            }
            if let Some(value) = map.get_mut("prefixItems") {
                sanitize_json_schema(value);
            }
            for key in COMPOSITION_SCHEMA_KEYS {
                if let Some(value) = map.get_mut(key) {
                    sanitize_json_schema(value);
                }
            }
            for table in DEFINITION_TABLE_KEYS {
                sanitize_schema_table(map, table);
            }

            if let Some(const_value) = map.remove("const") {
                map.insert("enum".to_owned(), Value::Array(vec![const_value]));
            }

            let mut schema_types = normalized_schema_types(map);

            if schema_types.is_empty() && (map.contains_key("$ref") || has_composition_keyword(map))
            {
                return;
            }

            if schema_types.is_empty() {
                if map.contains_key("properties")
                    || map.contains_key("required")
                    || map.contains_key("additionalProperties")
                {
                    schema_types.push(JsonSchemaPrimitiveType::Object);
                } else if map.contains_key("items") || map.contains_key("prefixItems") {
                    schema_types.push(JsonSchemaPrimitiveType::Array);
                } else if map.contains_key("enum") || map.contains_key("format") {
                    schema_types.push(JsonSchemaPrimitiveType::String);
                } else if map.contains_key("minimum")
                    || map.contains_key("maximum")
                    || map.contains_key("exclusiveMinimum")
                    || map.contains_key("exclusiveMaximum")
                    || map.contains_key("multipleOf")
                {
                    schema_types.push(JsonSchemaPrimitiveType::Number);
                } else {
                    map.clear();
                    return;
                }
            }

            write_schema_types(map, &schema_types);
            ensure_default_children_for_schema_types(map, &schema_types);
        }
        _ => {}
    }
}

/// Sanitizes a definition table, or drops it if it is not an object, so a malformed table that
/// nothing may reach does not refuse the schema.
fn sanitize_schema_table(map: &mut Map<String, Value>, key: &str) {
    let should_remove = match map.get_mut(key) {
        Some(Value::Object(definitions)) => {
            for definition in definitions.values_mut() {
                sanitize_json_schema(definition);
            }
            false
        }
        Some(_) => true,
        None => false,
    };

    if should_remove {
        map.remove(key);
    }
}

fn ensure_default_children_for_schema_types(
    map: &mut Map<String, Value>,
    schema_types: &[JsonSchemaPrimitiveType],
) {
    if schema_types.contains(&JsonSchemaPrimitiveType::Object) && !map.contains_key("properties") {
        map.insert("properties".to_owned(), Value::Object(Map::new()));
    }

    if schema_types.contains(&JsonSchemaPrimitiveType::Array) && !map.contains_key("items") {
        map.insert("items".to_owned(), json!({ "type": "string" }));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DefinitionPointer {
    table: &'static str,
    name: String,
}

/// Drops root definitions the schema never references, which would only cost tokens.
fn prune_unreachable_definitions(value: &mut Value) {
    let reachable = collect_reachable_definitions(value);
    let Value::Object(map) = value else {
        return;
    };

    for table in DEFINITION_TABLE_KEYS {
        prune_schema_table(map, table, &reachable);
    }
}

fn prune_schema_table(
    map: &mut Map<String, Value>,
    table: &'static str,
    reachable: &BTreeSet<DefinitionPointer>,
) {
    let Some(Value::Object(definitions)) = map.get_mut(table) else {
        return;
    };

    definitions.retain(|name, _| {
        reachable.contains(&DefinitionPointer {
            table,
            name: name.clone(),
        })
    });

    if definitions.is_empty() {
        map.remove(table);
    }
}

fn collect_reachable_definitions(value: &Value) -> BTreeSet<DefinitionPointer> {
    let mut reachable = BTreeSet::new();
    let mut pending = Vec::new();

    collect_refs_outside_definitions(value, &mut pending);

    while let Some(pointer) = pending.pop() {
        if !reachable.insert(pointer.clone()) {
            continue;
        }

        if let Some(definition) = definition_for_pointer(value, &pointer) {
            collect_refs(definition, &mut pending);
        }
    }

    reachable
}

fn collect_refs_outside_definitions(value: &Value, refs: &mut Vec<DefinitionPointer>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_refs_outside_definitions(value, refs);
            }
        }
        Value::Object(map) => {
            collect_ref_from_map(map, refs);
            for_each_schema_child(map, DefinitionTraversal::Skip, &mut |value| {
                collect_refs_outside_definitions(value, refs);
            });
        }
        _ => {}
    }
}

fn collect_refs(value: &Value, refs: &mut Vec<DefinitionPointer>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_refs(value, refs);
            }
        }
        Value::Object(map) => {
            collect_ref_from_map(map, refs);
            for value in map.values() {
                collect_refs(value, refs);
            }
        }
        _ => {}
    }
}

fn collect_ref_from_map(map: &Map<String, Value>, refs: &mut Vec<DefinitionPointer>) {
    if let Some(Value::String(schema_ref)) = map.get("$ref")
        && let Some(pointer) = parse_local_definition_ref(schema_ref)
    {
        refs.push(pointer);
    }
}

fn definition_for_pointer<'a>(value: &'a Value, pointer: &DefinitionPointer) -> Option<&'a Value> {
    let Value::Object(map) = value else {
        return None;
    };

    map.get(pointer.table)
        .and_then(Value::as_object)
        .and_then(|definitions| definitions.get(&pointer.name))
}

/// The root definition a local `$ref` points into. A nested ref such as
/// `#/$defs/User/properties/name`, which the Responses API accepts in non-strict mode, keeps its
/// parent definition reachable.
fn parse_local_definition_ref(schema_ref: &str) -> Option<DefinitionPointer> {
    let fragment = schema_ref.strip_prefix('#')?;
    let pointer = percent_decode(fragment)?;
    let mut tokens = json_pointer_tokens(&pointer)?.into_iter();

    let table_token = tokens.next()?;
    let table = DEFINITION_TABLE_KEYS
        .into_iter()
        .find(|candidate| table_token == *candidate)?;
    let name = tokens.next()?;
    Some(DefinitionPointer { table, name })
}

/// Decodes `%XX` escapes as UTF-8, leaving any `%` that does not start one as it is, as
/// `urlencoding::decode` does. `None` if the decoded bytes are not UTF-8.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex_value),
                bytes.get(index + 2).copied().and_then(hex_value),
            )
        {
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The unescaped tokens of a JSON Pointer (RFC 6901), as `jsonptr::Pointer::parse` reads one:
/// empty for the root, otherwise each `/`-led token with `~1` read as `/` and `~0` as `~`. `None`
/// for a pointer that does not start with `/` or holds any other `~` escape.
fn json_pointer_tokens(pointer: &str) -> Option<Vec<String>> {
    if pointer.is_empty() {
        return Some(Vec::new());
    }
    pointer
        .strip_prefix('/')?
        .split('/')
        .map(|token| {
            let mut decoded = String::with_capacity(token.len());
            let mut chars = token.chars();
            while let Some(character) = chars.next() {
                if character == '~' {
                    match chars.next() {
                        Some('0') => decoded.push('~'),
                        Some('1') => decoded.push('/'),
                        _ => return None,
                    }
                } else {
                    decoded.push(character);
                }
            }
            Some(decoded)
        })
        .collect()
}

fn normalized_schema_types(map: &Map<String, Value>) -> Vec<JsonSchemaPrimitiveType> {
    let Some(schema_type) = map.get("type") else {
        return Vec::new();
    };

    match schema_type {
        Value::String(schema_type) => schema_type_from_str(schema_type).into_iter().collect(),
        Value::Array(schema_types) => schema_types
            .iter()
            .filter_map(Value::as_str)
            .filter_map(schema_type_from_str)
            .collect(),
        _ => Vec::new(),
    }
}

fn write_schema_types(map: &mut Map<String, Value>, schema_types: &[JsonSchemaPrimitiveType]) {
    match schema_types {
        [] => {
            map.remove("type");
        }
        [schema_type] => {
            map.insert(
                "type".to_owned(),
                Value::String(schema_type_name(*schema_type).to_owned()),
            );
        }
        _ => {
            map.insert(
                "type".to_owned(),
                Value::Array(
                    schema_types
                        .iter()
                        .map(|schema_type| Value::String(schema_type_name(*schema_type).to_owned()))
                        .collect(),
                ),
            );
        }
    }
}

fn schema_type_from_str(schema_type: &str) -> Option<JsonSchemaPrimitiveType> {
    match schema_type {
        "string" => Some(JsonSchemaPrimitiveType::String),
        "number" => Some(JsonSchemaPrimitiveType::Number),
        "boolean" => Some(JsonSchemaPrimitiveType::Boolean),
        "integer" => Some(JsonSchemaPrimitiveType::Integer),
        "object" => Some(JsonSchemaPrimitiveType::Object),
        "array" => Some(JsonSchemaPrimitiveType::Array),
        "null" => Some(JsonSchemaPrimitiveType::Null),
        _ => None,
    }
}

const fn schema_type_name(schema_type: JsonSchemaPrimitiveType) -> &'static str {
    match schema_type {
        JsonSchemaPrimitiveType::String => "string",
        JsonSchemaPrimitiveType::Number => "number",
        JsonSchemaPrimitiveType::Boolean => "boolean",
        JsonSchemaPrimitiveType::Integer => "integer",
        JsonSchemaPrimitiveType::Object => "object",
        JsonSchemaPrimitiveType::Array => "array",
        JsonSchemaPrimitiveType::Null => "null",
    }
}

fn singleton_null_schema_error() -> serde_json::Error {
    serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "tool input schema must not be a singleton null type",
    ))
}
