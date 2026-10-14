//! Output declarations and decoded values shared by agents and the runner.

use crate::{
    error::{Error, Result},
    model::ModelOutputSchema,
    strict::{ensure_strict_json_schema, verify_strict_json_schema},
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    any::{Any, TypeId},
    fmt,
    marker::PhantomData,
    sync::{Arc, OnceLock},
};

mod coercion;
const OUTPUT_SCHEMA_LABEL: &str = "agent output schema";
/// Property wrapping output types that are not JSON objects.
pub const OUTPUT_WRAPPER_KEY: &str = "response";

/// An output decoded before delivery, with a JSON projection for routing and presentation.
///
/// Typed declarations retain the actual Rust object. Reading a result never repeats decoding.
/// This is an in-process value; checkpoints retain model items and rebind declarations on resume.
#[derive(Clone)]
pub struct OutputValue {
    value: Value,
    decoded: Arc<dyn Any + Send + Sync>,
    plain_text: bool,
}

impl OutputValue {
    /// Creates ordinary text.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            value: Value::String(text.clone()),
            decoded: Arc::new(text),
            plain_text: true,
        }
    }
    /// Creates JSON, including values promoted directly from tools.
    #[must_use]
    pub fn json(value: Value) -> Self {
        Self {
            decoded: Arc::new(value.clone()),
            value,
            plain_text: false,
        }
    }
    /// Retains a decoded Rust object and its serializable projection.
    pub fn typed<T: Serialize + Send + Sync + 'static>(value: T) -> Result<Self> {
        let projected = serde_json::to_value(&value).map_err(|error| {
            Error::caller("the decoded output cannot be serialized").with_source(error)
        })?;
        Ok(Self {
            value: projected,
            decoded: Arc::new(value),
            plain_text: false,
        })
    }
    /// Unwrapped JSON projection.
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.value
    }
    /// Borrows the already decoded object if its exact Rust type matches.
    #[must_use]
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.decoded.downcast_ref()
    }
    /// Whether this output is ordinary text.
    #[must_use]
    pub const fn is_plain_text(&self) -> bool {
        self.plain_text
    }
    /// Plain text verbatim, otherwise unwrapped JSON for presentation.
    #[must_use]
    pub fn text_content(&self) -> String {
        if self.plain_text {
            self.value.as_str().unwrap_or_default().to_owned()
        } else {
            self.value.to_string()
        }
    }
}

impl fmt::Debug for OutputValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputValue")
            .field("plain_text", &self.plain_text)
            .finish_non_exhaustive()
    }
}

/// Custom business validation, corresponding to the reference's output-schema adapter.
///
/// The decoder may return a value different from the wire JSON. Reject invalid model data with
/// an invalid-final-output error; provider strictness never replaces business validation.
pub trait OutputDecoder: Send + Sync + 'static {
    /// Validates a payload and produces its host-facing object.
    fn decode(&self, value: Value, strict: bool) -> Result<OutputValue>;
}

struct TypedDecoder<T> {
    schema: Value,
    marker: PhantomData<T>,
}
impl<T: DeserializeOwned + Serialize + Send + Sync + 'static> OutputDecoder for TypedDecoder<T> {
    fn decode(&self, mut value: Value, strict: bool) -> Result<OutputValue> {
        if !strict {
            coercion::coerce(&mut value, &self.schema);
        }
        let decoded: T = serde_json::from_value(value).map_err(|error| {
            Error::invalid_final_output("Invalid JSON for the declared Rust output type")
                .with_source(error)
        })?;
        OutputValue::typed(decoded)
    }
}

type CachedValidator = Arc<OnceLock<std::result::Result<jsonschema::Validator, String>>>;

/// Output contract: schema, wrapping, validation, and decoding.
///
/// Plain text is the default. Generated schemas use the function-input strict normalizer.
/// Handwritten schemas are verified without rewriting. Typed decoding uses Serde's field/default
/// semantics, as Python uses its type adapter rather than revalidating the provider schema.
#[derive(Clone)]
pub struct OutputSchema {
    model_schema: Option<ModelOutputSchema>,
    wrapped: bool,
    decoder: Option<Arc<dyn OutputDecoder>>,
    decoder_type: Option<TypeId>,
    validator: Option<CachedValidator>,
}

impl OutputSchema {
    /// Declares ordinary text.
    #[must_use]
    pub const fn plain_text() -> Self {
        Self {
            model_schema: None,
            wrapped: false,
            decoder: None,
            decoder_type: None,
            validator: None,
        }
    }
    /// Declares handwritten JSON schema, strict by default and validated exactly as supplied.
    #[must_use]
    pub fn json_schema(name: impl Into<String>, schema: Value) -> Self {
        Self {
            model_schema: Some(ModelOutputSchema::new(name, schema).with_strict(true)),
            validator: Some(Arc::new(OnceLock::new())),
            ..Self::plain_text()
        }
    }
    /// Generates a strict declaration and decoder from a Rust type.
    ///
    /// `String` means plain text. Structs and maps remain objects. Scalars, sequences, tuples,
    /// options, and unions use a `response` wrapper, removed before decoding. Open maps need
    /// loose mode, matching the reference.
    pub fn for_type<T: JsonSchema + DeserializeOwned + Serialize + Send + Sync + 'static>()
    -> Result<Self> {
        Self::for_type_with_strict::<T>(true)
    }
    /// Generates a declaration with explicit strictness.
    ///
    /// Loose mode preserves the original schema and accepts common scalar coercions. Serde
    /// attributes and custom deserializers remain authoritative for Rust-specific representations.
    pub fn for_type_with_strict<
        T: JsonSchema + DeserializeOwned + Serialize + Send + Sync + 'static,
    >(
        strict: bool,
    ) -> Result<Self> {
        if TypeId::of::<T>() == TypeId::of::<String>() {
            return Ok(Self::plain_text());
        }
        let mut schema = serde_json::to_value(schemars::schema_for!(T))
            .map_err(|error| Error::config("cannot generate output schema").with_source(error))?;
        if let Some(object) = schema.as_object_mut() {
            object.remove("$schema");
        }
        let decoder_schema = schema.clone();
        let wrapped = schema.get("type").and_then(Value::as_str) != Some("object");
        if wrapped {
            // Preserve local reference paths by keeping definitions at the root.
            let definitions = schema
                .as_object_mut()
                .and_then(|object| object.remove("definitions"));
            schema = serde_json::json!({"type":"object", "properties":{OUTPUT_WRAPPER_KEY:schema}, "required":[OUTPUT_WRAPPER_KEY]});
            if let Some(definitions) = definitions {
                schema["definitions"] = definitions;
            }
        }
        if strict {
            ensure_strict_json_schema(&mut schema, OUTPUT_SCHEMA_LABEL)?;
        }
        let mut declaration = Self::json_schema(T::schema_name(), schema).with_strict(strict);
        declaration.wrapped = wrapped;
        declaration.decoder_type = Some(TypeId::of::<T>());
        declaration.decoder = Some(Arc::new(TypedDecoder::<T> {
            schema: decoder_schema,
            marker: PhantomData,
        }));
        declaration.validate()?;
        Ok(declaration)
    }
    /// Attaches custom validation to a handwritten declaration.
    #[must_use]
    pub fn with_decoder(mut self, decoder: Arc<dyn OutputDecoder>) -> Self {
        self.decoder = Some(decoder);
        self.decoder_type = None;
        self
    }
    /// Changes the provider strictness flag; plain text is unaffected.
    ///
    /// Use `for_type_with_strict(false)` to generate the original loose type schema.
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        if let Some(schema) = self.model_schema.take() {
            self.model_schema = Some(schema.with_strict(strict));
        }
        self
    }
    /// Checks names, strict invariants, and compilation of handwritten schemas at agent build.
    pub fn validate(&self) -> Result<()> {
        let Some(schema) = &self.model_schema else {
            return Ok(());
        };
        let name = schema.name();
        if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
            return Err(Error::config(format!(
                "{OUTPUT_SCHEMA_LABEL} name must be non-empty, trimmed, and contain no control characters"
            )));
        }
        if !schema.schema().is_object() {
            return Err(Error::config(format!(
                "the root {OUTPUT_SCHEMA_LABEL} must be a JSON object"
            )));
        }
        if schema.strict() {
            verify_strict_json_schema(schema.schema(), OUTPUT_SCHEMA_LABEL)?;
        }
        if self.decoder.is_none() {
            self.compiled_validator()?;
        }
        Ok(())
    }
    fn compiled_validator(&self) -> Result<&jsonschema::Validator> {
        let schema = self
            .json_schema_value()
            .ok_or_else(|| Error::caller("plain text has no JSON schema"))?;
        self.validator
            .as_ref()
            .ok_or_else(|| Error::caller("missing output validator"))?
            .get_or_init(|| {
                // Retrieval features are disabled; validation cannot fetch files or network resources.
                jsonschema::options()
                    .build(schema)
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .map_err(|error| Error::config(format!("invalid {OUTPUT_SCHEMA_LABEL}: {error}")))
    }
    /// Validates final text and returns the decoded output before delivery.
    pub fn validate_json(&self, text: &str) -> Result<OutputValue> {
        if self.is_plain_text() {
            return Ok(OutputValue::text(text));
        }
        let mut value: Value = serde_json::from_str(text)
            .map_err(|error| Error::invalid_final_output("Invalid JSON").with_source(error))?;
        if self.wrapped {
            value = value
                .as_object_mut()
                .and_then(|object| object.remove(OUTPUT_WRAPPER_KEY))
                .ok_or_else(|| {
                    Error::invalid_final_output("Expected an object containing response")
                })?;
        }
        if let Some(decoder) = &self.decoder {
            return decoder.decode(value, self.strict().unwrap_or(false));
        }
        self.compiled_validator()?
            .validate(&value)
            .map_err(|error| {
                Error::invalid_final_output(format!("Invalid JSON at {}", error.instance_path()))
            })?;
        Ok(OutputValue::json(value))
    }
    /// Serializes a host value for explicit recovery, adding a wrapper when required.
    ///
    /// An already wrapped object is accepted, matching the reference's error-handler behavior.
    pub fn format_output<T: Serialize>(&self, output: &T) -> Result<String> {
        let mut value = serde_json::to_value(output)
            .map_err(|error| Error::caller("cannot serialize handler output").with_source(error))?;
        if self.is_plain_text() {
            return Ok(match value {
                Value::String(text) => text,
                other => other.to_string(),
            });
        }
        if self.wrapped
            && !value
                .as_object()
                .is_some_and(|object| object.contains_key(OUTPUT_WRAPPER_KEY))
        {
            value = serde_json::json!({OUTPUT_WRAPPER_KEY:value});
        }
        Ok(value.to_string())
    }
    /// Whether this declaration requests ordinary text.
    #[must_use]
    pub const fn is_plain_text(&self) -> bool {
        self.model_schema.is_none()
    }
    /// Whether wire JSON wraps the value in a `response` object.
    #[must_use]
    pub const fn is_wrapped(&self) -> bool {
        self.wrapped
    }
    /// Provider-required schema name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.model_schema.as_ref().map(ModelOutputSchema::name)
    }
    /// Provider-neutral schema, including the wire wrapper.
    #[must_use]
    pub const fn json_schema_value(&self) -> Option<&Value> {
        match &self.model_schema {
            Some(schema) => Some(schema.schema()),
            None => None,
        }
    }
    /// Provider strictness for structured JSON.
    #[must_use]
    pub const fn strict(&self) -> Option<bool> {
        match &self.model_schema {
            Some(schema) => Some(schema.strict()),
            None => None,
        }
    }
    /// Projects the declaration into a model request.
    #[must_use]
    pub fn to_model_output_schema(&self) -> Option<ModelOutputSchema> {
        self.model_schema.clone()
    }
}

impl PartialEq for OutputSchema {
    fn eq(&self, other: &Self) -> bool {
        let decoders_equal = match (&self.decoder, &other.decoder) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                self.decoder_type
                    .zip(other.decoder_type)
                    .is_some_and(|(left, right)| left == right)
                    || Arc::ptr_eq(left, right)
            }
            _ => false,
        };
        self.model_schema == other.model_schema && self.wrapped == other.wrapped && decoders_equal
    }
}
impl Default for OutputSchema {
    fn default() -> Self {
        Self::plain_text()
    }
}
impl fmt::Debug for OutputSchema {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputSchema")
            .field("name", &self.name())
            .field("strict", &self.strict())
            .field("wrapped", &self.wrapped)
            .finish_non_exhaustive()
    }
}
