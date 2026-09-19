//! What a directory listing says about one path.

use serde::{Deserialize, Serialize};

use super::types::Permissions;

/// What kind of thing a path is.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A directory.
    Directory,
    /// An ordinary file.
    #[default]
    File,
    /// A symbolic link.
    Symlink,
    /// Something else: a socket, a device, a pipe.
    ///
    /// Present so a listing can report what it found rather than misfiling it as a file. A caller
    /// deciding whether to read something needs to be able to tell that it should not.
    Other,
}

impl EntryKind {
    /// The kind's wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
            Self::Symlink => "symlink",
            Self::Other => "other",
        }
    }
}

/// One path in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// The path, as the sandbox sees it.
    pub path: String,
    /// The permission bits it carries.
    pub permissions: Permissions,
    /// The account that owns it.
    pub owner: String,
    /// The group that owns it.
    pub group: String,
    /// Its size in bytes.
    pub size: u64,
    /// What kind of thing it is.
    pub kind: EntryKind,
}

impl FileEntry {
    /// Records one listed path.
    #[must_use]
    pub fn new(path: impl Into<String>, permissions: Permissions) -> Self {
        Self {
            path: path.into(),
            permissions,
            owner: String::new(),
            group: String::new(),
            size: 0,
            kind: EntryKind::File,
        }
    }

    /// Names the owning account and group.
    #[must_use]
    pub fn with_ownership(mut self, owner: impl Into<String>, group: impl Into<String>) -> Self {
        self.owner = owner.into();
        self.group = group.into();
        self
    }

    /// Records the size in bytes.
    #[must_use]
    pub const fn with_size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }

    /// Records what kind of thing this is.
    #[must_use]
    pub const fn with_kind(mut self, kind: EntryKind) -> Self {
        self.kind = kind;
        self
    }

    /// Whether this entry is a directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}
