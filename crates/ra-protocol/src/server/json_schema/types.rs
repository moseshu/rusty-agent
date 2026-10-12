//! The JSON Schema subset a tool's input is read into: Codex's `codex-tools` `json_schema` types.
//! Keywords outside the subset are dropped when a schema is read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A primitive JSON Schema type name: the subset of the Structured Outputs schema types Codex
/// supports.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum JsonSchemaPrimitiveType {
    String,
    Number,
    Boolean,
    Integer,
    Object,
    Array,
    Null,
}

/// A schema's `type`: one name or a union of names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum JsonSchemaType {
    Single(JsonSchemaPrimitiveType),
    Multiple(Vec<JsonSchemaPrimitiveType>),
}

/// The schema subset tool definitions use: Codex's `JsonSchema`. Codex's MCP-only input size
/// limit, which is never serialized, has no counterpart here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct JsonSchema {
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub(crate) schema_ref: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub(crate) schema_type: Option<JsonSchemaType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    /// The Responses marker for reviewed encrypted tool parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) encrypted: Option<bool>,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub(crate) enum_values: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) items: Option<Box<Self>>,
    #[serde(rename = "minItems", skip_serializing_if = "Option::is_none")]
    pub(crate) min_items: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) properties: Option<BTreeMap<String, Self>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) required: Option<Vec<String>>,
    #[serde(
        rename = "additionalProperties",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) additional_properties: Option<AdditionalProperties>,
    #[serde(rename = "anyOf", skip_serializing_if = "Option::is_none")]
    pub(crate) any_of: Option<Vec<Self>>,
    #[serde(rename = "oneOf", skip_serializing_if = "Option::is_none")]
    pub(crate) one_of: Option<Vec<Self>>,
    #[serde(rename = "allOf", skip_serializing_if = "Option::is_none")]
    pub(crate) all_of: Option<Vec<Self>>,
    #[serde(rename = "$defs", skip_serializing_if = "Option::is_none")]
    pub(crate) defs: Option<BTreeMap<String, Self>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) definitions: Option<BTreeMap<String, Self>>,
}

/// Whether other properties are allowed, and if so, the schema they must meet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum AdditionalProperties {
    Boolean(bool),
    Schema(Box<JsonSchema>),
}
