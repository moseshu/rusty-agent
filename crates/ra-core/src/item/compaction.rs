//! Context compaction items.

use serde::{Deserialize, Serialize};

use super::ItemId;
use crate::compat::{SchemaVersion, Unknown};

/// Current compaction schema version.
pub const COMPACTION_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// The result of a provider-neutral compaction operation.
///
/// Provider-specific opaque compact items belong in [`super::RawProviderItem`]. This type stores
/// the portable summary and the session item IDs it covers, keeping local sessions independent of
/// any one server format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    schema_version: SchemaVersion,
    summary: String,
    #[serde(default)]
    compacted_items: Vec<ItemId>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Compaction {
    /// Creates a compaction summary.
    #[must_use]
    pub fn new(summary: impl Into<String>, compacted_items: Vec<ItemId>) -> Self {
        Self {
            schema_version: COMPACTION_SCHEMA_VERSION,
            summary: summary.into(),
            compacted_items,
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Portable summary.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Session item IDs covered by this summary.
    #[must_use]
    pub fn compacted_items(&self) -> &[ItemId] {
        &self.compacted_items
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
