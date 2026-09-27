//! What the skills capability indexes, and the contract of a source that loads skills on demand.
//!
//! A port of the protocol half of the reference's `sandbox/capabilities/skills.py`: the metadata
//! an index line is rendered from, the frontmatter reader both the capability and a host-backed
//! source parse `SKILL.md` with, and `LazySkillSource`. The capability itself lives with the other
//! sandbox capabilities in `ra-tools`; the source that reads a directory on the host lives with the
//! backends in `ra-sandbox`, because only a crate that owns a host filesystem can read one. This
//! module is what lets the two meet without either depending on the other.
//!
//! # Deviations from the reference
//!
//! - **What loading reports is an ordered record of strings, not a dictionary.** The reference's
//!   sources return `dict[str, str]`, whose keys keep the order they were written in and which a
//!   custom source may fill with keys of its own. [`SkillLoadResult`] keeps both properties; it
//!   serializes as a JSON object in that order.
//! - **Listing can fail.** The reference's `list_skill_metadata` returns a list and lets anything
//!   unexpected escape as an exception; [`LazySkillSource::list_skill_metadata`] returns that
//!   failure instead.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::ser::{Serialize, SerializeMap, Serializer};

use super::session::{SandboxResult, SandboxSession};
use super::types::User;
use super::workspace_paths::{PosixPath, SandboxPathGrant};

/// The file a skill directory is indexed by.
pub const SKILL_MARKDOWN: &str = "SKILL.md";

/// The description an index line carries when a skill does not give one.
pub const NO_SKILL_DESCRIPTION: &str = "No description provided.";

/// One skill as the index lists it: a name, what it is for, and where its root is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    name: String,
    description: String,
    path: PosixPath,
}

impl SkillMetadata {
    /// A skill whose root is `path`, relative to the workspace root.
    ///
    /// The path is read with backslashes as separators, as the reference reads a path of either
    /// flavour: a source that built it with Windows separators still names the same place.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, path: &str) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            path: PosixPath::coerce(path),
        }
    }

    /// The name a model asks for the skill by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the skill is for.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The skill's root, relative to the workspace root.
    #[must_use]
    pub const fn path(&self) -> &PosixPath {
        &self.path
    }

    /// The last component of the skill's root: the directory name a model may also ask by.
    #[must_use]
    pub fn directory_name(&self) -> &str {
        self.path
            .parts()
            .last()
            .copied()
            .filter(|part| !part.starts_with('/'))
            .unwrap_or_default()
    }
}

/// What loading a lazy skill reports: string fields, in the order the source wrote them.
///
/// The built-in source reports `status` (`loaded` or `already_loaded`), `skill_name` and `path`. A
/// custom source may report anything; the capability reads only `path`, and only to re-measure it
/// for a run with its own working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillLoadResult {
    fields: Vec<(String, String)>,
}

impl SkillLoadResult {
    /// A result with no fields.
    #[must_use]
    pub const fn new() -> Self {
        Self { fields: Vec::new() }
    }

    /// The built-in source's report: `status`, `skill_name` and `path`, in that order.
    #[must_use]
    pub fn loaded(status: &str, skill_name: &str, path: &str) -> Self {
        Self::new()
            .with_field("status", status)
            .with_field("skill_name", skill_name)
            .with_field("path", path)
    }

    /// Sets a field, in place when it is already there and at the end otherwise.
    ///
    /// In place because that is what the reference's `{**result, "path": ...}` does: the key keeps
    /// the position it had.
    #[must_use]
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        let value = value.into();
        match self
            .fields
            .iter_mut()
            .find(|(existing, _)| *existing == key)
        {
            Some((_, existing)) => *existing = value,
            None => self.fields.push((key, value)),
        }
        self
    }

    /// The value of a field, if the source reported it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value.as_str())
    }

    /// Every field, in order.
    #[must_use]
    pub fn fields(&self) -> &[(String, String)] {
        &self.fields
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for SkillLoadResult {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        iter.into_iter().fold(Self::new(), |result, (key, value)| {
            result.with_field(key, value)
        })
    }
}

impl Serialize for SkillLoadResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.fields.len()))?;
        for (key, value) in &self.fields {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// Where skill metadata comes from, and how one skill is put in a workspace when it is asked for.
///
/// The reference's `LazySkillSource`. A capability configured with one lists every skill in its
/// index up front but materializes none of them; the model calls `load_skill` for the one it
/// needs, and the source stages exactly that skill under the capability's skills path.
#[async_trait]
pub trait LazySkillSource: Send + Sync + 'static {
    /// The skills this source can load, each with its root under `skills_path`.
    ///
    /// `source_grants` are the manifest's extra path grants: a source that reads the host reads
    /// only what the manifest's own authority reaches.
    ///
    /// # Errors
    ///
    /// Returns what listing failed with. A source that simply has nothing to offer — its directory
    /// is missing or outside what the grants reach — returns an empty list instead.
    fn list_skill_metadata(
        &self,
        skills_path: &str,
        source_grants: &[SandboxPathGrant],
    ) -> SandboxResult<Vec<SkillMetadata>>;

    /// Stages the skill named `skill_name` into `session` under `skills_path`, as `user`.
    ///
    /// # Errors
    ///
    /// Returns a skills configuration failure when the skill cannot be found or named
    /// unambiguously, and whatever the session failed with while staging it.
    async fn load_skill(
        &self,
        skill_name: &str,
        session: &Arc<dyn SandboxSession>,
        skills_path: &str,
        user: Option<&User>,
    ) -> SandboxResult<SkillLoadResult>;
}

/// Reads the simple frontmatter a `SKILL.md` opens with.
///
/// The reference's `_parse_frontmatter`, which is not a YAML parser and is not meant to be one: a
/// first line of `---`, then `key: value` lines up to the next `---`, each trimmed, with one pair
/// of matching quotes taken off a value. Blank lines, comments and lines without a colon are
/// skipped; a later key replaces an earlier one. Anything without both fences has no frontmatter.
#[must_use]
pub fn parse_skill_frontmatter(markdown: &str) -> BTreeMap<String, String> {
    let lines = python_splitlines(markdown);
    if lines.first().is_none_or(|line| python_strip(line) != "---") {
        return BTreeMap::new();
    }
    let Some(end_index) = lines
        .iter()
        .skip(1)
        .position(|line| python_strip(line) == "---")
        .map(|index| index + 1)
    else {
        return BTreeMap::new();
    };

    let mut metadata = BTreeMap::new();
    for line in &lines[1..end_index] {
        let stripped = python_strip(line);
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let Some((key, value)) = stripped.split_once(':') else {
            continue;
        };
        let mut value = python_strip(value);
        let bytes = value.as_bytes();
        if bytes.len() >= 2
            && bytes[0] == bytes[bytes.len() - 1]
            && matches!(bytes[0], b'\'' | b'"')
        {
            value = &value[1..value.len() - 1];
        }
        metadata.insert(python_strip(key).to_owned(), value.to_owned());
    }
    metadata
}

/// Whether Python's `str.isspace` holds for a character: Unicode whitespace, plus the four
/// information separators Python also counts.
fn is_python_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

/// Python's `str.strip()` with no argument.
fn python_strip(text: &str) -> &str {
    text.trim_matches(is_python_space)
}

/// Python's `str.splitlines()`: every line boundary Python recognises, `\r\n` as one, and no
/// trailing empty line for text that ends with a boundary.
fn python_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        let is_boundary = matches!(
            character,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !is_boundary {
            continue;
        }
        lines.push(&text[start..index]);
        let mut next = index + character.len_utf8();
        if character == '\r' && characters.peek().is_some_and(|(_, next)| *next == '\n') {
            characters.next();
            next += 1;
        }
        start = next;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}
