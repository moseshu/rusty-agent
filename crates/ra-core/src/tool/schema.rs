//! Versioned schema and argument-decoder boundary for tools.

use std::{any::Any, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned, de::Error as _};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    canonicalize_json,
    origin::validate_tool_name,
    strict::{ensure_strict_json_schema, verify_strict_json_schema},
};
use crate::{
    compat::{SchemaVersion, Unknown},
    error::{Error, Result, ToolErrorKind},
    model::ModelToolDefinition,
};

/// Current tool-schema version.
pub const TOOL_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);
/// Current function-schema/decoder boundary version.
pub const FUNC_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Supertrait bundle used only by `ToolInput` macro expansions for generic structs.
#[doc(hidden)]
pub trait ToolInputRequirements: DeserializeOwned + JsonSchema + Send + Sync + 'static {}

impl<T> ToolInputRequirements for T where T: DeserializeOwned + JsonSchema + Send + Sync + 'static {}

/// A typed argument object accepted by a tool.
///
/// `#[derive(ra_macros::ToolInput)]` supplies the constants. `Deserialize` and `JsonSchema` remain
/// explicit derives so the Rust compiler proves that decoding and schema generation describe the
/// same concrete type.
pub trait ToolInput: ToolInputRequirements {
    /// Type-level documentation shown as the tool description.
    const DESCRIPTION: Option<&'static str> = None;
    /// Whether strict schema normalization is enabled.
    const STRICT_JSON_SCHEMA: bool = true;

    /// Builds the versioned model-facing schema for this exact decodable type.
    fn tool_schema(name: impl Into<String>) -> Result<ToolSchema>
    where
        Self: Sized,
    {
        let root = schemars::schema_for!(Self);
        let mut input_schema = serde_json::to_value(root).map_err(|error| {
            Error::config("failed to serialize derived tool schema").with_source(error)
        })?;
        if let Some(root) = input_schema.as_object_mut() {
            // These belong to the function envelope, not the parameter schema. Keeping them here
            // duplicates the derive docs on every request and wastes the schema token budget.
            root.remove("$schema");
            root.remove("title");
            root.remove("description");
        }
        let mut schema = if Self::STRICT_JSON_SCHEMA {
            ensure_strict_json_schema(&mut input_schema)?;
            ToolSchema::new(name, input_schema)?
        } else {
            ToolSchema::loose(name, input_schema)?
        };
        if let Some(description) = Self::DESCRIPTION {
            schema = schema.with_description(description);
        }
        Ok(schema)
    }

    /// Builds a schema tied to a decoder for this exact type.
    fn func_schema(name: impl Into<String>) -> Result<FuncSchema>
    where
        Self: Sized,
    {
        FuncSchema::for_input::<Self>(name)
    }
}

/// Stable schema half of an executable tool.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSchema {
    schema_version: SchemaVersion,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
    input_schema_hash: String,
    #[serde(default = "default_true")]
    strict_json_schema: bool,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolSchema {
    /// Creates a strict schema, rejecting one that does not satisfy the strict-mode invariants.
    ///
    /// Strictness is checked here rather than assumed, because `strict` is also sent to the
    /// provider: a schema that claims strict mode without `additionalProperties: false` and a
    /// complete `required` list is rejected by the provider, far from the code that built it.
    /// Derived inputs are normalized by the `ToolInput` derive before reaching this constructor;
    /// hand-written and MCP-sourced schemas must already be strict, or use [`Self::loose`].
    pub fn new(name: impl Into<String>, input_schema: Value) -> Result<Self> {
        let schema = Self::build(name, input_schema, true)?;
        verify_strict_json_schema(&schema.input_schema)?;
        Ok(schema)
    }

    /// Creates a schema that opts out of provider-side strict validation.
    pub fn loose(name: impl Into<String>, input_schema: Value) -> Result<Self> {
        Self::build(name, input_schema, false)
    }

    fn build(name: impl Into<String>, input_schema: Value, strict: bool) -> Result<Self> {
        let name = name.into();
        validate_tool_name(&name)?;
        let mut input_schema = input_schema;
        canonicalize_json(&mut input_schema);
        let input_schema_hash = hash_schema(&input_schema)?;
        Ok(Self {
            schema_version: TOOL_SCHEMA_VERSION,
            name,
            description: None,
            input_schema,
            input_schema_hash,
            strict_json_schema: strict,
            unknown: Unknown::new(),
        })
    }

    /// Adds the behavior contract shown to the model.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Revalidates persisted identity, content-addressed fields, and the strict-mode claim.
    pub fn validate(&self) -> Result<()> {
        validate_tool_name(&self.name)?;
        let actual = hash_schema(&self.input_schema)?;
        if actual != self.input_schema_hash {
            return Err(Error::config(format!(
                "tool schema hash mismatch for `{}`",
                self.name
            )));
        }
        if self.strict_json_schema {
            verify_strict_json_schema(&self.input_schema)?;
        }
        Ok(())
    }

    /// Projects only model-visible fields across the provider boundary.
    #[must_use]
    pub fn to_model_definition(&self) -> ModelToolDefinition {
        let definition = ModelToolDefinition::new(self.name.clone(), self.input_schema.clone())
            .with_strict(self.strict_json_schema);
        match &self.description {
            Some(description) => definition.with_description(description.clone()),
            None => definition,
        }
    }

    /// Deterministic compact JSON used for hashing, snapshots, and provider rendering.
    pub fn canonical_json(&self) -> Result<String> {
        serde_json::to_string(&self.input_schema).map_err(|error| {
            Error::config("failed to render canonical tool schema").with_source(error)
        })
    }

    /// What one advertised entry costs in the turn's tool table, in bytes.
    ///
    /// Measured on [`ModelToolDefinition::advertised_bytes`], which is the single definition of
    /// the measure: a per-tool ceiling and a whole-surface budget that each computed their own
    /// would drift, and the surface would then pass while its entries failed. This form exists so
    /// a per-tool assertion can be written against the schema it is looking at; a tool that
    /// overrides its own projection is measured by that projection instead.
    pub fn advertised_bytes(&self) -> Result<usize> {
        self.to_model_definition().advertised_bytes()
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Public name advertised to the model.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Optional model-facing behavior contract.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Canonical input JSON schema.
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// Lowercase SHA-256 of [`Self::canonical_json`].
    #[must_use]
    pub fn input_schema_hash(&self) -> &str {
        &self.input_schema_hash
    }

    /// Whether providers should request strict schema validation.
    #[must_use]
    pub const fn strict_json_schema(&self) -> bool {
        self.strict_json_schema
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

#[derive(Deserialize)]
struct ToolSchemaWire {
    schema_version: SchemaVersion,
    name: String,
    #[serde(default)]
    description: Option<String>,
    input_schema: Value,
    #[serde(default)]
    input_schema_hash: Option<String>,
    #[serde(default = "default_true")]
    strict_json_schema: bool,
    #[serde(flatten, default)]
    unknown: Unknown,
}

impl<'de> Deserialize<'de> for ToolSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolSchemaWire::deserialize(deserializer)?;
        validate_tool_name(&wire.name).map_err(D::Error::custom)?;
        let mut input_schema = wire.input_schema;
        canonicalize_json(&mut input_schema);
        let actual_hash = hash_schema(&input_schema).map_err(D::Error::custom)?;
        if wire
            .input_schema_hash
            .as_ref()
            .is_some_and(|expected| expected != &actual_hash)
        {
            return Err(D::Error::custom(format!(
                "tool schema hash mismatch for `{}`",
                wire.name
            )));
        }
        if wire.strict_json_schema {
            verify_strict_json_schema(&input_schema).map_err(D::Error::custom)?;
        }
        Ok(Self {
            schema_version: wire.schema_version,
            name: wire.name,
            description: wire.description,
            input_schema,
            input_schema_hash: actual_hash,
            strict_json_schema: wire.strict_json_schema,
            unknown: wire.unknown,
        })
    }
}

/// A decoded argument object whose concrete type is recorded and checked on downcast.
pub struct DecodedToolInput {
    type_name: &'static str,
    value: Box<dyn Any + Send + Sync>,
}

impl DecodedToolInput {
    /// Concrete Rust type recorded by the schema-bound decoder.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        self.type_name
    }

    /// Borrows the decoded value when `T` is the schema's input type.
    #[must_use]
    pub fn downcast_ref<T: ToolInput>(&self) -> Option<&T> {
        self.value.downcast_ref()
    }

    /// Takes ownership of the decoded value with a checked type boundary.
    pub fn downcast<T: ToolInput>(self) -> Result<T> {
        self.value.downcast::<T>().map(|value| *value).map_err(|_| {
            Error::caller(format!(
                "decoded tool input `{}` cannot be downcast to `{}`",
                self.type_name,
                core::any::type_name::<T>()
            ))
        })
    }
}

impl fmt::Debug for DecodedToolInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedToolInput")
            .field("type_name", &self.type_name)
            .field("value", &"<decoded-tool-input>")
            .finish()
    }
}

type ArgumentDecoder = fn(Value) -> std::result::Result<DecodedToolInput, ToolArgumentDecodeError>;

/// A model-safe diagnostic produced while decoding a tool argument object.
///
/// This deliberately does not use the framework [`Error`]. Framework errors are for hosts and
/// logs and may be localized; tools with custom failure handling can use this value to produce
/// their own model-facing explanation without copying framework prose into a tool result.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolArgumentDecodeError {
    /// The provider payload could not be parsed as JSON.
    InvalidJson {
        /// Parser diagnostic.
        message: String,
    },
    /// The parsed JSON value does not satisfy the declared input schema.
    Shape(ArgumentShapeViolation),
    /// The schema accepted the value but the Rust input type rejected it.
    Deserialize {
        /// Rust type bound to this decoder.
        input_type: &'static str,
        /// Serde's direct diagnostic.
        message: String,
    },
}

impl fmt::Display for ToolArgumentDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson { message } => {
                write!(formatter, "arguments are not valid JSON: {message}")
            }
            Self::Shape(violation) => violation.fmt(formatter),
            Self::Deserialize {
                input_type,
                message,
            } => write!(
                formatter,
                "arguments do not match `{input_type}`: {message}"
            ),
        }
    }
}

impl std::error::Error for ToolArgumentDecodeError {}

/// One schema-level violation in a model argument object.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentShapeViolation {
    /// The schema is internally malformed at this location.
    InvalidSchema {
        /// JSON-path-like schema location.
        path: String,
        /// Concrete problem in the schema.
        message: String,
    },
    /// No member of an `anyOf` accepted the argument.
    NoMatchingVariant {
        /// JSON-path-like argument location.
        path: String,
    },
    /// The argument has a different JSON type than the schema expects.
    UnexpectedType {
        /// JSON-path-like argument location.
        path: String,
        /// JSON type supplied by the model.
        actual: String,
        /// JSON type or types accepted by the schema.
        expected: String,
    },
    /// A schema with object properties received a non-object value.
    ExpectedObject {
        /// JSON-path-like argument location.
        path: String,
    },
    /// A required argument is absent.
    MissingRequired {
        /// JSON-path-like argument location.
        path: String,
    },
    /// The model supplied a property forbidden by the schema.
    UnknownArgument {
        /// JSON-path-like argument location.
        path: String,
    },
}

impl fmt::Display for ArgumentShapeViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema { path, message } => {
                write!(formatter, "schema node at `{path}` is invalid: {message}")
            }
            Self::NoMatchingVariant { path } => {
                write!(
                    formatter,
                    "argument `{path}` does not match any allowed schema variant"
                )
            }
            Self::UnexpectedType {
                path,
                actual,
                expected,
            } => write!(
                formatter,
                "argument `{path}` has type {actual}, expected {expected}"
            ),
            Self::ExpectedObject { path } => {
                write!(formatter, "argument `{path}` must be an object")
            }
            Self::MissingRequired { path } => {
                write!(formatter, "required argument `{path}` is missing")
            }
            Self::UnknownArgument { path } => write!(formatter, "unknown argument `{path}`"),
        }
    }
}

/// Function schema with its argument decoder and runtime signature metadata.
#[non_exhaustive]
#[derive(Clone)]
pub struct FuncSchema {
    schema_version: SchemaVersion,
    tool_schema: ToolSchema,
    input_type_name: &'static str,
    inject_tool_context: bool,
    return_type_name: Option<&'static str>,
    decoder: ArgumentDecoder,
}

impl FuncSchema {
    /// Builds schema and decoder from the same concrete [`ToolInput`] type.
    pub fn for_input<T: ToolInput>(name: impl Into<String>) -> Result<Self> {
        Ok(Self {
            schema_version: FUNC_SCHEMA_VERSION,
            tool_schema: T::tool_schema(name)?,
            input_type_name: core::any::type_name::<T>(),
            inject_tool_context: false,
            return_type_name: None,
            decoder: decode_arguments::<T>,
        })
    }

    /// Records whether the eventual callable receives runtime `ToolContext` separately.
    #[must_use]
    pub const fn with_tool_context(mut self, inject: bool) -> Self {
        self.inject_tool_context = inject;
        self
    }

    /// Records the callable's concrete return type without imposing an output codec yet.
    #[must_use]
    pub fn with_return_type<T: 'static>(mut self) -> Self {
        self.return_type_name = Some(core::any::type_name::<T>());
        self
    }

    /// Decodes one model argument JSON string using the type that generated the schema.
    pub fn decode_arguments(&self, arguments: &str) -> Result<DecodedToolInput> {
        self.decode_arguments_diagnostic(arguments)
            .map_err(|error| self.decode_error(error))
    }

    /// Decodes one already-parsed model argument value using the type that generated the schema.
    ///
    /// Providers normalize arguments before dispatch, so the common invocation entry must not
    /// serialize a value merely to parse it again. The string form remains for adapters that have
    /// not parsed their wire payload yet; both paths share the same schema validation and typed
    /// decoder.
    pub fn decode_value(&self, value: Value) -> Result<DecodedToolInput> {
        self.decode_value_diagnostic(value)
            .map_err(|error| self.decode_error(error))
    }

    /// Decodes one model argument JSON string and retains a model-safe diagnostic on failure.
    ///
    /// Tools with custom failure handling should use this method rather than rendering a framework
    /// [`Error`]. Its diagnostics are direct parser or schema facts, not host-facing framework
    /// prose.
    pub fn decode_arguments_diagnostic(
        &self,
        arguments: &str,
    ) -> std::result::Result<DecodedToolInput, ToolArgumentDecodeError> {
        let value = serde_json::from_str(arguments).map_err(|error| {
            ToolArgumentDecodeError::InvalidJson {
                message: error.to_string(),
            }
        })?;
        self.decode_value_diagnostic(value)
    }

    /// Decodes one parsed model argument object and retains a model-safe diagnostic on failure.
    ///
    /// This is the custom failure-handling counterpart to [`Self::decode_value`]. It preserves
    /// structured schema violations and direct serde diagnostics without exposing framework error
    /// formatting to the model.
    pub fn decode_value_diagnostic(
        &self,
        value: Value,
    ) -> std::result::Result<DecodedToolInput, ToolArgumentDecodeError> {
        validate_argument_shape(&value, self.tool_schema.input_schema())?;
        (self.decoder)(value)
    }

    fn decode_error(&self, error: ToolArgumentDecodeError) -> Error {
        Error::tool(
            ToolErrorKind::InvalidInput,
            self.tool_schema.name(),
            error.to_string(),
        )
        .with_source(error)
    }

    /// Boundary schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Model-facing schema.
    #[must_use]
    pub const fn tool_schema(&self) -> &ToolSchema {
        &self.tool_schema
    }

    /// Fully-qualified Rust input type.
    #[must_use]
    pub const fn input_type_name(&self) -> &'static str {
        self.input_type_name
    }

    /// Whether runtime context is injected outside model arguments.
    #[must_use]
    pub const fn inject_tool_context(&self) -> bool {
        self.inject_tool_context
    }

    /// Fully-qualified Rust return type, when recorded.
    #[must_use]
    pub const fn return_type_name(&self) -> Option<&'static str> {
        self.return_type_name
    }
}

impl fmt::Debug for FuncSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FuncSchema")
            .field("schema_version", &self.schema_version)
            .field("tool_schema", &self.tool_schema)
            .field("input_type_name", &self.input_type_name)
            .field("inject_tool_context", &self.inject_tool_context)
            .field("return_type_name", &self.return_type_name)
            .field("decoder", &"<schema-bound-decoder>")
            .finish()
    }
}

fn decode_arguments<T: ToolInput>(
    arguments: Value,
) -> std::result::Result<DecodedToolInput, ToolArgumentDecodeError> {
    let value = serde_json::from_value::<T>(arguments).map_err(|error| {
        ToolArgumentDecodeError::Deserialize {
            input_type: core::any::type_name::<T>(),
            message: error.to_string(),
        }
    })?;
    Ok(DecodedToolInput {
        type_name: core::any::type_name::<T>(),
        value: Box::new(value),
    })
}

fn validate_argument_shape(
    value: &Value,
    schema: &Value,
) -> std::result::Result<(), ToolArgumentDecodeError> {
    validate_schema_node(value, schema, schema, "$").map_err(ToolArgumentDecodeError::Shape)
}

fn validate_schema_node(
    value: &Value,
    schema: &Value,
    root: &Value,
    path: &str,
) -> core::result::Result<(), ArgumentShapeViolation> {
    let object = schema
        .as_object()
        .ok_or_else(|| ArgumentShapeViolation::InvalidSchema {
            path: path.to_owned(),
            message: "it is not an object".to_owned(),
        })?;

    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let resolved = resolve_schema_ref(root, reference).ok_or_else(|| {
            ArgumentShapeViolation::InvalidSchema {
                path: path.to_owned(),
                message: format!("reference `{reference}` cannot be resolved"),
            }
        })?;
        validate_schema_node(value, resolved, root, path)?;
    }

    // Sibling keywords still apply after a variant matches, so this does not short-circuit the
    // rest of the node the way an early return would.
    if let Some(variants) = object.get("anyOf").and_then(Value::as_array)
        && !variants
            .iter()
            .any(|variant| validate_schema_node(value, variant, root, path).is_ok())
    {
        return Err(ArgumentShapeViolation::NoMatchingVariant {
            path: path.to_owned(),
        });
    }

    if let Some(variants) = object.get("allOf").and_then(Value::as_array) {
        for variant in variants {
            validate_schema_node(value, variant, root, path)?;
        }
    }

    if let Some(kind) = object.get("type")
        && !matches_schema_type(value, kind)
    {
        return Err(ArgumentShapeViolation::UnexpectedType {
            path: path.to_owned(),
            actual: value_type(value).to_owned(),
            expected: schema_type_label(kind),
        });
    }

    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        let input = value
            .as_object()
            .ok_or_else(|| ArgumentShapeViolation::ExpectedObject {
                path: path.to_owned(),
            })?;
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !input.contains_key(name) {
                    return Err(ArgumentShapeViolation::MissingRequired {
                        path: format!("{path}.{name}"),
                    });
                }
            }
        }
        if object.get("additionalProperties") == Some(&Value::Bool(false)) {
            for name in input.keys() {
                if !properties.contains_key(name) {
                    return Err(ArgumentShapeViolation::UnknownArgument {
                        path: format!("{path}.{name}"),
                    });
                }
            }
        }
        for (name, property_schema) in properties {
            if let Some(property_value) = input.get(name) {
                validate_schema_node(
                    property_value,
                    property_schema,
                    root,
                    &format!("{path}.{name}"),
                )?;
            }
        }
    }

    if let Some(values) = value.as_array() {
        // Tuples are described positionally (`items` as an array, or draft 2020-12 `prefixItems`);
        // homogeneous sequences use a single `items` schema for every element.
        let positional = object
            .get("prefixItems")
            .or_else(|| object.get("items"))
            .and_then(Value::as_array);
        if let Some(schemas) = positional {
            for (index, (item, item_schema)) in values.iter().zip(schemas).enumerate() {
                validate_schema_node(item, item_schema, root, &format!("{path}[{index}]"))?;
            }
        } else if let Some(items) = object.get("items").filter(|items| items.is_object()) {
            for (index, item) in values.iter().enumerate() {
                validate_schema_node(item, items, root, &format!("{path}[{index}]"))?;
            }
        }
    }
    Ok(())
}

fn resolve_schema_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let path = reference.strip_prefix("#/")?;
    let mut current = root;
    for segment in path.split('/') {
        current = current.get(segment.replace("~1", "/").replace("~0", "~"))?;
    }
    Some(current)
}

fn matches_schema_type(value: &Value, kind: &Value) -> bool {
    match kind {
        Value::String(kind) => matches_one_type(value, kind),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| matches_one_type(value, kind)),
        _ => true,
    }
}

fn matches_one_type(value: &Value, kind: &str) -> bool {
    match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value
            .as_number()
            .is_some_and(|number| number.is_i64() || number.is_u64()),
        "string" => value.is_string(),
        _ => true,
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn schema_type_label(kind: &Value) -> String {
    match kind {
        Value::String(kind) => kind.clone(),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "the declared schema type".to_owned(),
    }
}

fn hash_schema(schema: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(schema).map_err(|error| {
        Error::config("failed to render canonical tool schema").with_source(error)
    })?;
    let digest = Sha256::digest(bytes);
    Ok(format!("{digest:x}"))
}

const fn default_true() -> bool {
    true
}
