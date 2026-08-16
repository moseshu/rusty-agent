//! Resource identities and concurrency claims for tool effects.
//!
//! Tools can declare fine-grained resource claims to coordinate concurrent execution. When two
//! tools target distinct resources, or request shared read access to the same resource, they may
//! run in parallel without serializing the whole turn.

use std::{borrow::Cow, fmt};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::error::{Error, Result};

/// The domain or category of an execution resource.
///
/// Standard categories include workspaces and processes, while custom extensions can represent
/// database connections, network endpoints, or external devices.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    /// A file workspace or filesystem sandbox.
    Workspace,
    /// A running process or execution handle.
    Process,
    /// A category unrecognized by this build, preserved verbatim.
    Custom(Cow<'static, str>),
}

impl ResourceKind {
    /// Creates a [custom category](Self::Custom).
    #[must_use]
    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self {
        let name = name.into();
        Self::known(&name).unwrap_or(Self::Custom(name))
    }

    /// Stable string identifier for this category.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Workspace => "workspace",
            Self::Process => "process",
            Self::Custom(name) => name.as_ref(),
        }
    }

    fn known(name: &str) -> Option<Self> {
        match name {
            "workspace" => Some(Self::Workspace),
            "process" => Some(Self::Process),
            _ => None,
        }
    }
}

impl From<&'static str> for ResourceKind {
    fn from(name: &'static str) -> Self {
        Self::known(name).unwrap_or(Self::Custom(Cow::Borrowed(name)))
    }
}

impl From<String> for ResourceKind {
    fn from(name: String) -> Self {
        match Self::known(&name) {
            Some(known) => known,
            None => Self::Custom(Cow::Owned(name)),
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ResourceKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ResourceKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        Ok(Self::from(name))
    }
}

/// An opaque, structured identifier for a managed resource.
///
/// Runtime schedulers match resource identities to evaluate mutual exclusion, without inspecting
/// or interpreting the internal semantics of the resource path or identifier string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ResourceId {
    kind: ResourceKind,
    value: Cow<'static, str>,
}

impl<'de> Deserialize<'de> for ResourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ResourceIdWire {
            kind: ResourceKind,
            value: String,
        }

        let wire = ResourceIdWire::deserialize(deserializer)?;
        Self::new(wire.kind, wire.value).map_err(D::Error::custom)
    }
}

impl ResourceId {
    /// Creates a validated resource identity.
    pub fn new(kind: ResourceKind, value: impl Into<Cow<'static, str>>) -> Result<Self> {
        let kind = match kind {
            ResourceKind::Custom(custom_name) => {
                if custom_name.is_empty()
                    || custom_name.trim() != custom_name.as_ref()
                    || custom_name.chars().any(char::is_control)
                {
                    return Err(Error::caller(
                        "resource kind custom name must be non-empty, trimmed, and contain no control characters",
                    ));
                }
                ResourceKind::known(custom_name.as_ref())
                    .unwrap_or(ResourceKind::Custom(custom_name))
            }
            other => other,
        };
        let value = value.into();
        if value.is_empty() || value.trim() != value.as_ref() || value.chars().any(char::is_control)
        {
            return Err(Error::caller(
                "resource ID value must be non-empty, trimmed, and contain no control characters",
            ));
        }
        Ok(Self { kind, value })
    }

    /// Creates a workspace resource identity.
    pub fn workspace(name: impl Into<Cow<'static, str>>) -> Result<Self> {
        Self::new(ResourceKind::Workspace, name)
    }

    /// Creates a process resource identity.
    pub fn process(name: impl Into<Cow<'static, str>>) -> Result<Self> {
        Self::new(ResourceKind::Process, name)
    }

    /// Creates a resource identity with a custom category.
    pub fn custom(
        kind: impl Into<Cow<'static, str>>,
        name: impl Into<Cow<'static, str>>,
    ) -> Result<Self> {
        Self::new(ResourceKind::custom(kind), name)
    }

    /// The category of this resource.
    #[must_use]
    pub const fn kind(&self) -> &ResourceKind {
        &self.kind
    }

    /// The string identifier of this resource.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.value)
    }
}

/// The mode of access requested on a resource.
#[non_exhaustive]
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    /// Shared read-only access that may overlap with other shared claims.
    #[default]
    Shared,
    /// Exclusive read-write access that conflicts with all other claims on the resource.
    Exclusive,
}

impl ResourceAccess {
    /// Whether this claim requests exclusive access.
    #[must_use]
    pub const fn is_exclusive(self) -> bool {
        matches!(self, Self::Exclusive)
    }

    /// Whether this claim requests shared access.
    #[must_use]
    pub const fn is_shared(self) -> bool {
        matches!(self, Self::Shared)
    }

    /// Whether two access modes conflict when requested on the same resource.
    #[must_use]
    pub const fn conflicts_with(self, other: Self) -> bool {
        self.is_exclusive() || other.is_exclusive()
    }
}

/// A declared effect or concurrency requirement on a specific resource.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResourceClaim {
    resource: ResourceId,
    access: ResourceAccess,
}

impl ResourceClaim {
    /// Creates a claim on a resource with the given access mode.
    #[must_use]
    pub const fn new(resource: ResourceId, access: ResourceAccess) -> Self {
        Self { resource, access }
    }

    /// Creates a shared claim on a resource.
    #[must_use]
    pub const fn shared(resource: ResourceId) -> Self {
        Self::new(resource, ResourceAccess::Shared)
    }

    /// Creates an exclusive claim on a resource.
    #[must_use]
    pub const fn exclusive(resource: ResourceId) -> Self {
        Self::new(resource, ResourceAccess::Exclusive)
    }

    /// Deduplicates resource claims on identical resources, upgrading to exclusive access if
    /// any claim on that resource is exclusive, and sorting by [`ResourceId`] for deterministic
    /// canonical lock ordering.
    #[must_use]
    pub fn deduplicate(claims: impl IntoIterator<Item = Self>) -> Vec<Self> {
        let iter = claims.into_iter();
        let (lower, _) = iter.size_hint();
        let mut result: Vec<Self> = Vec::with_capacity(lower);
        for claim in iter {
            if let Some(existing) = result.iter_mut().find(|c| c.resource == claim.resource) {
                if claim.is_exclusive() {
                    existing.access = ResourceAccess::Exclusive;
                }
            } else {
                result.push(claim);
            }
        }
        result.sort_by(|a, b| a.resource.cmp(&b.resource));
        result
    }

    /// The target resource.
    #[must_use]
    pub const fn resource(&self) -> &ResourceId {
        &self.resource
    }

    /// The requested access mode.
    #[must_use]
    pub const fn access(&self) -> ResourceAccess {
        self.access
    }

    /// Whether this claim is exclusive.
    #[must_use]
    pub const fn is_exclusive(&self) -> bool {
        self.access.is_exclusive()
    }

    /// Whether this claim is shared.
    #[must_use]
    pub const fn is_shared(&self) -> bool {
        self.access.is_shared()
    }

    /// Whether this claim conflicts with another claim.
    #[must_use]
    pub fn conflicts_with(&self, other: &Self) -> bool {
        self.resource == other.resource && self.access.conflicts_with(other.access)
    }
}
