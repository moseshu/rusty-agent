//! V4A patch representation, AST, and matching levels.

use std::path::{Path, PathBuf};

use ra_core::compat::{SchemaVersion, Unknown};
use serde::{Deserialize, Serialize};

use crate::PATCH_SCHEMA_VERSION;

const fn default_schema_version() -> SchemaVersion {
    PATCH_SCHEMA_VERSION
}

/// The tolerance level used when matching hunk context lines against target file content.
#[non_exhaustive]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PatchMatchLevel {
    /// Exact character-for-character match.
    #[default]
    Exact,
    /// Match ignoring trailing whitespace on each line.
    TrimEnd,
    /// Match ignoring leading and trailing whitespace.
    Trim,
    /// Match with normalized Unicode whitespace and punctuation.
    Normalized,
}

/// A conflict or ambiguity detected when evaluating a patch against target files.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PatchConflict {
    /// Target file does not exist on disk.
    FileNotFound {
        /// Expected path.
        path: PathBuf,
    },
    /// Hunk context lines cannot be located in the file.
    HunkFailed {
        /// File where the hunk failed.
        path: PathBuf,
        /// Hunk index.
        hunk_index: usize,
        /// Matching tolerance at which search failed.
        level: PatchMatchLevel,
    },
    /// Multiple disjoint locations match the hunk context equally well.
    AmbiguousMatch {
        /// File with ambiguity.
        path: PathBuf,
        /// Hunk index.
        hunk_index: usize,
        /// Number of candidate locations found.
        candidate_count: usize,
    },
    /// File to be created already exists.
    FileAlreadyExists {
        /// Existing path.
        path: PathBuf,
    },
    /// Source file for move does not exist.
    SourceFileNotFound {
        /// Missing source path.
        path: PathBuf,
    },
    /// Destination path for move already exists.
    DestinationAlreadyExists {
        /// Existing destination path.
        path: PathBuf,
    },
}

/// One hunk of changes within a file update.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchHunk {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line_hint: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    context_before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed_lines: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    added_lines: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    context_after: Vec<String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl PatchHunk {
    /// Creates an empty hunk.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: PATCH_SCHEMA_VERSION,
            line_hint: None,
            context_before: Vec::new(),
            removed_lines: Vec::new(),
            added_lines: Vec::new(),
            context_after: Vec::new(),
            unknown: Unknown::new(),
        }
    }

    /// Schema version of this hunk.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Sets the line hint.
    #[must_use]
    pub const fn with_line_hint(mut self, hint: usize) -> Self {
        self.line_hint = Some(hint);
        self
    }

    /// Sets the preceding context lines.
    #[must_use]
    pub fn with_context_before(
        mut self,
        lines: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.context_before = lines.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the removed lines.
    #[must_use]
    pub fn with_removed_lines(
        mut self,
        lines: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.removed_lines = lines.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the added lines.
    #[must_use]
    pub fn with_added_lines(mut self, lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.added_lines = lines.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the following context lines.
    #[must_use]
    pub fn with_context_after(
        mut self,
        lines: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.context_after = lines.into_iter().map(Into::into).collect();
        self
    }

    /// Line number hint from patch header.
    #[must_use]
    pub const fn line_hint(&self) -> Option<usize> {
        self.line_hint
    }

    /// Context lines preceding modification.
    #[must_use]
    pub fn context_before(&self) -> &[String] {
        &self.context_before
    }

    /// Removed lines.
    #[must_use]
    pub fn removed_lines(&self) -> &[String] {
        &self.removed_lines
    }

    /// Added lines.
    #[must_use]
    pub fn added_lines(&self) -> &[String] {
        &self.added_lines
    }

    /// Context lines following modification.
    #[must_use]
    pub fn context_after(&self) -> &[String] {
        &self.context_after
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

impl Default for PatchHunk {
    fn default() -> Self {
        Self::new()
    }
}

/// An individual action in a patch plan.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PatchAction {
    /// Update existing file with hunks.
    UpdateFile {
        /// Target file path.
        path: PathBuf,
        /// Hunks to apply.
        hunks: Vec<PatchHunk>,
    },
    /// Create a new file with full content.
    AddFile {
        /// Target file path.
        path: PathBuf,
        /// File content.
        content: String,
    },
    /// Delete a file.
    DeleteFile {
        /// Target file path.
        path: PathBuf,
    },
    /// Move/rename a file.
    MoveFile {
        /// Source path.
        from: PathBuf,
        /// Destination path.
        to: PathBuf,
    },
}

impl PatchAction {
    /// Returns all paths affected by this action (for `MoveFile`, includes both `from` and `to`).
    #[must_use]
    pub fn target_paths(&self) -> Vec<&Path> {
        match self {
            Self::UpdateFile { path, .. }
            | Self::AddFile { path, .. }
            | Self::DeleteFile { path } => vec![path.as_path()],
            Self::MoveFile { from, to } => vec![from.as_path(), to.as_path()],
        }
    }
}

/// A structured, previewable patch plan prior to filesystem application.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchPlan {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    actions: Vec<PatchAction>,
    #[serde(default)]
    match_level: PatchMatchLevel,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<PatchConflict>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl PatchPlan {
    /// Creates a patch plan from a sequence of actions.
    #[must_use]
    pub fn new(actions: Vec<PatchAction>) -> Self {
        Self {
            schema_version: PATCH_SCHEMA_VERSION,
            actions,
            match_level: PatchMatchLevel::Exact,
            conflicts: Vec::new(),
            unknown: Unknown::new(),
        }
    }

    /// Sets conflicts detected for the plan.
    #[must_use]
    pub fn with_conflicts(mut self, conflicts: Vec<PatchConflict>) -> Self {
        self.conflicts = conflicts;
        self
    }

    /// Sets the match level for the plan.
    #[must_use]
    pub const fn with_match_level(mut self, match_level: PatchMatchLevel) -> Self {
        self.match_level = match_level;
        self
    }

    /// Schema version of this patch plan.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Planned actions.
    #[must_use]
    pub fn actions(&self) -> &[PatchAction] {
        &self.actions
    }

    /// All target files affected by the actions in this plan (deduplicated).
    #[must_use]
    pub fn target_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for action in &self.actions {
            for p in action.target_paths() {
                let pb = p.to_path_buf();
                if !files.contains(&pb) {
                    files.push(pb);
                }
            }
        }
        files
    }

    /// Match tolerance level.
    #[must_use]
    pub const fn match_level(&self) -> PatchMatchLevel {
        self.match_level
    }

    /// Detected conflicts.
    #[must_use]
    pub fn conflicts(&self) -> &[PatchConflict] {
        &self.conflicts
    }

    /// Whether any conflicts or ambiguities were detected.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
