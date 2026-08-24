//! V4A patch representation, AST, and matching levels.

use std::path::{Path, PathBuf};

use ra_core::compat::{SchemaVersion, Unknown};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::PATCH_SCHEMA_VERSION;

const fn default_schema_version() -> SchemaVersion {
    PATCH_SCHEMA_VERSION
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
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
    /// Multiple locations match the hunk context equally well.
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

/// A syntactically invalid or unsafe V4A patch.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid patch at line {line}: {message}")]
pub struct PatchParseError {
    line: usize,
    message: String,
}

impl PatchParseError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
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
    context_anchors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    context_before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed_lines: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    added_lines: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    context_after: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    end_of_file: bool,
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
            context_anchors: Vec::new(),
            context_before: Vec::new(),
            removed_lines: Vec::new(),
            added_lines: Vec::new(),
            context_after: Vec::new(),
            end_of_file: false,
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

    /// Sets the optional V4A hunk-header anchor used to narrow the later context search.
    #[must_use]
    pub fn with_context_anchor(mut self, anchor: impl Into<String>) -> Self {
        self.context_anchors.push(anchor.into());
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

    /// Requires this hunk to match at the end of its target file.
    #[must_use]
    pub const fn with_end_of_file(mut self) -> Self {
        self.end_of_file = true;
        self
    }

    /// Line number hint from patch header.
    #[must_use]
    pub const fn line_hint(&self) -> Option<usize> {
        self.line_hint
    }

    /// Search anchors from one or more nested `@@ scope` headers.
    #[must_use]
    pub fn context_anchors(&self) -> &[String] {
        &self.context_anchors
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

    /// Whether the hunk carries an end-of-file marker.
    #[must_use]
    pub const fn end_of_file(&self) -> bool {
        self.end_of_file
    }

    pub(crate) fn pattern_lines(&self) -> Vec<String> {
        self.context_before
            .iter()
            .chain(&self.removed_lines)
            .chain(&self.context_after)
            .cloned()
            .collect()
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

/// Parses complete V4A freeform patch text into a filesystem-independent plan.
///
/// Marker lines accept surrounding whitespace, but paths never accept absolute or parent-relative
/// forms: the parser must not produce a plan that a rooted filesystem capability cannot safely use.
pub fn parse_patch(input: &str) -> Result<PatchPlan, PatchParseError> {
    let mut lines = input.lines().collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() < 2 || lines.first().map(|line| line.trim()) != Some("*** Begin Patch") {
        return Err(PatchParseError::new(
            1,
            "the first line must be `*** Begin Patch`",
        ));
    }
    if lines.last().map(|line| line.trim()) != Some("*** End Patch") {
        return Err(PatchParseError::new(
            lines.len().max(1),
            "the last line must be `*** End Patch`",
        ));
    }

    let mut actions = Vec::new();
    let mut cursor = 1;
    while cursor + 1 < lines.len() {
        let line_number = cursor + 1;
        let marker = lines[cursor].trim();
        if let Some(path) = marker.strip_prefix("*** Add File:") {
            let path = parse_path(path, line_number)?;
            cursor += 1;
            let mut content = String::new();
            while cursor + 1 < lines.len() && !lines[cursor].trim_start().starts_with("*** ") {
                let line = lines[cursor];
                let Some(text) = line.strip_prefix('+') else {
                    return Err(PatchParseError::new(
                        cursor + 1,
                        "add-file content must start with `+`",
                    ));
                };
                content.push_str(text);
                content.push('\n');
                cursor += 1;
            }
            if content.is_empty() {
                return Err(PatchParseError::new(
                    line_number,
                    "an added file needs at least one `+` line",
                ));
            }
            actions.push(PatchAction::AddFile { path, content });
        } else if let Some(path) = marker.strip_prefix("*** Delete File:") {
            actions.push(PatchAction::DeleteFile {
                path: parse_path(path, line_number)?,
            });
            cursor += 1;
        } else if let Some(path) = marker.strip_prefix("*** Update File:") {
            let path = parse_path(path, line_number)?;
            cursor += 1;
            let mut move_to = None;
            if cursor + 1 < lines.len()
                && let Some(destination) = lines[cursor].trim().strip_prefix("*** Move to:")
            {
                move_to = Some(parse_path(destination, cursor + 1)?);
                cursor += 1;
            }
            let mut hunks = Vec::new();
            while cursor + 1 < lines.len() && !lines[cursor].trim_start().starts_with("*** ") {
                hunks.extend(parse_hunks(&lines, &mut cursor)?);
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(PatchParseError::new(
                    line_number,
                    "an updated file needs at least one hunk or a move destination",
                ));
            }
            if !hunks.is_empty() {
                actions.push(PatchAction::UpdateFile {
                    path: path.clone(),
                    hunks,
                });
            }
            if let Some(to) = move_to {
                actions.push(PatchAction::MoveFile { from: path, to });
            }
        } else {
            return Err(PatchParseError::new(
                line_number,
                "expected an add, delete, or update file marker",
            ));
        }
    }
    Ok(PatchPlan::new(actions))
}

fn parse_path(value: &str, line: usize) -> Result<PathBuf, PatchParseError> {
    let path = PathBuf::from(value.trim());
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(PatchParseError::new(
            line,
            "file paths must be non-empty and stay below the workspace root",
        ));
    }
    Ok(path)
}

fn parse_hunks(lines: &[&str], cursor: &mut usize) -> Result<Vec<PatchHunk>, PatchParseError> {
    let mut anchors = Vec::new();
    loop {
        let header = lines[*cursor].trim();
        let Some(scope) = header.strip_prefix("@@") else {
            return Err(PatchParseError::new(
                *cursor + 1,
                "expected a hunk header beginning with `@@`",
            ));
        };
        if !scope.trim().is_empty() {
            anchors.push(scope.trim().to_owned());
        }
        *cursor += 1;
        if *cursor + 1 >= lines.len() || !lines[*cursor].trim().starts_with("@@") {
            break;
        }
    }
    let mut before = Vec::new();
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut after = Vec::new();
    let mut saw_change = false;
    let mut result = Vec::new();
    while *cursor + 1 < lines.len()
        && !lines[*cursor].trim_start().starts_with("*** ")
        && !lines[*cursor].trim().starts_with("@@")
    {
        let line = lines[*cursor];
        let Some((kind, text)) = line
            .chars()
            .next()
            .map(|kind| (kind, &line[kind.len_utf8()..]))
        else {
            return Err(PatchParseError::new(
                *cursor + 1,
                "hunk lines cannot be empty",
            ));
        };
        match kind {
            ' ' if saw_change => after.push(text.to_owned()),
            ' ' => before.push(text.to_owned()),
            '-' => {
                if saw_change && !after.is_empty() {
                    result.push(build_hunk(
                        &before,
                        &removed,
                        &added,
                        &after,
                        std::mem::take(&mut anchors),
                    ));
                    before = std::mem::take(&mut after);
                    removed.clear();
                    added.clear();
                }
                saw_change = true;
                removed.push(text.to_owned());
            }
            '+' => {
                if saw_change && !after.is_empty() {
                    result.push(build_hunk(
                        &before,
                        &removed,
                        &added,
                        &after,
                        std::mem::take(&mut anchors),
                    ));
                    before = std::mem::take(&mut after);
                    removed.clear();
                    added.clear();
                }
                saw_change = true;
                added.push(text.to_owned());
            }
            _ => {
                return Err(PatchParseError::new(
                    *cursor + 1,
                    "hunk lines must start with space, `+`, or `-`",
                ));
            }
        }
        *cursor += 1;
    }
    if !saw_change {
        return Err(PatchParseError::new(
            *cursor + 1,
            "a hunk needs at least one added or removed line",
        ));
    }
    let mut hunk = build_hunk(&before, &removed, &added, &after, anchors);
    if *cursor + 1 < lines.len() && lines[*cursor].trim() == "*** End of File" {
        hunk = hunk.with_end_of_file();
        *cursor += 1;
    }
    result.push(hunk);
    Ok(result)
}

fn build_hunk(
    before: &[String],
    removed: &[String],
    added: &[String],
    after: &[String],
    anchors: Vec<String>,
) -> PatchHunk {
    let hunk = PatchHunk::new()
        .with_context_before(before.iter().cloned())
        .with_removed_lines(removed.iter().cloned())
        .with_added_lines(added.iter().cloned())
        .with_context_after(after.iter().cloned());
    anchors
        .into_iter()
        .fold(hunk, PatchHunk::with_context_anchor)
}
