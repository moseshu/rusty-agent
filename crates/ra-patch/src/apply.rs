//! Pure V4A hunk application and committed patch records.

use std::path::{Path, PathBuf};

use ra_core::compat::{SchemaVersion, Unknown};
use serde::{Deserialize, Serialize};

use crate::{
    PATCH_SCHEMA_VERSION, PatchConflict, PatchHunk, PatchMatchLevel,
    fuzz::{find_anchor_candidates, find_candidates},
};

const fn default_schema_version() -> SchemaVersion {
    PATCH_SCHEMA_VERSION
}

/// The authoritative record of modifications actually written to disk.
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
    /// Creates a record from files written and line counts observed during application.
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
    /// Creates a record that proves no action reached the filesystem.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new(), 0, 0)
    }
    /// Schema version of this record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }
    /// Paths that were successfully modified.
    #[must_use]
    pub fn applied_files(&self) -> &[PathBuf] {
        &self.applied_files
    }
    /// Number of inserted lines in successfully modified files.
    #[must_use]
    pub const fn lines_added(&self) -> usize {
        self.lines_added
    }
    /// Number of removed lines in successfully modified files.
    #[must_use]
    pub const fn lines_removed(&self) -> usize {
        self.lines_removed
    }
    /// Whether this record contains no committed changes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.applied_files.is_empty() && self.lines_added == 0 && self.lines_removed == 0
    }
    /// Unknown fields preserved while reading a newer record.
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

/// The in-memory result of applying update hunks to one text file.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedFileUpdate {
    content: String,
    match_level: PatchMatchLevel,
    lines_added: usize,
    lines_removed: usize,
}

impl AppliedFileUpdate {
    /// Updated text, ready for the filesystem boundary to write.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }
    /// Least strict matching level needed by any hunk.
    #[must_use]
    pub const fn match_level(&self) -> PatchMatchLevel {
        self.match_level
    }
    /// Number of added lines.
    #[must_use]
    pub const fn lines_added(&self) -> usize {
        self.lines_added
    }
    /// Number of removed lines.
    #[must_use]
    pub const fn lines_removed(&self) -> usize {
        self.lines_removed
    }
}

/// Applies update hunks without accessing the filesystem.
///
/// Existing line endings are kept for untouched lines and replacement lines use the local line
/// ending, so a CRLF file is not silently rewritten as LF.
pub fn apply_hunks(
    path: &Path,
    original: &str,
    hunks: &[PatchHunk],
) -> Result<AppliedFileUpdate, PatchConflict> {
    let mut lines = SourceFile::parse(original);
    let mut search_start = 0;
    let mut match_level = PatchMatchLevel::Exact;
    let mut lines_added = 0;
    let mut lines_removed = 0;
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        let pattern = hunk.pattern_lines();
        let comparable = lines.texts();
        let mut anchor_starts = vec![search_start];
        let mut anchor_level = PatchMatchLevel::Exact;
        for anchor in hunk.context_anchors() {
            let mut next_starts = Vec::new();
            for start in anchor_starts {
                let (level, candidates) = find_anchor_candidates(&comparable, anchor, start);
                anchor_level = anchor_level.max(level);
                next_starts.extend(candidates.into_iter().map(|candidate| candidate + 1));
            }
            next_starts.sort_unstable();
            next_starts.dedup();
            if next_starts.is_empty() {
                return Err(PatchConflict::HunkFailed {
                    path: path.to_path_buf(),
                    hunk_index,
                    level: anchor_level,
                });
            }
            anchor_starts = next_starts;
        }

        let mut level = PatchMatchLevel::Exact;
        let mut candidates = Vec::new();
        for start in anchor_starts {
            let (candidate_level, found) =
                find_candidates(&comparable, &pattern, start, hunk.end_of_file());
            level = level.max(candidate_level);
            candidates.extend(found);
        }
        candidates.sort_unstable();
        candidates.dedup();
        match candidates.as_slice() {
            [] => {
                return Err(PatchConflict::HunkFailed {
                    path: path.to_path_buf(),
                    hunk_index,
                    level,
                });
            }
            [start] => {
                match_level = match_level.max(level);
                let before_len = hunk.context_before().len();
                let remove_start = start + before_len;
                let remove_end = remove_start + hunk.removed_lines().len();
                let ending = lines.ending_for_replacement(remove_start, remove_end);
                let added = hunk
                    .added_lines()
                    .iter()
                    .cloned()
                    .map(|text| SourceLine {
                        text,
                        ending: ending.clone(),
                    })
                    .collect::<Vec<_>>();
                lines.splice(remove_start..remove_end, added);
                // The following hunk may intentionally reuse this hunk's trailing context as its
                // leading context, so continue immediately after the replacement rather than after
                // the full matched pattern.
                search_start = remove_start + hunk.added_lines().len();
                lines_added += hunk.added_lines().len();
                lines_removed += hunk.removed_lines().len();
            }
            many => {
                return Err(PatchConflict::AmbiguousMatch {
                    path: path.to_path_buf(),
                    hunk_index,
                    candidate_count: many.len(),
                });
            }
        }
    }
    Ok(AppliedFileUpdate {
        content: lines.render(),
        match_level,
        lines_added,
        lines_removed,
    })
}

#[derive(Debug, Clone)]
struct SourceLine {
    text: String,
    ending: String,
}
#[derive(Debug, Clone)]
struct SourceFile {
    lines: Vec<SourceLine>,
    default_ending: String,
}

impl SourceFile {
    fn parse(content: &str) -> Self {
        let mut lines = Vec::new();
        let mut rest = content;
        while let Some(newline) = rest.find('\n') {
            let (segment, tail) = rest.split_at(newline + 1);
            let (text, ending) = match segment.strip_suffix("\r\n") {
                Some(text) => (text, "\r\n"),
                None => (&segment[..segment.len() - 1], "\n"),
            };
            lines.push(SourceLine {
                text: text.to_owned(),
                ending: ending.to_owned(),
            });
            rest = tail;
        }
        if !rest.is_empty() {
            lines.push(SourceLine {
                text: rest.to_owned(),
                ending: String::new(),
            });
        }
        let default_ending = lines
            .iter()
            .find_map(|line| (!line.ending.is_empty()).then(|| line.ending.clone()))
            .unwrap_or_else(|| "\n".to_owned());
        Self {
            lines,
            default_ending,
        }
    }
    fn texts(&self) -> Vec<String> {
        self.lines.iter().map(|line| line.text.clone()).collect()
    }
    fn ending_for_replacement(&self, start: usize, end: usize) -> String {
        if start < end {
            return self
                .lines
                .get(start)
                .map_or_else(|| self.default_ending.clone(), |line| line.ending.clone());
        }
        self.lines
            .get(start)
            .or_else(|| self.lines.get(end))
            .filter(|line| !line.ending.is_empty())
            .map_or_else(|| self.default_ending.clone(), |line| line.ending.clone())
    }
    fn splice(&mut self, range: std::ops::Range<usize>, mut replacement: Vec<SourceLine>) {
        if range.start == self.lines.len()
            && range.is_empty()
            && self.lines.last().is_some_and(|line| line.ending.is_empty())
            && !replacement.is_empty()
        {
            let ending = self.default_ending.clone();
            if let Some(last) = self.lines.last_mut() {
                last.ending = ending;
            }
            if let Some(last) = replacement.last_mut() {
                last.ending.clear();
            }
        }
        self.lines.splice(range, replacement);
    }
    fn render(&self) -> String {
        let mut content = String::new();
        for line in &self.lines {
            content.push_str(&line.text);
            content.push_str(&line.ending);
        }
        content
    }
}
