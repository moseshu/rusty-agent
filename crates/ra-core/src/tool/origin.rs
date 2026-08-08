//! Serializable tool identity used by dispatch and state restoration.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use super::{ToolNamespace, namespace::validate_namespace};
use crate::{
    compat::{SchemaVersion, Unknown},
    error::{Error, Result},
};

/// Current tool-origin schema version.
pub const TOOL_ORIGIN_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Shape of a [`ToolLookupKey`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLookupKind {
    /// A normal top-level function tool.
    Bare,
    /// A function tool routed through an explicit namespace.
    Namespaced,
    /// A top-level tool exposed only after deferred discovery.
    DeferredTopLevel,
}

/// Collision-free routing identity.
///
/// Private fields force every constructed or deserialized value through shape validation. In
/// particular, a deferred top-level tool is not equivalent to a bare tool with the same name.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ToolLookupKey {
    kind: ToolLookupKind,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<ToolNamespace>,
}

impl ToolLookupKey {
    /// Creates a bare lookup key.
    pub fn bare(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_tool_name(&name)?;
        Ok(Self {
            kind: ToolLookupKind::Bare,
            name,
            namespace: None,
        })
    }

    /// Creates an explicitly namespaced lookup key.
    pub fn namespaced(namespace: ToolNamespace, name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_tool_name(&name)?;
        if namespace.as_str() == name {
            return Err(Error::caller(
                "a namespace equal to the tool name is reserved for deferred top-level routing",
            ));
        }
        Ok(Self {
            kind: ToolLookupKind::Namespaced,
            name,
            namespace: Some(namespace),
        })
    }

    /// Creates a deferred top-level lookup key.
    pub fn deferred_top_level(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_tool_name(&name)?;
        Ok(Self {
            kind: ToolLookupKind::DeferredTopLevel,
            name,
            namespace: None,
        })
    }

    /// Reconstructs the lookup shape carried by a provider call.
    ///
    /// A provider may encode deferred top-level calls using a synthetic namespace equal to the
    /// tool name; that reserved shape maps back to `DeferredTopLevel`.
    pub fn for_call(name: impl Into<String>, namespace: Option<ToolNamespace>) -> Result<Self> {
        let name = name.into();
        match namespace {
            Some(namespace) if namespace.as_str() == name => Self::deferred_top_level(name),
            Some(namespace) => Self::namespaced(namespace, name),
            None => Self::bare(name),
        }
    }

    /// Routing shape.
    #[must_use]
    pub const fn kind(&self) -> ToolLookupKind {
        self.kind
    }

    /// Public tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Explicit namespace, excluding the synthetic deferred wire namespace.
    #[must_use]
    pub const fn namespace(&self) -> Option<&ToolNamespace> {
        self.namespace.as_ref()
    }

    /// Whether this key denotes a deferred top-level tool.
    #[must_use]
    pub const fn is_deferred_top_level(&self) -> bool {
        matches!(self.kind, ToolLookupKind::DeferredTopLevel)
    }
}

#[derive(Deserialize)]
struct ToolLookupKeyWire {
    kind: ToolLookupKind,
    name: String,
    #[serde(default)]
    namespace: Option<ToolNamespace>,
}

impl<'de> Deserialize<'de> for ToolLookupKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolLookupKeyWire::deserialize(deserializer)?;
        let key = match (wire.kind, wire.namespace) {
            (ToolLookupKind::Bare, None) => Self::bare(wire.name),
            (ToolLookupKind::Namespaced, Some(namespace)) => Self::namespaced(namespace, wire.name),
            (ToolLookupKind::DeferredTopLevel, None) => Self::deferred_top_level(wire.name),
            (kind, namespace) => Err(Error::caller(format!(
                "tool lookup key shape `{kind:?}` has invalid namespace {namespace:?}"
            ))),
        };
        key.map_err(D::Error::custom)
    }
}

/// Serializable source and routing identity for one tool.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolOrigin {
    schema_version: SchemaVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<ToolNamespace>,
    qualified_name: String,
    lookup_key: ToolLookupKey,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ToolOrigin {
    /// Creates an unnamespaced tool identity.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        Self::from_lookup_key(ToolLookupKey::bare(name)?)
    }

    /// Creates an explicitly namespaced tool identity.
    pub fn namespaced(namespace: ToolNamespace, name: impl Into<String>) -> Result<Self> {
        Self::from_lookup_key(ToolLookupKey::namespaced(namespace, name)?)
    }

    /// Creates a deferred top-level identity distinct from the corresponding bare tool.
    pub fn deferred_top_level(name: impl Into<String>) -> Result<Self> {
        Self::from_lookup_key(ToolLookupKey::deferred_top_level(name)?)
    }

    /// Reconstructs a canonical origin from a persisted lookup key.
    pub fn from_lookup_key(lookup_key: ToolLookupKey) -> Result<Self> {
        validate_lookup_key(&lookup_key)?;
        let namespace = lookup_key.namespace().cloned();
        let qualified_name = qualify(lookup_key.name(), namespace.as_ref());
        Ok(Self {
            schema_version: TOOL_ORIGIN_SCHEMA_VERSION,
            namespace,
            qualified_name,
            lookup_key,
            unknown: Unknown::new(),
        })
    }

    /// Validates redundant persisted fields against the canonical lookup key.
    pub fn validate(&self) -> Result<()> {
        validate_lookup_key(&self.lookup_key)?;
        if self.namespace.as_ref() != self.lookup_key.namespace() {
            return Err(Error::caller(
                "tool origin namespace does not match its lookup key",
            ));
        }
        let expected = qualify(self.lookup_key.name(), self.lookup_key.namespace());
        if self.qualified_name != expected {
            return Err(Error::caller(format!(
                "tool origin qualified name `{}` does not match canonical `{expected}`",
                self.qualified_name
            )));
        }
        Ok(())
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Public name inside the namespace.
    #[must_use]
    pub fn name(&self) -> &str {
        self.lookup_key.name()
    }

    /// Explicit namespace.
    #[must_use]
    pub const fn namespace(&self) -> Option<&ToolNamespace> {
        self.namespace.as_ref()
    }

    /// Display/trace name. Never use this string as a dispatch key.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Stable key used by registries, approvals, and persisted run state.
    #[must_use]
    pub const fn lookup_key(&self) -> &ToolLookupKey {
        &self.lookup_key
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

#[derive(Deserialize)]
struct ToolOriginWire {
    schema_version: SchemaVersion,
    #[serde(default)]
    namespace: Option<ToolNamespace>,
    qualified_name: String,
    lookup_key: ToolLookupKey,
    #[serde(flatten, default)]
    unknown: Unknown,
}

impl<'de> Deserialize<'de> for ToolOrigin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolOriginWire::deserialize(deserializer)?;
        let origin = Self {
            schema_version: wire.schema_version,
            namespace: wire.namespace,
            qualified_name: wire.qualified_name,
            lookup_key: wire.lookup_key,
            unknown: wire.unknown,
        };
        origin.validate().map_err(D::Error::custom)?;
        Ok(origin)
    }
}

fn qualify(name: &str, namespace: Option<&ToolNamespace>) -> String {
    namespace.map_or_else(
        || name.to_owned(),
        |namespace| format!("{namespace}.{name}"),
    )
}

fn validate_lookup_key(key: &ToolLookupKey) -> Result<()> {
    validate_tool_name(key.name())?;
    match (key.kind(), key.namespace()) {
        (ToolLookupKind::Bare | ToolLookupKind::DeferredTopLevel, None) => {}
        (ToolLookupKind::Namespaced, Some(namespace)) => {
            validate_namespace(namespace.as_str())?;
            if namespace.as_str() == key.name() {
                return Err(Error::caller(
                    "a namespace equal to the tool name is reserved for deferred top-level routing",
                ));
            }
        }
        (kind, namespace) => {
            return Err(Error::caller(format!(
                "tool lookup key shape `{kind:?}` has invalid namespace {namespace:?}"
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
        return Err(Error::caller(
            "tool name must be non-empty, trimmed, and contain no control characters",
        ));
    }
    Ok(())
}
