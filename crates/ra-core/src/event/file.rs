//! Host events recording the file reads and file changes a tool actually performed.
//!
//! # Why these exist as their own family
//!
//! A session log that records every command and no file access can answer "what did this run do?"
//! only for the half of the work that went through a shell. The framework's own read and edit entry
//! points went straight to the filesystem and told nobody, so a run that read twenty files and
//! rewrote three left the same trace as a run that read nothing.
//!
//! They are not execution events. Nothing here has a process, a process group, or an exit status,
//! and folding them into that family would have meant a session identifier that identifies nothing
//! on every one of them.
//!
//! # What these events promise, and what they do not
//!
//! They are a record of what the file tools did: the path, the range, the kind of change, and
//! whether it landed. **They are not a complete audit of the run's file access** — a command run
//! through the execution tool can read and write whatever the sandbox allows without producing one
//! of these — and they are **not enough to reconstruct file contents**: a read event says how much
//! was read, never what was in it. Both limits are deliberate. The first is a property of where
//! these are emitted from; the second keeps a log that hosts persist from becoming a second copy of
//! the workspace, and keeps file contents out of a channel whose retention nobody has reasoned
//! about.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    compat::{SchemaVersion, Unknown},
    item::CallId,
};

/// The current schema version for file events.
pub const FILE_EVENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn default_schema_version() -> SchemaVersion {
    FILE_EVENT_SCHEMA_VERSION
}

/// A file access a tool performed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    /// A file was read.
    Read(FileReadEvent),
    /// A file was created, modified, deleted, or moved.
    Changed(FileChangedEvent),
    /// Forward-compatible unknown file event kind.
    Unknown(serde_json::Value),
}

impl Serialize for FileEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Known<'a> {
            Read(&'a FileReadEvent),
            Changed(&'a FileChangedEvent),
        }

        match self {
            Self::Read(event) => Known::Read(event).serialize(serializer),
            Self::Changed(event) => Known::Changed(event).serialize(serializer),
            Self::Unknown(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for FileEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut val = serde_json::Value::deserialize(deserializer)?;
        let kind = val.as_object_mut().and_then(|map| map.remove("kind"));
        match kind {
            Some(serde_json::Value::String(ref name)) if name == "read" => {
                serde_json::from_value(val)
                    .map(Self::Read)
                    .map_err(serde::de::Error::custom)
            }
            Some(serde_json::Value::String(ref name)) if name == "changed" => {
                serde_json::from_value(val)
                    .map(Self::Changed)
                    .map_err(serde::de::Error::custom)
            }
            other => {
                if let Some(kind) = other
                    && let Some(map) = val.as_object_mut()
                {
                    map.insert("kind".to_owned(), kind);
                }
                Ok(Self::Unknown(val))
            }
        }
    }
}

/// What a change did to a file.
///
/// An **open label**, on the same terms as
/// [`ExecEvictionReason`](crate::event::exec::ExecEvictionReason): a newer build naming a change
/// this one has never heard of must still be readable, because the alternative is a session log
/// that a version bump turns into an error.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChangeKind {
    /// The file did not exist before.
    Added,
    /// The file existed and its contents changed.
    Updated,
    /// The file was removed.
    Deleted,
    /// The file was written at a new path and removed from the old one.
    Moved,
    /// A change this build has no variant for, kept verbatim.
    Custom(Cow<'static, str>),
}

impl FileChangeKind {
    /// The name of this change, byte-identical to its serialized form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Deleted => "deleted",
            Self::Moved => "moved",
            Self::Custom(name) => name,
        }
    }

    fn known(name: &str) -> Option<Self> {
        match name {
            "added" => Some(Self::Added),
            "updated" => Some(Self::Updated),
            "deleted" => Some(Self::Deleted),
            "moved" => Some(Self::Moved),
            _ => None,
        }
    }
}

impl From<&str> for FileChangeKind {
    fn from(name: &str) -> Self {
        Self::known(name).unwrap_or_else(|| Self::Custom(Cow::Owned(name.to_owned())))
    }
}

impl From<String> for FileChangeKind {
    fn from(name: String) -> Self {
        Self::known(&name).unwrap_or(Self::Custom(Cow::Owned(name)))
    }
}

impl std::fmt::Display for FileChangeKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for FileChangeKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FileChangeKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from(String::deserialize(deserializer)?))
    }
}

/// Emitted when a tool read a file.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReadEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    call_id: CallId,
    path: String,
    bytes_read: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    first_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_lines: Option<u32>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl FileReadEvent {
    /// Records a read of `path` that took `bytes_read` bytes off the file.
    #[must_use]
    pub fn new(call_id: CallId, path: impl Into<String>, bytes_read: u64) -> Self {
        Self {
            schema_version: FILE_EVENT_SCHEMA_VERSION,
            call_id,
            path: path.into(),
            bytes_read,
            first_line: None,
            line_count: None,
            total_lines: None,
            unknown: Unknown::new(),
        }
    }

    /// Records which lines the read returned, and how many the file has.
    #[must_use]
    pub const fn with_line_window(
        mut self,
        first_line: u32,
        line_count: u32,
        total_lines: u32,
    ) -> Self {
        self.first_line = Some(first_line);
        self.line_count = Some(line_count);
        self.total_lines = Some(total_lines);
        self
    }

    /// The tool call this read belongs to.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The path as the caller named it.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Bytes taken off the file.
    #[must_use]
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// First line returned, 1-based, for a read that returned lines.
    #[must_use]
    pub const fn first_line(&self) -> Option<u32> {
        self.first_line
    }

    /// How many lines were returned.
    #[must_use]
    pub const fn line_count(&self) -> Option<u32> {
        self.line_count
    }

    /// How many lines the file has.
    #[must_use]
    pub const fn total_lines(&self) -> Option<u32> {
        self.total_lines
    }

    /// Unknown fields preserved while reading a newer record.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// Emitted when a tool changed a file, once per file the change reached.
///
/// **One event per file, emitted as each one lands.** A patch that writes three files and then
/// fails on the fourth has changed three files, and a record written only on overall success would
/// describe that run as having changed none.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangedEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    call_id: CallId,
    path: String,
    change: FileChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    moved_to: Option<String>,
    lines_added: u64,
    lines_removed: u64,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl FileChangedEvent {
    /// Records a change that has already reached the filesystem.
    #[must_use]
    pub fn new(call_id: CallId, path: impl Into<String>, change: FileChangeKind) -> Self {
        Self {
            schema_version: FILE_EVENT_SCHEMA_VERSION,
            call_id,
            path: path.into(),
            change,
            moved_to: None,
            lines_added: 0,
            lines_removed: 0,
            unknown: Unknown::new(),
        }
    }

    /// Records where a moved file now lives.
    #[must_use]
    pub fn with_moved_to(mut self, path: impl Into<String>) -> Self {
        self.moved_to = Some(path.into());
        self
    }

    /// Records how many lines the change added and removed.
    #[must_use]
    pub const fn with_line_counts(mut self, added: u64, removed: u64) -> Self {
        self.lines_added = added;
        self.lines_removed = removed;
        self
    }

    /// The tool call this change belongs to.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The path that was changed.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// What the change did.
    #[must_use]
    pub const fn change(&self) -> &FileChangeKind {
        &self.change
    }

    /// Where a moved file now lives.
    #[must_use]
    pub fn moved_to(&self) -> Option<&str> {
        self.moved_to.as_deref()
    }

    /// Lines this change added.
    #[must_use]
    pub const fn lines_added(&self) -> u64 {
        self.lines_added
    }

    /// Lines this change removed.
    #[must_use]
    pub const fn lines_removed(&self) -> u64 {
        self.lines_removed
    }

    /// Unknown fields preserved while reading a newer record.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
