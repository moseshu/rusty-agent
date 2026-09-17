//! The discriminated-payload mechanism three sandbox types share.
//!
//! Client options, snapshots and session states are each an open family: the reference declares a
//! base with a `type` field, every backend subclasses it, and a payload is routed back to the right
//! subclass by that field. All three implement the same four rules, three times over — a subclass
//! must declare a non-empty `type`, a type may be claimed once, an unknown type is refused rather
//! than guessed at, and serialization always emits the discriminator even when nothing else was set.
//!
//! Here they are one mechanism used three times. The duplication upstream is not a contract; the
//! four rules are.
//!
//! # Why a payload rather than a trait object
//!
//! Rust has no open subclassing, and a closed enum of built-in backends would do the one thing the
//! reference is careful to allow: shut out a third-party client. So a parsed value keeps its
//! discriminator and carries every other field verbatim. A backend that knows the type reads its
//! own fields out; a backend that does not still round-trips them without loss, which is what a
//! session state written by one process and resumed by another depends on.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value};

/// The key every discriminated payload carries.
pub const TYPE_FIELD: &str = "type";

/// Which family a registry serves, and therefore how its refusals read.
///
/// The wording is the reference's, because these strings reach a host: a message that says
/// "snapshot" when a session state was rejected sends someone to the wrong file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryKind {
    /// The family's name as it appears mid-sentence, such as `sandbox client options`.
    noun: &'static str,
    /// The base type's name, named in the message about an unusable payload.
    base: &'static str,
    /// Whether a refusal may quote the discriminator it could not route.
    echoes_type: bool,
}

impl RegistryKind {
    /// Names a family that may quote a rejected discriminator back to the caller.
    #[must_use]
    pub const fn new(noun: &'static str, base: &'static str) -> Self {
        Self {
            noun,
            base,
            echoes_type: true,
        }
    }

    /// Names a family whose refusals must not quote the payload.
    ///
    /// Session state is the case: the reference parses it inside a block that discards the payload
    /// and redacts the error before it escapes, because a state carries mount authority. Quoting
    /// even the discriminator would make this family's refusals the one place that echoes a payload
    /// the surrounding code went to some trouble not to.
    #[must_use]
    pub const fn redacted(noun: &'static str, base: &'static str) -> Self {
        Self {
            noun,
            base,
            echoes_type: false,
        }
    }

    /// Whether a refusal may quote the discriminator it could not route.
    #[must_use]
    pub const fn echoes_type(self) -> bool {
        self.echoes_type
    }

    /// The family's name as it appears mid-sentence.
    #[must_use]
    pub const fn noun(self) -> &'static str {
        self.noun
    }

    /// The base type's name.
    #[must_use]
    pub const fn base(self) -> &'static str {
        self.base
    }
}

/// An unvalidated payload with a discriminator and extensible fields.
///
/// Field order is not preserved; the map is ordered by key so two renderings of the same payload
/// compare equal. Construction and deserialization do not establish registry validation; callers
/// must use [`TypeRegistry::parse`] or [`TypeRegistry::readmit`] before consuming backend fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscriminatedPayload {
    type_name: String,
    fields: BTreeMap<String, Value>,
}

impl DiscriminatedPayload {
    /// Builds a payload of the given type with no other fields.
    #[must_use]
    pub fn new(type_name: impl Into<String>) -> Self {
        Self {
            type_name: type_name.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Sets one extension field, replacing any previous value.
    ///
    /// The reserved `type` key is ignored: only the constructor sets the discriminator.
    #[must_use]
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        let key = key.into();
        if key != TYPE_FIELD {
            self.fields.insert(key, value.into());
        }
        self
    }

    /// Which family member this is.
    #[must_use]
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Every field other than the discriminator.
    #[must_use]
    pub const fn fields(&self) -> &BTreeMap<String, Value> {
        &self.fields
    }

    /// Reads one field, or `None` when the payload does not carry it.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// Renders the payload, always including the discriminator.
    ///
    /// The reference emits `type` even under `exclude_unset`, because a payload that lost its
    /// discriminator cannot be routed back to anything.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = JsonMap::new();
        for (key, value) in &self.fields {
            map.insert(key.clone(), value.clone());
        }
        map.insert(TYPE_FIELD.to_owned(), Value::String(self.type_name.clone()));
        Value::Object(map)
    }
}

impl Serialize for DiscriminatedPayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DiscriminatedPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let Value::Object(map) = value else {
            return Err(serde::de::Error::custom("payload must be an object"));
        };
        let type_name = match map.get(TYPE_FIELD) {
            Some(Value::String(name)) => name.clone(),
            _ => {
                return Err(serde::de::Error::custom(
                    "payload must include a string `type`",
                ));
            }
        };
        let fields = map
            .into_iter()
            .filter(|(key, _)| key != TYPE_FIELD)
            .collect();
        Ok(Self { type_name, fields })
    }
}

/// Why a type could not be registered, or a payload could not be routed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A registration supplied an empty discriminator.
    #[error("{registrant} must define a non-empty string default for `type`")]
    EmptyType {
        /// What tried to register.
        registrant: String,
    },
    /// The discriminator is already claimed by something else.
    #[error("{noun} type `{type_name}` is already registered by {existing}")]
    AlreadyRegistered {
        /// The family, for the message.
        noun: &'static str,
        /// The discriminator being claimed.
        type_name: String,
        /// What already holds it.
        existing: String,
    },
    /// No registrant claims this discriminator.
    ///
    /// Refused rather than defaulted: falling back to a built-in backend would run the work
    /// somewhere other than where the payload said, which is worse than not running it.
    #[error("unknown {noun} type `{type_name}`")]
    UnknownType {
        /// The family, for the message.
        noun: &'static str,
        /// The discriminator that matched nothing.
        type_name: String,
    },
    /// No registrant claims the payload's discriminator, which this family does not quote.
    ///
    /// Contains no payload data, including in `Debug` output and error chains.
    #[error("unknown {noun} type")]
    UnknownTypeRedacted {
        /// The family, for the message.
        noun: &'static str,
    },
    /// The payload is not an object, so it has no discriminator to read.
    #[error("{noun} payload must be a {base} or object payload")]
    NotAPayload {
        /// The family, for the message.
        noun: &'static str,
        /// The base type a caller should supply instead.
        base: &'static str,
    },
    /// The payload is an object but its `type` is missing or not a string.
    ///
    /// Only families whose refusals are redacted raise this. The ones that quote the discriminator
    /// report a missing `type` as an unknown one, because that is the single refusal the reference
    /// gives both cases.
    #[error("{noun} payload must include a string `type`")]
    MissingType {
        /// The family, for the message.
        noun: &'static str,
    },
    /// The registrant that owns this type rejected the payload.
    #[error("{noun} payload is invalid for type `{type_name}`: {reason}")]
    InvalidPayload {
        /// The family, for the message.
        noun: &'static str,
        /// The discriminator whose owner refused.
        type_name: String,
        /// What the owner objected to.
        reason: String,
    },
    /// A validator rejected a payload whose contents must not escape through diagnostics.
    #[error("{noun} payload is invalid")]
    InvalidPayloadRedacted {
        /// The family, for the message.
        noun: &'static str,
    },
}

/// What a registrant does with a payload claimed by its type.
///
/// Returning the payload rather than validating in place lets an owner normalize — the reference's
/// port coercion is one such case — without the registry knowing any family's fields.
type Normalizer =
    Box<dyn Fn(DiscriminatedPayload) -> Result<DiscriminatedPayload, String> + Send + Sync>;

/// One registered family member.
struct Entry {
    /// What registered, named in a duplicate-registration refusal.
    registrant: String,
    normalize: Normalizer,
}

/// The open set of types one family accepts.
///
/// Registration is explicit: a host assembles the set it wants. Nothing registers itself by being
/// linked in, because then which backends exist would drift with whichever features happened to be
/// enabled, and the answer to "can this payload be resumed here" would differ between two builds of
/// the same program.
pub struct TypeRegistry {
    kind: RegistryKind,
    entries: BTreeMap<String, Entry>,
}

impl fmt::Debug for TypeRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TypeRegistry")
            .field("kind", &self.kind)
            .field("types", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl TypeRegistry {
    /// Opens an empty registry for one family.
    #[must_use]
    pub fn new(kind: RegistryKind) -> Self {
        Self {
            kind,
            entries: BTreeMap::new(),
        }
    }

    /// Claims a discriminator, accepting any payload that carries it.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::EmptyType`] for an empty discriminator and
    /// [`RegistryError::AlreadyRegistered`] when something else already holds it.
    pub fn register(
        &mut self,
        type_name: impl Into<String>,
        registrant: impl Into<String>,
    ) -> Result<(), RegistryError> {
        self.register_with(type_name, registrant, Ok)
    }

    /// Claims a discriminator, normalizing every payload that carries it.
    ///
    /// # Errors
    ///
    /// As [`Self::register`].
    pub fn register_with(
        &mut self,
        type_name: impl Into<String>,
        registrant: impl Into<String>,
        normalize: impl Fn(DiscriminatedPayload) -> Result<DiscriminatedPayload, String>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), RegistryError> {
        let type_name = type_name.into();
        let registrant = registrant.into();

        if type_name.is_empty() {
            return Err(RegistryError::EmptyType { registrant });
        }
        if let Some(existing) = self.entries.get(&type_name) {
            // Re-registering the same owner is the reference's tolerated case; a different owner
            // claiming a live type is what silently reroutes payloads.
            if existing.registrant == registrant {
                return Ok(());
            }
            return Err(RegistryError::AlreadyRegistered {
                noun: self.kind.noun(),
                type_name,
                existing: existing.registrant.clone(),
            });
        }

        self.entries.insert(
            type_name,
            Entry {
                registrant,
                normalize: Box::new(normalize),
            },
        );
        Ok(())
    }

    /// Whether this discriminator is claimed.
    #[must_use]
    pub fn is_registered(&self, type_name: &str) -> bool {
        self.entries.contains_key(type_name)
    }

    /// Every claimed discriminator, in order.
    pub fn registered_types(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Routes a JSON payload to its owner and returns what the owner made of it.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::NotAPayload`] when the value is not an object,
    /// [`RegistryError::MissingType`] when it carries no string discriminator,
    /// [`RegistryError::UnknownType`] when nothing claims that discriminator, and
    /// [`RegistryError::InvalidPayload`] when the owner refuses it.
    pub fn parse(&self, value: &Value) -> Result<DiscriminatedPayload, RegistryError> {
        let Value::Object(map) = value else {
            return Err(RegistryError::NotAPayload {
                noun: self.kind.noun(),
                base: self.kind.base(),
            });
        };
        // A missing or non-string discriminator is not its own failure for the families that quote
        // it: the reference funnels both into the same "unknown type" refusal, so a caller branching
        // on the error sees one case rather than two. Only the redacted family, which cannot quote
        // the value, needs to say separately that the field was not there.
        let Some(Value::String(type_name)) = map.get(TYPE_FIELD) else {
            if !self.kind.echoes_type() {
                return Err(RegistryError::MissingType {
                    noun: self.kind.noun(),
                });
            }
            let rendered = map.get(TYPE_FIELD).map_or_else(
                || "null".to_owned(),
                |value| match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                },
            );
            return Err(self.unknown_type(rendered));
        };

        let payload = DiscriminatedPayload {
            type_name: type_name.clone(),
            fields: map
                .iter()
                .filter(|(key, _)| key.as_str() != TYPE_FIELD)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        };

        self.readmit(payload)
    }

    /// Validates and normalizes a payload using this registry's current owner.
    ///
    /// Unlike an upstream concrete model instance, this payload can be constructed, edited, or
    /// deserialized without validation. Re-run the same checks as JSON parsing rather than treating
    /// the discriminator alone as evidence that backend fields have been validated.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`].
    pub fn readmit(
        &self,
        payload: DiscriminatedPayload,
    ) -> Result<DiscriminatedPayload, RegistryError> {
        let type_name = payload.type_name.clone();
        let entry = self
            .entries
            .get(&type_name)
            .ok_or_else(|| self.unknown_type(type_name.clone()))?;
        let normalized = (entry.normalize)(payload)
            .map_err(|reason| self.invalid_payload(type_name.clone(), reason))?;
        if normalized.type_name() != type_name {
            return Err(self.invalid_payload(
                type_name,
                "normalizer must preserve the registered discriminator".to_owned(),
            ));
        }
        Ok(normalized)
    }

    /// Builds the refusal for an unclaimed discriminator, quoting it only where the family allows.
    fn unknown_type(&self, type_name: String) -> RegistryError {
        if self.kind.echoes_type() {
            RegistryError::UnknownType {
                noun: self.kind.noun(),
                type_name,
            }
        } else {
            RegistryError::UnknownTypeRedacted {
                noun: self.kind.noun(),
            }
        }
    }

    /// Discards both the discriminator and the validator's diagnostic for redacted families.
    fn invalid_payload(&self, type_name: String, reason: String) -> RegistryError {
        if self.kind.echoes_type() {
            RegistryError::InvalidPayload {
                noun: self.kind.noun(),
                type_name,
                reason,
            }
        } else {
            RegistryError::InvalidPayloadRedacted {
                noun: self.kind.noun(),
            }
        }
    }
}

/// The family that routes sandbox client options.
#[must_use]
pub fn client_options_kind() -> RegistryKind {
    RegistryKind::new("sandbox client options", "BaseSandboxClientOptions")
}

/// The family that routes snapshots.
#[must_use]
pub fn snapshot_kind() -> RegistryKind {
    RegistryKind::new("snapshot", "SnapshotBase")
}

/// The family that routes session states.
#[must_use]
pub fn session_state_kind() -> RegistryKind {
    RegistryKind::redacted("sandbox session state", "SandboxSessionState")
}
