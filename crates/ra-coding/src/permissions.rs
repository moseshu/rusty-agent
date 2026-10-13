//! The permissions an agent asks for beyond its thread's sandbox, and the policy that decides what
//! a grant covers: Codex's permission profile (`protocol/src/models.rs`' `AdditionalPermissionProfile`,
//! `FileSystemPermissions` and `NetworkPermissions`, `protocol/src/permissions.rs`' file system
//! entries and policy context) and its transforms (`sandboxing/src/policy_transforms.rs`).
//!
//! A profile names file system entries — a path, a glob, or a special path such as the project
//! roots — each with an access, and whether network access is wanted. [`normalize_additional_permissions`]
//! checks what an agent asks for, [`materialize_additional_permissions_with_context`] resolves what
//! depends on where it was asked, [`intersect_permission_profiles_with_context`] keeps the part of
//! a client's grant the request covers, and [`merge_permission_profiles`] adds grants together.
//!
//! # Differences from Codex
//!
//! - Paths are native absolute paths rather than Codex's `PathUri`: this runtime executes only on
//!   its own host, where Codex's one environment is `local`, so a path never belongs to another
//!   host's convention. Only POSIX paths are handled; Codex's Windows conventions — drive and UNC
//!   roots, case-insensitive segments — are not ported, and neither are its opaque fallback paths,
//!   which only arise for paths a URI cannot spell.
//! - A path that cannot be resolved is refused where Codex refuses one of another convention.
//! - Native `PathBuf` values can be constructed without the invariants Codex's path types enforce.
//!   Entry constructors normalize absolute literals; context-aware transforms reject unresolved
//!   literals before evaluating authority, preserving unresolvable denies rather than dropping them.
//! - [`AbsolutePathBufGuard`] retains Codex's synchronous base and home deserialization scopes. It
//!   is thread-bound so Rust async hosts cannot move its thread-local state to another worker.

pub(crate) mod path;
mod transforms;

use std::{num::NonZeroUsize, path::PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser};

pub use self::path::AbsolutePathBufGuard;
pub use self::transforms::{
    FileSystemSandboxPolicyContext, intersect_permission_profiles_with_context,
    materialize_additional_permissions_with_context, merge_permission_profiles,
    normalize_additional_permissions, normalize_additional_permissions_with_context,
};
pub(crate) use self::transforms::{is_read_denied, resolve_access, resolved_entries};

/// The access an entry gives: Codex's `FileSystemAccessMode`.
///
/// When two equally specific entries name the same path, the later in this order wins: `deny`
/// beats `write`, and `write` beats `read`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileSystemAccessMode {
    /// Reading.
    Read,
    /// Reading and writing.
    Write,
    /// Neither; `none` is read as this, as Codex still accepts it.
    #[serde(alias = "none")]
    Deny,
}

impl FileSystemAccessMode {
    /// Whether the access lets a path be read.
    #[must_use]
    pub const fn can_read(self) -> bool {
        !matches!(self, Self::Deny)
    }

    /// Whether the access lets a path be written.
    #[must_use]
    pub const fn can_write(self) -> bool {
        matches!(self, Self::Write)
    }
}

/// A path named by what it is rather than where: Codex's `FileSystemSpecialPath`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileSystemSpecialPath {
    /// The root of the file system.
    Root,
    /// The minimal set of system paths a platform sandbox allows.
    Minimal,
    /// The project roots, or a path below them; `current_working_directory` is read as this.
    #[serde(alias = "current_working_directory")]
    ProjectRoots {
        /// The path below each root, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
    },
    /// The directory `TMPDIR` names.
    Tmpdir,
    /// `/tmp`.
    SlashTmp,
    /// A token this build does not know, kept so a newer configuration is ignored rather than
    /// refused.
    Unknown {
        /// The token.
        path: String,
        /// The path below it, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
    },
}

impl FileSystemSpecialPath {
    /// The project roots, or `subpath` below them.
    #[must_use]
    pub const fn project_roots(subpath: Option<String>) -> Self {
        Self::ProjectRoots { subpath }
    }
}

/// What an entry names: Codex's `FileSystemPath`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileSystemPath {
    /// An absolute path, lexically normal.
    Path {
        /// The path.
        #[serde(deserialize_with = "deserialize_absolute_path")]
        path: PathBuf,
    },
    /// A git-style glob pattern; only a deny entry may name one.
    GlobPattern {
        /// The pattern.
        pattern: String,
    },
    /// A special path.
    Special {
        /// Which.
        value: FileSystemSpecialPath,
    },
}

impl FileSystemPath {
    /// The entry for `path`, made absolute and normal as Codex's `AbsolutePathBuf` makes it.
    ///
    /// # Errors
    ///
    /// Returns the error reading the current directory, which a relative `path` is resolved
    /// against.
    pub fn path(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        Ok(Self::Path {
            path: path::from_absolute_path(&path.into())?,
        })
    }

    /// The entry for the special path `value`.
    #[must_use]
    pub const fn special(value: FileSystemSpecialPath) -> Self {
        Self::Special { value }
    }

    /// The entry for the glob `pattern`.
    #[must_use]
    pub fn glob(pattern: impl Into<String>) -> Self {
        Self::GlobPattern {
            pattern: pattern.into(),
        }
    }
}

fn deserialize_absolute_path<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<PathBuf, D::Error> {
    let path = PathBuf::deserialize(deserializer)?;
    path::deserialize_absolute_path(&path).map_err(de::Error::custom)
}

/// What an entry does when its path does not exist: Codex's
/// `FileSystemSandboxEntryMissingPathBehavior`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSystemSandboxEntryMissingPathBehavior {
    /// The entry is skipped.
    Skip,
}

/// One file system entry: Codex's `FileSystemSandboxEntry`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileSystemSandboxEntry {
    path: FileSystemPath,
    access: FileSystemAccessMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    missing_path_behavior: Option<FileSystemSandboxEntryMissingPathBehavior>,
}

impl FileSystemSandboxEntry {
    /// `access` to `path`, normalizing absolute literal paths lexically.
    /// Unresolvable literals remain represented so a deny is not silently discarded; contextual
    /// transforms refuse them before evaluating authority.
    #[must_use]
    pub fn new(path: FileSystemPath, access: FileSystemAccessMode) -> Self {
        Self {
            path,
            access,
            missing_path_behavior: None,
        }
        .normalize_path()
    }

    /// `access` to `path`, skipped when the path does not exist.
    #[must_use]
    pub fn skip_missing_path(path: FileSystemPath, access: FileSystemAccessMode) -> Self {
        Self {
            path,
            access,
            missing_path_behavior: Some(FileSystemSandboxEntryMissingPathBehavior::Skip),
        }
        .normalize_path()
    }

    /// What the entry names.
    #[must_use]
    pub const fn path(&self) -> &FileSystemPath {
        &self.path
    }

    /// The access it gives.
    #[must_use]
    pub const fn access(&self) -> FileSystemAccessMode {
        self.access
    }

    /// What it does when its path does not exist, if that is set.
    #[must_use]
    pub const fn missing_path_behavior(&self) -> Option<FileSystemSandboxEntryMissingPathBehavior> {
        self.missing_path_behavior
    }

    /// Whether it is skipped when its path does not exist.
    #[must_use]
    pub fn skips_missing_path(&self) -> bool {
        self.missing_path_behavior == Some(FileSystemSandboxEntryMissingPathBehavior::Skip)
    }

    pub(crate) fn with_path(&self, path: FileSystemPath) -> Self {
        Self {
            path,
            access: self.access,
            missing_path_behavior: self.missing_path_behavior,
        }
        .normalize_path()
    }

    fn normalize_path(mut self) -> Self {
        if let FileSystemPath::Path { path } = &mut self.path
            && let Some(normalized) = path::normalize(path)
        {
            *path = normalized;
        }
        self
    }
}

/// The file system part of a profile: Codex's `FileSystemPermissions`.
///
/// It is written as Codex writes it: as `{read, write}` lists of paths when every entry reads or
/// writes a path and no glob scan depth is set, and as `{entries, glob_scan_max_depth}` otherwise.
/// Either is read.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct FileSystemPermissions {
    entries: Vec<FileSystemSandboxEntry>,
    glob_scan_max_depth: Option<NonZeroUsize>,
}

impl FileSystemPermissions {
    /// The permissions `entries` give, with absolute literal paths normalized lexically.
    #[must_use]
    pub fn new(entries: Vec<FileSystemSandboxEntry>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(FileSystemSandboxEntry::normalize_path)
                .collect(),
            glob_scan_max_depth: None,
        }
    }

    /// Reading the paths `read` and writing the paths `write`: Codex's `from_read_write_roots`.
    #[must_use]
    pub fn from_read_write_roots(read: Option<Vec<PathBuf>>, write: Option<Vec<PathBuf>>) -> Self {
        let entries = read
            .into_iter()
            .flatten()
            .map(|path| (path, FileSystemAccessMode::Read))
            .chain(
                write
                    .into_iter()
                    .flatten()
                    .map(|path| (path, FileSystemAccessMode::Write)),
            )
            .map(|(path, access)| {
                FileSystemSandboxEntry::new(FileSystemPath::Path { path }, access)
            })
            .collect();
        Self::new(entries)
    }

    /// Sets how deep a deny glob's scan goes.
    #[must_use]
    pub const fn with_glob_scan_max_depth(mut self, depth: Option<NonZeroUsize>) -> Self {
        self.glob_scan_max_depth = depth;
        self
    }

    /// The entries.
    #[must_use]
    pub fn entries(&self) -> &[FileSystemSandboxEntry] {
        &self.entries
    }

    /// How deep a deny glob's scan goes, if bounded.
    #[must_use]
    pub const fn glob_scan_max_depth(&self) -> Option<NonZeroUsize> {
        self.glob_scan_max_depth
    }

    /// Whether it gives nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The `{read, write}` lists it is written as, if it can be: Codex's `as_legacy_permissions`.
    fn as_legacy_permissions(&self) -> Option<LegacyReadWriteRoots> {
        if self.glob_scan_max_depth.is_some() {
            return None;
        }
        let mut read = Vec::new();
        let mut write = Vec::new();
        for entry in &self.entries {
            let FileSystemPath::Path { path } = &entry.path else {
                return None;
            };
            match entry.access {
                FileSystemAccessMode::Read => read.push(path.clone()),
                FileSystemAccessMode::Write => write.push(path.clone()),
                FileSystemAccessMode::Deny => return None,
            }
        }
        Some(LegacyReadWriteRoots {
            read: (!read.is_empty()).then_some(read),
            write: (!write.is_empty()).then_some(write),
        })
    }
}

/// Codex's `LegacyReadWriteRoots`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyReadWriteRoots {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_absolute_paths"
    )]
    read: Option<Vec<PathBuf>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_absolute_paths"
    )]
    write: Option<Vec<PathBuf>>,
}

fn deserialize_absolute_paths<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<PathBuf>>, D::Error> {
    Option::<Vec<PathBuf>>::deserialize(deserializer)?
        .map(|paths| {
            paths
                .iter()
                .map(|path| path::deserialize_absolute_path(path).map_err(de::Error::custom))
                .collect()
        })
        .transpose()
}

/// Codex's `CanonicalFileSystemPermissions`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalFileSystemPermissions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    entries: Vec<FileSystemSandboxEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    glob_scan_max_depth: Option<NonZeroUsize>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FileSystemPermissionsDe {
    Canonical(CanonicalFileSystemPermissions),
    Legacy(LegacyReadWriteRoots),
}

impl Serialize for FileSystemPermissions {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.as_legacy_permissions() {
            Some(legacy) => legacy.serialize(serializer),
            None => CanonicalFileSystemPermissions {
                entries: self.entries.clone(),
                glob_scan_max_depth: self.glob_scan_max_depth,
            }
            .serialize(serializer)
            .map_err(ser::Error::custom),
        }
    }
}

impl<'de> Deserialize<'de> for FileSystemPermissions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match FileSystemPermissionsDe::deserialize(deserializer)? {
            FileSystemPermissionsDe::Canonical(CanonicalFileSystemPermissions {
                entries,
                glob_scan_max_depth,
            }) => Self {
                entries,
                glob_scan_max_depth,
            },
            FileSystemPermissionsDe::Legacy(LegacyReadWriteRoots { read, write }) => {
                Self::from_read_write_roots(read, write)
            }
        })
    }
}

/// The network part of a profile: Codex's `NetworkPermissions`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetworkPermissions {
    enabled: Option<bool>,
}

impl NetworkPermissions {
    /// Network access asked for or given when `enabled`, refused when not, or left unsaid.
    #[must_use]
    pub const fn new(enabled: Option<bool>) -> Self {
        Self { enabled }
    }

    /// Whether network access is asked for or given, if it is said.
    #[must_use]
    pub const fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    /// Whether it says nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.enabled.is_none()
    }

    pub(crate) const fn grants(&self) -> bool {
        matches!(self.enabled, Some(true))
    }
}

/// Permissions beyond a sandbox, asked for or granted: Codex's `AdditionalPermissionProfile`,
/// which its `RequestPermissionProfile` mirrors field for field.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AdditionalPermissionProfile {
    #[serde(default)]
    network: Option<NetworkPermissions>,
    #[serde(default)]
    file_system: Option<FileSystemPermissions>,
}

impl AdditionalPermissionProfile {
    /// Nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the network part.
    #[must_use]
    pub fn with_network(mut self, network: NetworkPermissions) -> Self {
        self.network = Some(network);
        self
    }

    /// Sets the file system part.
    #[must_use]
    pub fn with_file_system(mut self, file_system: FileSystemPermissions) -> Self {
        self.file_system = Some(file_system);
        self
    }

    /// The network part, if any.
    #[must_use]
    pub const fn network(&self) -> Option<&NetworkPermissions> {
        self.network.as_ref()
    }

    /// The file system part, if any.
    #[must_use]
    pub const fn file_system(&self) -> Option<&FileSystemPermissions> {
        self.file_system.as_ref()
    }

    /// Whether it asks for or gives nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.network.is_none() && self.file_system.is_none()
    }
}
