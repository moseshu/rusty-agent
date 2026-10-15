//! Structured function outputs, independent of an agent's final output declaration.

use std::{collections::BTreeSet, fmt};

use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::{ToolOutput, ToolOutputBlock};
use crate::{
    error::{Error, Result},
    strict::ensure_strict_json_schema,
};

type OutputDecoder = fn(Value) -> Result<Value>;

/// A strict object schema and, for typed declarations, its Rust output decoder.
///
/// Handwritten schemas describe output to the provider and require an object at runtime;
/// they do not validate individual properties, matching the reference's low-level escape hatch.
#[derive(Clone)]
pub struct FunctionToolOutputSchema {
    schema: Value,
    decoder: Option<OutputDecoder>,
}

impl FunctionToolOutputSchema {
    /// Copies and normalizes a handwritten strict object schema.
    pub fn json_schema(mut schema: Value) -> Result<Self> {
        check_depth(&schema)?;
        if !is_object_schema(&schema) {
            return Err(Error::config(
                "Function tool output_json_schema must define a JSON object schema.",
            ));
        }
        normalize_object_type_arrays(&mut schema);
        // The shared normalizer requires an explicit object root. Resolve a pure root reference
        // first, retaining definitions and root annotations for nested references and advertising.
        if schema.get("type").is_none() {
            let target = object_root(&schema)
                .cloned()
                .ok_or_else(|| Error::config("Invalid function output reference"))?;
            let root = schema
                .as_object_mut()
                .ok_or_else(|| Error::config("Invalid function output schema"))?;
            root.remove("$ref");
            for (key, value) in target.as_object().into_iter().flatten() {
                root.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        ensure_strict_json_schema(&mut schema, "function tool output schema").map_err(|error| {
            Error::config(
                "Function tool output_json_schema must define a strict JSON object schema.",
            )
            .with_source(error)
        })?;
        Ok(Self {
            schema,
            decoder: None,
        })
    }

    /// Generates a strict object schema and validates outputs with Serde.
    ///
    /// Scalars and open maps are rejected; function outputs never use the agent output wrapper.
    pub fn for_type<T: JsonSchema + DeserializeOwned + Serialize + Send + Sync + 'static>()
    -> Result<Self> {
        let mut schema = serde_json::to_value(schemars::schema_for!(T)).map_err(|error| {
            Error::config("cannot generate function output schema").with_source(error)
        })?;
        if let Some(object) = schema.as_object_mut() {
            object.remove("$schema");
        }
        let mut output = Self::json_schema(schema).map_err(|error| {
            Error::config("Function tool output_type must define a strict JSON object schema.")
                .with_source(error)
        })?;
        output.decoder = Some(|value| {
            let decoded: T = serde_json::from_value(value).map_err(|error| {
                Error::config("Function tool output does not match its declared output schema.")
                    .with_source(error)
            })?;
            serde_json::to_value(decoded).map_err(|error| {
                Error::config("cannot serialize declared function output").with_source(error)
            })
        });
        Ok(output)
    }

    /// The normalized schema advertised for this tool.
    #[must_use]
    pub const fn json_schema_value(&self) -> &Value {
        &self.schema
    }

    pub(crate) const fn is_typed(&self) -> bool {
        self.decoder.is_some()
    }

    /// Validates a callable's return value before invocation hooks see it.
    ///
    /// A typed callable must return an object, whereas a custom error formatter may return JSON
    /// text, parsed later by [`Self::validate_output`].
    pub fn validate_return(&self, value: Value) -> Result<Value> {
        match self.decoder {
            Some(decode) => decode(value),
            None => Ok(value),
        }
    }

    /// Validates and renders one final output, including user-provided failure formatter output.
    pub fn validate_output(&self, output: &ToolOutput) -> Result<ToolOutput> {
        let [ToolOutputBlock::Text { text, .. }] = output.blocks() else {
            return Err(Error::config(
                "Function tool output schema requires a JSON object.",
            ));
        };
        let value = serde_json::from_str(text).map_err(|error| {
            let message = if self.decoder.is_some() {
                "Function tool output does not match its declared output schema."
            } else {
                "Function tool output schema requires a JSON object."
            };
            Error::config(message).with_source(error)
        })?;
        let value = self.validate_return(value)?;
        if !value.is_object() {
            return Err(Error::config(
                "Function tool output schema requires a JSON object.",
            ));
        }
        Ok(ToolOutput::text(value.to_string()).with_metadata(output.metadata().clone()))
    }
}

impl fmt::Debug for FunctionToolOutputSchema {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FunctionToolOutputSchema")
            .field("typed", &self.decoder.is_some())
            .finish_non_exhaustive()
    }
}

fn object_root(schema: &Value) -> Option<&Value> {
    let mut current = schema;
    let mut seen = BTreeSet::new();
    loop {
        if current
            .get("type")
            .is_some_and(|kind| kind == "object" || kind == &serde_json::json!(["object"]))
        {
            return Some(current);
        }
        let reference = current.get("$ref")?.as_str()?;
        if !reference.starts_with("#/") || !seen.insert(reference) {
            return None;
        }
        current = schema.pointer(&reference[1..])?;
    }
}

fn is_object_schema(schema: &Value) -> bool {
    object_root(schema).is_some()
}

fn check_depth(schema: &Value) -> Result<()> {
    let mut stack = vec![(schema, 1)];
    while let Some((value, depth)) = stack.pop() {
        if depth > 100 {
            return Err(Error::config(
                "JSON schema is too deeply nested to process safely. Simplify or flatten the schema.",
            ));
        }
        match value {
            Value::Object(object) => stack.extend(
                object
                    .values()
                    .filter(|value| value.is_object() || value.is_array())
                    .map(|value| (value, depth + 1)),
            ),
            Value::Array(array) => stack.extend(
                array
                    .iter()
                    .filter(|value| value.is_object() || value.is_array())
                    .map(|value| (value, depth + 1)),
            ),
            _ => (),
        }
    }
    Ok(())
}

fn normalize_object_type_arrays(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.get("type") == Some(&serde_json::json!(["object"])) {
                object.insert("type".to_owned(), Value::String("object".to_owned()));
            }
            for value in object.values_mut() {
                normalize_object_type_arrays(value);
            }
        }
        Value::Array(array) => {
            for value in array {
                normalize_object_type_arrays(value);
            }
        }
        _ => (),
    }
}
