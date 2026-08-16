//! Committed patch application results and deltas.

use std::path::PathBuf;

use ra_core::compat::{SchemaVersion, Unknown};
use serde::{Deserialize, Serialize};

use crate::PATCH_SCHEMA_VERSION;

const fn default_schema_version() -> SchemaVersion {
    PATCH_SCHEMA_VERSION
}

/// The authoritative record of applied patch modifications.
///
/// This represents the concrete delta actually written to disk, without assuming unverified
/// cross-file atomicity.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedPatchDelta {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    applied_files: Vec<PathBuf>,
    lines_added: usize,
    lines_removed: usize,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl CommittedPatchDelta {
    /// Creates a committed delta record.
    #[must_use]
    pub fn new(applied_files: Vec<PathBuf>, lines_added: usize, lines_removed: usize) -> Self {
        Self {
            schema_version: PATCH_SCHEMA_VERSION,
            applied_files,
            lines_added,
            lines_removed,
            unknown: Unknown::new(),
        }
    }

    /// Creates an empty delta representing no changes.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema_version: PATCH_SCHEMA_VERSION,
            applied_files: Vec::new(),
            lines_added: 0,
            lines_removed: 0,
            unknown: Unknown::new(),
        }
    }

    /// Schema version of this delta record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Successfully modified file paths.
    #[must_use]
    pub fn applied_files(&self) -> &[PathBuf] {
        &self.applied_files
    }

    /// Total lines inserted.
    #[must_use]
    pub const fn lines_added(&self) -> usize {
        self.lines_added
    }

    /// Total lines deleted.
    #[must_use]
    pub const fn lines_removed(&self) -> usize {
        self.lines_removed
    }

    /// Whether this delta contains any modifications.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.applied_files.is_empty() && self.lines_added == 0 && self.lines_removed == 0
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

impl Default for CommittedPatchDelta {
    fn default() -> Self {
        Self::empty()
    }
}
