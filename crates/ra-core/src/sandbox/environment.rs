//! The environment a workspace is materialized with.
//!
//! Most environment variables are a name and a string. Some are a name and a *reference* to a value
//! that has to be fetched — from a secret store, a metadata service, somewhere that can fail and
//! take time. The reference models both, and the distinction is the reason this is not a
//! `map<string, string>`: a manifest that inlined the fetched values would have the secrets in it,
//! and a manifest is written down, passed around, and persisted.
//!
//! So a member is one of three shapes:
//!
//! - a plain string, for the ordinary case;
//! - an [`EnvValue`], which names *how* to get the value — `str` is built in, and a host registers
//!   whatever else it can resolve;
//! - an [`EnvEntry`], which wraps an `EnvValue` with a description and whether it survives a stop.
//!
//! [`Environment::normalized`] flattens all three into entries, and [`Environment::resolve`] turns
//! them into the strings a process actually gets.
//!
//! # Resolving is the host's, and a failure cancels the rest
//!
//! An `EnvValue` this crate does not model is resolved by a host-supplied [`EnvValueResolver`] —
//! user code that may reach a network. When one lookup fails the others have to stop: the manifest
//! has already failed, and leaving sibling fetches in flight means credentials still arriving for a
//! workspace that will never exist. The reference needed a dedicated gather for that. Here it falls
//! out of dropping the combinator, which cancels every future still pending.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::future::try_join_all;
use serde::Serialize;
use serde_json::{Map as JsonMap, Value};

use super::error::{ErrorCode, OpName, SandboxError};
use super::registry::{DiscriminatedPayload, RegistryError, RegistryKind, TypeRegistry};

/// The discriminator of a value that is simply written down.
pub const STR_ENV_VALUE_TYPE: &str = "str";

/// The family that routes environment values.
#[must_use]
pub fn env_value_kind() -> RegistryKind {
    RegistryKind::new("env value", "EnvValue")
}

/// A registry holding the environment value kinds this crate models.
///
/// Only `str`. Everything else is a host's: what a value reference means — which vault, which
/// metadata service — is not something a protocol crate can know.
#[must_use]
pub fn builtin_env_value_registry() -> TypeRegistry {
    let mut registry = TypeRegistry::new(env_value_kind());
    let registered = registry.register_with(STR_ENV_VALUE_TYPE, "StrEnvValue", |payload| {
        match payload.field("value") {
            Some(Value::String(_)) => Ok(payload),
            Some(_) => Err("`value` must be a string".to_owned()),
            None => Err("`value` is required".to_owned()),
        }
    });
    debug_assert!(registered.is_ok(), "a fresh registry has no `str` yet");
    registry
}

/// How to obtain one environment value.
///
/// Carries the discriminator and whatever fields the type it names needs. A value that is simply
/// written down is `str`; anything else is a reference a host resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvValue(DiscriminatedPayload);

impl EnvValue {
    /// A value written down in the manifest.
    #[must_use]
    pub fn literal(value: impl Into<String>) -> Self {
        Self(DiscriminatedPayload::new(STR_ENV_VALUE_TYPE).with_field("value", value.into()))
    }

    /// A reference of a type a host knows how to resolve.
    #[must_use]
    pub const fn reference(payload: DiscriminatedPayload) -> Self {
        Self(payload)
    }

    /// Which kind of value this is.
    #[must_use]
    pub fn type_name(&self) -> &str {
        self.0.type_name()
    }

    /// The value itself, when it is simply written down.
    ///
    /// `None` for a reference, which has to be resolved before there is a string to return.
    #[must_use]
    pub fn as_literal(&self) -> Option<&str> {
        if self.0.type_name() != STR_ENV_VALUE_TYPE {
            return None;
        }
        self.0.field("value").and_then(Value::as_str)
    }

    /// One of the reference's fields.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.0.field(key)
    }

    /// The payload underneath, discriminator included.
    #[must_use]
    pub const fn payload(&self) -> &DiscriminatedPayload {
        &self.0
    }

    /// Renders the value, discriminator included.
    #[must_use]
    pub fn to_json(&self) -> Value {
        self.0.to_json()
    }

    /// Reads a value, routing it to the type the registry says owns it.
    ///
    /// A mapping whose only field is a string `value` is read as a literal even without a
    /// discriminator. That shape predates the discriminator and still appears in manifests written
    /// by hand, so refusing it would reject configuration the reference accepts. A mapping that has
    /// *other* fields as well is ambiguous — it could be a literal or an entry — and is refused
    /// rather than guessed at.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the payload is not an object, carries no usable discriminator,
    /// or names a type nobody registered.
    pub fn parse(registry: &TypeRegistry, value: &Value) -> Result<Self, RegistryError> {
        let Value::Object(fields) = value else {
            return Err(RegistryError::NotAPayload {
                noun: env_value_kind().noun(),
                base: env_value_kind().base(),
            });
        };
        if fields.len() == 1
            && let Some(Value::String(literal)) = fields.get("value")
        {
            return Ok(Self::literal(literal.clone()));
        }
        registry.parse(value).map(Self)
    }
}

/// One environment variable, with what it is for and whether it survives a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEntry {
    description: Option<String>,
    ephemeral: bool,
    value: EnvValue,
}

impl EnvEntry {
    /// Declares a variable.
    #[must_use]
    pub const fn new(value: EnvValue) -> Self {
        Self {
            description: None,
            ephemeral: false,
            value,
        }
    }

    /// Records what the variable is for.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Declares whether the variable survives a stop.
    #[must_use]
    pub const fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    /// What the variable is for.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Whether the variable survives a stop.
    #[must_use]
    pub const fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// How to obtain the value.
    #[must_use]
    pub const fn value(&self) -> &EnvValue {
        &self.value
    }

    /// Renders the entry.
    #[must_use]
    pub fn to_json(&self) -> Value {
        Value::Object(JsonMap::from_iter([
            (
                "description".to_owned(),
                self.description.clone().map_or(Value::Null, Value::from),
            ),
            ("ephemeral".to_owned(), Value::from(self.ephemeral)),
            ("value".to_owned(), self.value.to_json()),
        ]))
    }
}

/// One member of an environment, in whichever of the three shapes it was written.
///
/// The shape is kept rather than flattened on the way in, so a manifest renders back the way it was
/// written. [`Environment::normalized`] is where they become one thing.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvMember {
    /// A value written down as a bare string.
    Plain(String),
    /// A value with a discriminator and no surrounding metadata.
    Value(EnvValue),
    /// A value with a description and a persistence decision.
    Entry(EnvEntry),
}

impl EnvMember {
    /// Renders the member in the shape it was written.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Plain(value) => Value::from(value.clone()),
            Self::Value(value) => value.to_json(),
            Self::Entry(entry) => entry.to_json(),
        }
    }

    /// Flattens the member into an entry.
    #[must_use]
    pub fn normalized(&self) -> EnvEntry {
        match self {
            Self::Plain(value) => EnvEntry::new(EnvValue::literal(value.clone())),
            Self::Value(value) => EnvEntry::new(value.clone()),
            Self::Entry(entry) => entry.clone(),
        }
    }
}

/// The environment a workspace is materialized with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    value: BTreeMap<String, EnvMember>,
}

impl Environment {
    /// An empty environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a variable to a string.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.value
            .insert(name.into(), EnvMember::Plain(value.into()));
        self
    }

    /// Sets a variable to a value that has to be resolved.
    #[must_use]
    pub fn with_value(mut self, name: impl Into<String>, value: EnvValue) -> Self {
        self.value.insert(name.into(), EnvMember::Value(value));
        self
    }

    /// Sets a variable to a described entry.
    #[must_use]
    pub fn with_entry(mut self, name: impl Into<String>, entry: EnvEntry) -> Self {
        self.value.insert(name.into(), EnvMember::Entry(entry));
        self
    }

    /// Whether this environment sets anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// The members, in the shapes they were written.
    #[must_use]
    pub const fn members(&self) -> &BTreeMap<String, EnvMember> {
        &self.value
    }

    /// Every member as an entry, whichever shape it was written in.
    #[must_use]
    pub fn normalized(&self) -> BTreeMap<String, EnvEntry> {
        self.value
            .iter()
            .map(|(name, member)| (name.clone(), member.normalized()))
            .collect()
    }

    /// Renders the environment.
    ///
    /// Wrapped in a `value` field, which is the reference's shape: the environment is a model with
    /// one field rather than a bare mapping, and flattening it here would produce a manifest the
    /// reference could not read.
    #[must_use]
    pub fn to_json(&self) -> Value {
        Value::Object(JsonMap::from_iter([(
            "value".to_owned(),
            Value::Object(
                self.value
                    .iter()
                    .map(|(name, member)| (name.clone(), member.to_json()))
                    .collect(),
            ),
        )]))
    }

    /// Reads an environment, routing every reference through the types this host knows.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] for a payload of the wrong shape, or one naming a value type nobody
    /// registered.
    pub fn parse(registry: &TypeRegistry, value: &Value) -> Result<Self, RegistryError> {
        let not_a_payload = || RegistryError::NotAPayload {
            noun: env_value_kind().noun(),
            base: env_value_kind().base(),
        };

        let Value::Object(fields) = value else {
            return Err(not_a_payload());
        };
        let members = match fields.get("value") {
            None | Some(Value::Null) => return Ok(Self::new()),
            Some(Value::Object(members)) => members,
            Some(_) => return Err(not_a_payload()),
        };

        let mut environment = Self::new();
        for (name, member) in members {
            let member = match member {
                Value::String(plain) => EnvMember::Plain(plain.clone()),
                Value::Object(fields) => {
                    // A mapping is a value when it says which kind it is, or when it is the
                    // discriminator-free literal shape. Anything else is an entry wrapping one.
                    if fields.contains_key("type")
                        || matches!(fields.get("value"), Some(Value::String(_)))
                    {
                        EnvMember::Value(EnvValue::parse(registry, member)?)
                    } else {
                        EnvMember::Entry(parse_entry(registry, fields)?)
                    }
                }
                _ => return Err(not_a_payload()),
            };
            environment.value.insert(name.clone(), member);
        }
        Ok(environment)
    }

    /// Resolves every member into the string a process would see.
    ///
    /// Literals are answered here; everything else goes to the host's resolver. A failure cancels
    /// the lookups still in flight — see the module docs on why that matters.
    ///
    /// # Errors
    ///
    /// Returns the first resolver failure. Which one is first is not defined when several fail: the
    /// lookups run together, and imposing an order would mean running them one at a time.
    pub async fn resolve(
        &self,
        resolver: &dyn EnvValueResolver,
    ) -> Result<BTreeMap<String, String>, SandboxError> {
        let normalized = self.normalized();
        let resolutions = normalized.iter().map(|(name, entry)| async move {
            let value = match entry.value().as_literal() {
                Some(literal) => literal.to_owned(),
                None => resolver.resolve(entry.value()).await?,
            };
            Ok::<_, SandboxError>((name.clone(), value))
        });
        Ok(try_join_all(resolutions).await?.into_iter().collect())
    }
}

/// Reads an entry wrapping a value.
fn parse_entry(
    registry: &TypeRegistry,
    fields: &JsonMap<String, Value>,
) -> Result<EnvEntry, RegistryError> {
    let invalid = |reason: &str| RegistryError::InvalidPayload {
        noun: env_value_kind().noun(),
        type_name: "EnvEntry".to_owned(),
        reason: reason.to_owned(),
    };

    let value = fields
        .get("value")
        .ok_or_else(|| invalid("env entry must include a `value`"))?;
    let mut entry = EnvEntry::new(EnvValue::parse(registry, value)?);
    match fields.get("description") {
        Some(Value::String(description)) => entry = entry.with_description(description.clone()),
        None | Some(Value::Null) => {}
        Some(_) => return Err(invalid("env entry `description` must be a string")),
    }
    match fields.get("ephemeral") {
        Some(Value::Bool(ephemeral)) => entry = entry.ephemeral(*ephemeral),
        None | Some(Value::Null) => {}
        Some(_) => return Err(invalid("env entry `ephemeral` must be a boolean")),
    }
    Ok(entry)
}

impl Serialize for Environment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

/// Turns an environment value reference into the string it stands for.
///
/// Implemented by a host for the value types it registered. A literal never reaches it.
#[async_trait]
pub trait EnvValueResolver: Send + Sync {
    /// Fetches one value.
    ///
    /// # Errors
    ///
    /// Returns whatever the lookup failed with. A failure here cancels the sibling lookups.
    async fn resolve(&self, value: &EnvValue) -> Result<String, SandboxError>;
}

/// A resolver that refuses every reference.
///
/// For a host that declares only literals: reaching it means a manifest named a value type nothing
/// was wired up to fetch, which is worth a clear failure rather than an empty string.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnresolvableEnvValues;

#[async_trait]
impl EnvValueResolver for UnresolvableEnvValues {
    async fn resolve(&self, value: &EnvValue) -> Result<String, SandboxError> {
        Err(SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            OpName::Materialize,
            format!(
                "no resolver is configured for env value type `{}`",
                value.type_name()
            ),
        ))
    }
}
