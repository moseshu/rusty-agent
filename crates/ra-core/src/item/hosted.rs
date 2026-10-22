//! Provider-executed tool records. Wire payloads remain opaque to the framework.

use serde::{Deserialize, Serialize};

use super::RawProviderItem;
use crate::compat::{SchemaVersion, Unknown};

/// A tool invocation completed by the provider, with no local execution or paired output owed.
///
/// The name has the same meaning as a tool name in the framework contract. The existing raw-item
/// envelope carries replay material because a Rust model-input projection owns its data rather
/// than retaining the upstream Python object's raw-item reference. Only an adapter interprets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedToolCall {
    schema_version: SchemaVersion,
    name: String,
    raw: RawProviderItem,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl HostedToolCall {
    /// Records the named tool's provider-owned invocation.
    #[must_use]
    pub fn new(name: impl Into<String>, raw: RawProviderItem) -> Self {
        Self {
            schema_version: SchemaVersion::new(1),
            unknown: Unknown::new(),
            name: name.into(),
            raw,
        }
    }

    /// Stored record schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Framework tool name, independent of the provider's item discriminator.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Opaque replay material, interpreted only by its adapter.
    #[must_use]
    pub const fn raw(&self) -> &RawProviderItem {
        &self.raw
    }
}
