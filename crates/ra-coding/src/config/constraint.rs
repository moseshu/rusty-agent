// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Validated values that retain the last valid state after a rejected update.

use std::fmt;
use std::sync::Arc;

use super::RequirementSource;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
/// A configured value or managed policy that violates a constraint.
pub enum ConstraintError {
    #[error(
        "invalid value for `{field_name}`: `{candidate}` is not in the allowed set {allowed} (set by {requirement_source})"
    )]
    /// A candidate is outside the allowed set selected by the named authority.
    InvalidValue {
        /// The constrained configuration field.
        field_name: &'static str,
        /// The rejected candidate rendered for diagnostics.
        candidate: String,
        /// The allowed set rendered for diagnostics.
        allowed: String,
        /// The authority imposing this restriction.
        requirement_source: RequirementSource,
    },

    #[error("To use model `{model}`, you need to use auto review.")]
    /// A model requires the auto-review approval path.
    AutoReviewRequired {
        /// The protected model identifier.
        model: String,
    },

    #[error("field `{field_name}` cannot be empty")]
    /// A required field has no usable value.
    EmptyField {
        /// The missing configuration field.
        field_name: String,
    },

    #[error("invalid rules in requirements (set by {requirement_source}): {reason}")]
    /// Managed execution rules could not be parsed.
    ExecPolicyParse {
        /// The authority imposing this restriction.
        requirement_source: RequirementSource,
        /// The parse failure reported by the underlying policy parser.
        reason: String,
    },

    #[error(
        "invalid requirement for MCP server `{server_name}` (set by {requirement_source}): {reason}"
    )]
    /// A named managed MCP matcher is invalid.
    McpServerRequirementParse {
        /// The MCP registration name.
        server_name: String,
        /// The authority imposing this restriction.
        requirement_source: RequirementSource,
        /// The parse failure reported by the underlying policy parser.
        reason: String,
    },
}

impl ConstraintError {
    /// Constructs a missing-field diagnostic.
    pub fn empty_field(field_name: impl Into<String>) -> Self {
        Self::EmptyField {
            field_name: field_name.into(),
        }
    }
}

/// A result whose error preserves the managed constraint diagnostic.
pub type ConstraintResult<T> = Result<T, ConstraintError>;

impl From<ConstraintError> for std::io::Error {
    fn from(err: ConstraintError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
    }
}

type ConstraintValidator<T> = dyn Fn(&T) -> ConstraintResult<()> + Send + Sync;
/// A `ConstraintNormalizer` is a function which transforms a value into another of the same type.
/// `Constrained` uses normalizers to transform values to satisfy constraints or enforce values.
type ConstraintNormalizer<T> = dyn Fn(T) -> T + Send + Sync;

#[derive(Clone)]
/// A value whose updates must satisfy its retained validator and optional normalizer.
pub struct Constrained<T> {
    value: T,
    validator: Arc<ConstraintValidator<T>>,
    normalizer: Option<Arc<ConstraintNormalizer<T>>>,
}

impl<T: Send + Sync> Constrained<T> {
    /// Validates the initial value before installing the validator.
    pub fn new(
        initial_value: T,
        validator: impl Fn(&T) -> ConstraintResult<()> + Send + Sync + 'static,
    ) -> ConstraintResult<Self> {
        let validator: Arc<ConstraintValidator<T>> = Arc::new(validator);
        validator(&initial_value)?;
        Ok(Self {
            value: initial_value,
            validator,
            normalizer: None,
        })
    }

    /// normalized creates a `Constrained` value with a normalizer function and a validator that allows any value.
    #[allow(
        clippy::similar_names,
        reason = "Retain the upstream normalizer terminology."
    )]
    pub fn normalized(
        initial_value: T,
        normalizer: impl Fn(T) -> T + Send + Sync + 'static,
    ) -> ConstraintResult<Self> {
        let validator: Arc<ConstraintValidator<T>> = Arc::new(|_| Ok(()));
        let normalizer: Arc<ConstraintNormalizer<T>> = Arc::new(normalizer);
        let normalized = normalizer(initial_value);
        validator(&normalized)?;
        Ok(Self {
            value: normalized,
            validator,
            normalizer: Some(normalizer),
        })
    }

    /// Creates an unconstrained value without normalization.
    pub fn allow_any(initial_value: T) -> Self {
        Self {
            value: initial_value,
            validator: Arc::new(|_| Ok(())),
            normalizer: None,
        }
    }

    /// Allows only values equal to the supplied initial value.
    pub fn allow_only(only_value: T) -> Self
    where
        T: Clone + fmt::Debug + PartialEq + 'static,
    {
        let allowed_value = only_value.clone();
        Self {
            value: only_value,
            validator: Arc::new(move |candidate| {
                if candidate == &allowed_value {
                    Ok(())
                } else {
                    Err(ConstraintError::InvalidValue {
                        field_name: "<unknown>",
                        candidate: format!("{candidate:?}"),
                        allowed: format!("[{allowed_value:?}]"),
                        requirement_source: RequirementSource::Unknown,
                    })
                }
            }),
            normalizer: None,
        }
    }

    /// Allow any value of T, using T's Default as the initial value.
    #[must_use]
    pub fn allow_any_from_default() -> Self
    where
        T: Default,
    {
        Self::allow_any(T::default())
    }

    /// Borrows the current validated value.
    pub fn get(&self) -> &T {
        &self.value
    }

    /// Copies the current validated value.
    pub fn value(&self) -> T
    where
        T: Copy,
    {
        self.value
    }

    /// Probes the validator without normalization or mutation.
    pub fn can_set(&self, candidate: &T) -> ConstraintResult<()> {
        (self.validator)(candidate)
    }

    /// Composes an additional validator onto the current constraint.
    ///
    /// The existing value must satisfy the combined validator before it is installed.
    pub fn add_validator(
        &mut self,
        validator: impl Fn(&T) -> ConstraintResult<()> + Send + Sync + 'static,
    ) -> ConstraintResult<()>
    where
        T: 'static,
    {
        let existing_validator = self.validator.clone();
        let combined_validator: Arc<ConstraintValidator<T>> = Arc::new(move |candidate| {
            existing_validator(candidate)?;
            validator(candidate)
        });

        combined_validator(&self.value)?;
        self.validator = combined_validator;
        Ok(())
    }

    /// Normalizes and validates a candidate before replacing the current value.
    pub fn set(&mut self, value: T) -> ConstraintResult<()> {
        let value = if let Some(normalizer) = &self.normalizer {
            normalizer(value)
        } else {
            value
        };
        (self.validator)(&value)?;
        self.value = value;
        Ok(())
    }
}

impl<T> std::ops::Deref for Constrained<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

#[allow(
    clippy::missing_fields_in_debug,
    reason = "Preserve the upstream value-only debug representation; closures have no Debug contract."
)]
impl<T: fmt::Debug> fmt::Debug for Constrained<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Constrained")
            .field("value", &self.value)
            .finish()
    }
}

impl<T: PartialEq> PartialEq for Constrained<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
