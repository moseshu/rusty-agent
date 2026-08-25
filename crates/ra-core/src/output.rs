//! Agent-level output declarations.
//!
//! [`OutputSchema`] describes the form an agent promises to produce. It deliberately owns the
//! declaration above the provider request layer: adapters receive a [`ModelOutputSchema`] only
//! after the runtime projects this value for one call. Parsing into a host-facing output value and
//! validating final messages are later parts of the structured-output contract.

use std::fmt;

use serde_json::Value;

use crate::{
    error::{Error, Result},
    model::ModelOutputSchema,
    strict::verify_strict_json_schema,
};

/// Names this declaration in the shared strict-mode diagnostics.
const OUTPUT_SCHEMA_LABEL: &str = "agent output schema";

/// The output form declared by an agent.
///
/// The default is plain text, which produces no provider structured-output request. A JSON schema
/// declaration is strict by default, matching the provider behavior needed for a reliable output
/// contract. Call [`Self::with_strict`] with `false` only when the selected schema deliberately
/// relies on JSON-schema features that strict providers do not support.
#[derive(Clone, PartialEq)]
pub struct OutputSchema {
    model_schema: Option<ModelOutputSchema>,
}

impl OutputSchema {
    /// Declares ordinary plain-text output.
    #[must_use]
    pub const fn plain_text() -> Self {
        Self { model_schema: None }
    }

    /// Declares JSON output conforming to a named JSON schema.
    ///
    /// The schema is requested in strict mode by default, and is kept exactly as supplied. It is
    /// checked, not rewritten, by [`Self::validate`] — the same split the tool surface already
    /// makes: a schema this framework generates from a Rust type is normalized into strict form,
    /// while one a caller wrote is only verified, because silently rewriting it would change a
    /// contract its author chose. Semantic validation of a model's JSON belongs to the later
    /// output-value contract, not here.
    #[must_use]
    pub fn json_schema(name: impl Into<String>, schema: Value) -> Self {
        Self {
            model_schema: Some(ModelOutputSchema::new(name, schema).with_strict(true)),
        }
    }

    /// Changes the strict-mode request for a JSON-schema declaration.
    ///
    /// Plain-text output has no JSON-schema strictness, so this leaves it unchanged.
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        if let Some(model_schema) = self.model_schema.take() {
            self.model_schema = Some(model_schema.with_strict(strict));
        }
        self
    }

    /// Checks that this declaration can be requested from a provider at all.
    ///
    /// [`AgentSpecBuilder::build`](crate::agent::AgentSpecBuilder::build) runs this, and that
    /// placement is the point: nothing between the declaration and the wire inspects it again, so
    /// a name or a schema no provider accepts would otherwise be found by **every model call of
    /// every run** rather than once, by the code that wrote it. The tool surface already refuses to
    /// ship that failure shape, and an output declaration reaches the same providers.
    ///
    /// A strict declaration is verified against the strict-mode invariants rather than normalized
    /// into them; see [`Self::json_schema`]. Plain text declares nothing and is always valid.
    ///
    /// The name is held to the same bar as a model-facing tool name — non-empty, trimmed, no
    /// control characters. Per-provider charset and length limits stay out of both, so that every
    /// model-facing name in the framework answers to one rule.
    pub fn validate(&self) -> Result<()> {
        let Some(model_schema) = &self.model_schema else {
            return Ok(());
        };

        let name = model_schema.name();
        if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
            return Err(Error::config(format!(
                "{OUTPUT_SCHEMA_LABEL} name must be non-empty, trimmed, and contain no \
                 control characters"
            )));
        }
        if !model_schema.schema().is_object() {
            return Err(Error::config(format!(
                "the root {OUTPUT_SCHEMA_LABEL} must be a JSON object"
            )));
        }
        if model_schema.strict() {
            verify_strict_json_schema(model_schema.schema(), OUTPUT_SCHEMA_LABEL)?;
        }
        Ok(())
    }

    /// Whether this declaration requests ordinary text rather than structured JSON.
    #[must_use]
    pub const fn is_plain_text(&self) -> bool {
        self.model_schema.is_none()
    }

    /// Provider-required name of the JSON schema, when structured output is declared.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.model_schema.as_ref().map(ModelOutputSchema::name)
    }

    /// Provider-neutral JSON schema, when structured output is declared.
    #[must_use]
    pub const fn json_schema_value(&self) -> Option<&Value> {
        match &self.model_schema {
            Some(model_schema) => Some(model_schema.schema()),
            None => None,
        }
    }

    /// Whether structured JSON is requested in strict mode.
    #[must_use]
    pub const fn strict(&self) -> Option<bool> {
        match &self.model_schema {
            Some(model_schema) => Some(model_schema.strict()),
            None => None,
        }
    }

    /// Projects this declaration into the one-call provider-neutral request form.
    ///
    /// Plain text has no structured-output request representation, so it projects to `None`.
    /// This projection does not parse or validate a model's final text.
    #[must_use]
    pub fn to_model_output_schema(&self) -> Option<ModelOutputSchema> {
        self.model_schema.clone()
    }
}

impl Default for OutputSchema {
    fn default() -> Self {
        Self::plain_text()
    }
}

impl fmt::Debug for OutputSchema {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.model_schema {
            Some(schema) => formatter
                .debug_struct("OutputSchema")
                .field("kind", &"json_schema")
                .field("name", &schema.name())
                .field("strict", &schema.strict())
                .finish_non_exhaustive(),
            None => formatter
                .debug_struct("OutputSchema")
                .field("kind", &"plain_text")
                .finish_non_exhaustive(),
        }
    }
}
