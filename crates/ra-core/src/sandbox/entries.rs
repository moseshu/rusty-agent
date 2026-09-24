//! What a manifest declares should exist in the workspace, one entry at a time.
//!
//! An entry is a declaration, not an action: it says *what* should be at a path, and a backend that
//! owns a filesystem works out how to put it there. Five kinds are built in — a directory, a file
//! with inline bytes, a file or directory copied from the host, and a git checkout — plus the six
//! mount types in [`mounts`], which attach storage that belongs to somebody else. Every entry
//! carries the same handful of facts about the result: who owns it, what it may be read and written
//! by, whether it survives a stop, and what it is for.
//!
//! # An open family, closed by the host
//!
//! Entry types are extensible upstream, so a closed enum would shut out a host that declares its
//! own. Parsing therefore goes through a [`TypeRegistry`], exactly as client options, snapshots and
//! session states do: [`builtin_entry_registry`] holds the kinds modelled here, a host adds its own,
//! and a type nobody claims is refused rather than carried as something materialization would then
//! have to guess at. A registered type this crate does not model comes back as
//! [`EntryContent::Extension`] with its fields intact.
//!
//! # Declaration only
//!
//! Nothing here reads a file, clones a repository or checks that a source exists. The validation
//! that can be done on the declaration alone is done here — a path that escapes the workspace is
//! refused whatever a filesystem would have said about it — and the rest belongs to the backend:
//! whether `src` exists, whether it is a symlink, whether the git ref resolves.

use std::collections::BTreeMap;

use serde::{Serialize, Serializer};
use serde_json::{Map as JsonMap, Value};

use super::error::{ErrorCode, OpName, SandboxError};
use super::registry::{DiscriminatedPayload, RegistryError, RegistryKind, TypeRegistry};
use super::types::{FileMode, Group, Permissions, User};
use super::workspace_paths::{PosixPath, windows_absolute_path};

pub mod mounts;

use mounts::{BUILTIN_MOUNT_TYPES, Mount};

use super::manifest::ManifestRegistries;

/// The discriminator of a directory.
pub const DIR_ENTRY_TYPE: &str = "dir";
/// The discriminator of a file with inline content.
pub const FILE_ENTRY_TYPE: &str = "file";
/// The discriminator of a file copied from the host.
pub const LOCAL_FILE_ENTRY_TYPE: &str = "local_file";
/// The discriminator of a directory copied from the host.
pub const LOCAL_DIR_ENTRY_TYPE: &str = "local_dir";
/// The discriminator of a git checkout.
pub const GIT_REPO_ENTRY_TYPE: &str = "git_repo";

/// The host a git entry clones from when it does not say.
pub const DEFAULT_GIT_HOST: &str = "github.com";

/// Fields every entry carries, and which therefore never live among an extension's own.
///
/// A field in both places could disagree, and rendering would have to pick a winner.
const COMMON_FIELDS: [&str; 5] = ["description", "ephemeral", "group", "is_dir", "permissions"];

/// The family that routes manifest entries.
#[must_use]
pub fn entry_kind() -> RegistryKind {
    RegistryKind::new("artifact", "BaseEntry")
}

/// A registry holding the entry kinds this crate models.
///
/// A host adds its own types to the returned registry. Handing back a fresh one each time is what
/// keeps "which entry types exist" a property of the host's assembly rather than of which features
/// happened to be compiled in.
#[must_use]
pub fn builtin_entry_registry() -> TypeRegistry {
    let mut registry = TypeRegistry::new(entry_kind());
    let builtins = [
        (
            DIR_ENTRY_TYPE,
            validate_dir as fn(&DiscriminatedPayload) -> Result<(), String>,
        ),
        (FILE_ENTRY_TYPE, validate_file),
        (LOCAL_FILE_ENTRY_TYPE, validate_local_file),
        (LOCAL_DIR_ENTRY_TYPE, validate_local_dir),
        (GIT_REPO_ENTRY_TYPE, validate_git_repo),
    ];
    for (type_name, validate) in builtins {
        let registered = registry.register_with(type_name, BUILTIN_REGISTRANT, move |payload| {
            validate(&payload)?;
            Ok(payload)
        });
        debug_assert!(registered.is_ok(), "built-in entry types must be distinct");
    }
    // A mount is registered here but validated in `Entry::from_payload`, which is where the mount
    // strategy registry is in reach: the provider's fields, the strategy, and whether the provider
    // supports that strategy are one question, and half of it cannot be answered from a registry
    // that only knows entry types.
    for mount_type in BUILTIN_MOUNT_TYPES {
        let registered = registry.register(mount_type, BUILTIN_REGISTRANT);
        debug_assert!(registered.is_ok(), "built-in mount types must be distinct");
    }
    registry
}

/// What the built-in entry types register themselves as.
const BUILTIN_REGISTRANT: &str = "ra_core::sandbox::entries";

/// The account a materialized entry is handed to.
///
/// The reference names this field `group` but accepts either, because what it does with it is
/// `chgrp`: on a system where every account has a group of its own, naming the user names that
/// group. Keeping both shapes means a manifest does not have to know which of the two it is looking
/// at.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum EntryOwner {
    /// A named group.
    Group(Group),
    /// A user, whose name doubles as a group name.
    User(User),
}

/// What an entry puts at its path.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryContent {
    /// A directory, and what should be inside it.
    Dir {
        /// Entries to materialize under this directory, keyed by name.
        children: BTreeMap<String, Entry>,
    },
    /// A file whose bytes the manifest carries.
    File {
        /// The file's contents.
        content: Vec<u8>,
    },
    /// One file copied from the host that built the manifest.
    LocalFile {
        /// The source, resolved by the backend against the directory it materializes from.
        src: String,
    },
    /// A directory copied from the host, or just created when there is no source.
    LocalDir {
        /// The source, or `None` to create an empty directory.
        src: Option<String>,
    },
    /// A git checkout.
    GitRepo {
        /// The host to clone from.
        host: String,
        /// The repository path on that host, conventionally `owner/name`.
        repo: String,
        /// A tag, branch or commit. Named `ref` on the wire, which is a keyword here.
        reference: String,
        /// A directory inside the repository to take instead of the whole of it.
        subpath: Option<String>,
    },
    /// External storage attached inside the workspace.
    ///
    /// Boxed: a mount carries a provider's whole credential set, and leaving it inline would make
    /// every entry — most of which are a path and a few bytes — as large as the largest mount.
    Mount(Box<Mount>),
    /// An entry type a host registered and this crate does not model.
    ///
    /// Carried verbatim so a manifest survives a process that does not understand every type in it.
    Extension(DiscriminatedPayload),
}

impl EntryContent {
    /// The discriminator this content is written under.
    #[must_use]
    pub fn type_name(&self) -> &str {
        match self {
            Self::Dir { .. } => DIR_ENTRY_TYPE,
            Self::File { .. } => FILE_ENTRY_TYPE,
            Self::LocalFile { .. } => LOCAL_FILE_ENTRY_TYPE,
            Self::LocalDir { .. } => LOCAL_DIR_ENTRY_TYPE,
            Self::GitRepo { .. } => GIT_REPO_ENTRY_TYPE,
            Self::Mount(mount) => mount.type_name(),
            Self::Extension(payload) => payload.type_name(),
        }
    }

    /// Whether this content puts a directory at its path.
    fn defaults_to_directory(&self) -> bool {
        matches!(
            self,
            Self::Dir { .. } | Self::LocalDir { .. } | Self::GitRepo { .. } | Self::Mount(_)
        )
    }

    /// Whether this content is external storage that must never be persisted.
    pub(crate) const fn is_mount(&self) -> bool {
        matches!(self, Self::Mount(_))
    }
}

/// One thing a manifest declares should exist in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    description: Option<String>,
    ephemeral: bool,
    group: Option<EntryOwner>,
    is_dir: bool,
    permissions: Permissions,
    content: EntryContent,
}

/// The permissions a materialized entry carries unless the manifest says otherwise.
///
/// `0755`: the owner may do anything, everyone else may read and traverse. Note that this is wider
/// than [`Permissions`]'s own default of `0700` — content declared by a manifest is meant to be
/// reachable by whatever the sandbox runs as, which is not always the account that materialized it.
#[must_use]
pub fn default_entry_permissions() -> Permissions {
    Permissions::default()
        .owner_can(FileMode::All)
        .group_can(FileMode::Read | FileMode::Exec)
        .others_can(FileMode::Read | FileMode::Exec)
}

impl Entry {
    /// Declares an entry with the default ownership and permissions for its content.
    #[must_use]
    pub fn new(content: EntryContent) -> Self {
        let is_dir = content.defaults_to_directory();
        let mut permissions = default_entry_permissions();
        permissions.directory = is_dir;
        Self {
            description: None,
            // A mount is somebody else's storage attached at a path, not workspace state. A snapshot
            // that carried one would record it as if it were, so this is not a default a caller may
            // change: `ephemeral` refuses to go false for a mount.
            ephemeral: content.is_mount(),
            group: None,
            is_dir,
            permissions,
            content,
        }
    }

    /// Declares external storage attached inside the workspace.
    ///
    /// Always ephemeral, and always at the entry's default permissions — see the module docs on why
    /// neither is a caller's to set.
    #[must_use]
    pub fn mount(mount: Mount) -> Self {
        Self::new(EntryContent::Mount(Box::new(mount)))
    }

    /// Declares a directory.
    #[must_use]
    pub fn dir() -> Self {
        Self::new(EntryContent::Dir {
            children: BTreeMap::new(),
        })
    }

    /// Declares a file whose bytes the manifest carries.
    #[must_use]
    pub fn file(content: impl Into<Vec<u8>>) -> Self {
        Self::new(EntryContent::File {
            content: content.into(),
        })
    }

    /// Declares one file copied from the host.
    #[must_use]
    pub fn local_file(src: impl Into<String>) -> Self {
        Self::new(EntryContent::LocalFile { src: src.into() })
    }

    /// Declares a directory copied from the host, or created empty when `src` is `None`.
    #[must_use]
    pub fn local_dir(src: Option<String>) -> Self {
        Self::new(EntryContent::LocalDir { src })
    }

    /// Declares a git checkout of one ref.
    #[must_use]
    pub fn git_repo(repo: impl Into<String>, reference: impl Into<String>) -> Self {
        Self::new(EntryContent::GitRepo {
            host: DEFAULT_GIT_HOST.to_owned(),
            repo: repo.into(),
            reference: reference.into(),
            subpath: None,
        })
    }

    /// Clones from a host other than the default.
    ///
    /// Ignored by content that is not a git checkout.
    #[must_use]
    pub fn from_git_host(mut self, git_host: impl Into<String>) -> Self {
        if let EntryContent::GitRepo { host, .. } = &mut self.content {
            *host = git_host.into();
        }
        self
    }

    /// Takes one directory out of the repository instead of the whole of it.
    ///
    /// Ignored by content that is not a git checkout.
    #[must_use]
    pub fn with_subpath(mut self, repo_subpath: impl Into<String>) -> Self {
        if let EntryContent::GitRepo { subpath, .. } = &mut self.content {
            *subpath = Some(repo_subpath.into());
        }
        self
    }

    /// Declares one entry inside this directory.
    ///
    /// Ignored by content that is not a directory: a file has nowhere to put a child, and inventing
    /// somewhere would materialize content the manifest never said was there.
    #[must_use]
    pub fn with_child(mut self, name: impl Into<String>, entry: Self) -> Self {
        if let EntryContent::Dir { children } = &mut self.content {
            children.insert(name.into(), entry);
        }
        self
    }

    /// Records what this entry is for.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Declares whether this entry survives a stop.
    ///
    /// An ephemeral entry is left out of what is persisted and materialized again on the next start,
    /// which is what a cache or a credential file wants: it has to be there, and it must not be what
    /// a snapshot carries forward.
    ///
    /// A mount ignores this and stays ephemeral. It is external storage attached at a path, so there
    /// is nothing of the workspace's to persist and a snapshot that took one would be recording
    /// somebody else's data.
    #[must_use]
    pub const fn ephemeral(mut self, ephemeral: bool) -> Self {
        if !self.content.is_mount() {
            self.ephemeral = ephemeral;
        }
        self
    }

    /// Hands the materialized entry to an account.
    #[must_use]
    pub fn owned_by(mut self, group: EntryOwner) -> Self {
        self.group = Some(group);
        self
    }

    /// Sets the permissions the materialized entry carries.
    ///
    /// Content that is a directory keeps its directory flag whatever the argument said: whether a
    /// path is a directory follows from what is being put there, and a mode that claimed otherwise
    /// would render a directory's permissions as a file's.
    #[must_use]
    pub fn with_permissions(mut self, permissions: Permissions) -> Self {
        self.permissions = permissions;
        if self.content.defaults_to_directory() {
            self.permissions.directory = true;
        }
        self
    }

    /// Declares whether this entry is a directory in the sandbox filesystem.
    ///
    /// Independent of the permission bits, as it is upstream: the flag describes what the sandbox
    /// filesystem should see, and the mode describes what may be done with it.
    #[must_use]
    pub const fn as_directory(mut self, is_dir: bool) -> Self {
        self.is_dir = is_dir;
        self
    }

    /// The discriminator this entry is written under.
    #[must_use]
    pub fn entry_type(&self) -> &str {
        self.content.type_name()
    }

    /// What this entry puts at its path.
    #[must_use]
    pub const fn content(&self) -> &EntryContent {
        &self.content
    }

    /// What this entry is for.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Whether this entry is left out of what is persisted.
    #[must_use]
    pub const fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// The account the materialized entry is handed to.
    #[must_use]
    pub const fn group(&self) -> Option<&EntryOwner> {
        self.group.as_ref()
    }

    /// Whether this entry is a directory in the sandbox filesystem.
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// The permissions the materialized entry carries.
    #[must_use]
    pub const fn permissions(&self) -> Permissions {
        self.permissions
    }

    /// What is declared inside this entry, or `None` for content that holds nothing.
    #[must_use]
    pub const fn children(&self) -> Option<&BTreeMap<String, Self>> {
        match &self.content {
            EntryContent::Dir { children } => Some(children),
            _ => None,
        }
    }

    /// What is declared inside this entry, for replacing one of them in place.
    pub(crate) const fn children_mut(&mut self) -> Option<&mut BTreeMap<String, Self>> {
        match &mut self.content {
            EntryContent::Dir { children } => Some(children),
            _ => None,
        }
    }

    /// Renders the entry, discriminator included.
    ///
    /// # Errors
    ///
    /// Returns [`EntryRenderError::NonUtf8Content`] for a file whose bytes are not text. Inline
    /// content is written as a JSON string, which is the reference's rendering and the reason it
    /// refuses the same manifests: a manifest stays readable and editable by hand, and encoding
    /// arbitrary bytes instead would be a second representation only one of the two could read back.
    /// An entry that has to carry binary content copies it from the host instead.
    pub fn to_json(&self) -> Result<Value, EntryRenderError> {
        let mut fields = match &self.content {
            EntryContent::Dir { children } => {
                let mut rendered = JsonMap::new();
                for (name, child) in children {
                    rendered.insert(name.clone(), child.to_json()?);
                }
                JsonMap::from_iter([("children".to_owned(), Value::Object(rendered))])
            }
            EntryContent::File { content } => {
                let text =
                    std::str::from_utf8(content).map_err(|_| EntryRenderError::NonUtf8Content)?;
                JsonMap::from_iter([("content".to_owned(), Value::from(text))])
            }
            EntryContent::LocalFile { src } => {
                JsonMap::from_iter([("src".to_owned(), Value::from(src.clone()))])
            }
            EntryContent::LocalDir { src } => JsonMap::from_iter([(
                "src".to_owned(),
                src.clone().map_or(Value::Null, Value::from),
            )]),
            EntryContent::GitRepo {
                host,
                repo,
                reference,
                subpath,
            } => JsonMap::from_iter([
                ("host".to_owned(), Value::from(host.clone())),
                ("repo".to_owned(), Value::from(repo.clone())),
                ("ref".to_owned(), Value::from(reference.clone())),
                (
                    "subpath".to_owned(),
                    subpath.clone().map_or(Value::Null, Value::from),
                ),
            ]),
            EntryContent::Mount(mount) => mount.to_fields(),
            EntryContent::Extension(payload) => payload
                .fields()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        };

        for key in COMMON_FIELDS {
            fields.remove(key);
        }
        fields.insert(
            "description".to_owned(),
            self.description.clone().map_or(Value::Null, Value::from),
        );
        fields.insert("ephemeral".to_owned(), Value::from(self.ephemeral));
        // Neither of these can fail to render: both are plain records of strings, numbers and
        // booleans. `Null` is the only answer left if that ever stops being true, and it is the one
        // a reader treats as "not set" rather than silently mistaking for something else.
        fields.insert(
            "group".to_owned(),
            self.group.as_ref().map_or(Value::Null, |group| {
                serde_json::to_value(group).unwrap_or(Value::Null)
            }),
        );
        fields.insert("is_dir".to_owned(), Value::from(self.is_dir));
        fields.insert(
            "permissions".to_owned(),
            serde_json::to_value(self.permissions).unwrap_or(Value::Null),
        );
        fields.insert("type".to_owned(), Value::from(self.entry_type()));
        Ok(Value::Object(fields))
    }

    /// Reads an entry, routing it to the type the registry says owns it.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::UnknownType`] for a type nobody registered — refused rather than
    /// carried, because an entry whose meaning is unknown is one materialization would have to guess
    /// at — and [`RegistryError::InvalidPayload`] when the owning type rejects the fields.
    pub fn parse(registries: &ManifestRegistries, value: &Value) -> Result<Self, RegistryError> {
        let payload = registries.entries().parse(value)?;
        Self::from_payload(registries, &payload)
    }

    /// Builds an entry from a payload the registry has already admitted.
    fn from_payload(
        registries: &ManifestRegistries,
        payload: &DiscriminatedPayload,
    ) -> Result<Self, RegistryError> {
        let invalid = |reason: String| RegistryError::InvalidPayload {
            noun: entry_kind().noun(),
            type_name: payload.type_name().to_owned(),
            reason,
        };

        let content = match payload.type_name() {
            DIR_ENTRY_TYPE => {
                let mut children = BTreeMap::new();
                if let Some(declared) = payload.field("children") {
                    let Value::Object(declared) = declared else {
                        return Err(invalid("`children` must be a mapping".to_owned()));
                    };
                    for (name, child) in declared {
                        children.insert(name.clone(), Self::parse(registries, child)?);
                    }
                }
                EntryContent::Dir { children }
            }
            FILE_ENTRY_TYPE => EntryContent::File {
                content: required_text(payload, "content")
                    .map_err(invalid)?
                    .into_bytes(),
            },
            LOCAL_FILE_ENTRY_TYPE => EntryContent::LocalFile {
                src: required_text(payload, "src").map_err(invalid)?,
            },
            LOCAL_DIR_ENTRY_TYPE => EntryContent::LocalDir {
                src: optional_text(payload, "src").map_err(invalid)?,
            },
            GIT_REPO_ENTRY_TYPE => EntryContent::GitRepo {
                host: optional_text(payload, "host")
                    .map_err(invalid)?
                    .unwrap_or_else(|| DEFAULT_GIT_HOST.to_owned()),
                repo: required_text(payload, "repo").map_err(invalid)?,
                reference: required_text(payload, "ref").map_err(invalid)?,
                subpath: optional_text(payload, "subpath").map_err(invalid)?,
            },
            mount_type if BUILTIN_MOUNT_TYPES.contains(&mount_type) => {
                EntryContent::Mount(Box::new(
                    Mount::from_fields(mount_type, payload.fields(), registries.mount_strategies())
                        .map_err(invalid)?,
                ))
            }
            _ => {
                let mut extension = DiscriminatedPayload::new(payload.type_name());
                for (key, value) in payload.fields() {
                    if !COMMON_FIELDS.contains(&key.as_str()) {
                        extension = extension.with_field(key.clone(), value.clone());
                    }
                }
                EntryContent::Extension(extension)
            }
        };

        let mut entry = Self::new(content);
        entry.description = optional_text(payload, "description").map_err(invalid)?;
        entry.ephemeral = match payload.field("ephemeral") {
            // A mount stays ephemeral whatever the payload said. A state written by something that
            // did not enforce this must not be able to talk a snapshot into capturing a mount.
            _ if entry.content.is_mount() => true,
            Some(Value::Bool(ephemeral)) => *ephemeral,
            None | Some(Value::Null) => false,
            Some(_) => return Err(invalid("`ephemeral` must be a boolean".to_owned())),
        };
        entry.group = match payload.field("group") {
            None | Some(Value::Null) => None,
            Some(group) => Some(
                serde_json::from_value(group.clone())
                    .map_err(|_| invalid("`group` must be a user or a group".to_owned()))?,
            ),
        };
        if let Some(permissions) = payload.field("permissions")
            && !permissions.is_null()
        {
            let permissions = serde_json::from_value(permissions.clone())
                .map_err(|_| invalid("`permissions` must be a permission set".to_owned()))?;
            entry = entry.with_permissions(permissions);
        }
        entry.is_dir = match payload.field("is_dir") {
            Some(Value::Bool(is_dir)) => *is_dir,
            None | Some(Value::Null) => entry.is_dir,
            Some(_) => return Err(invalid("`is_dir` must be a boolean".to_owned())),
        };
        Ok(entry)
    }
}

impl Serialize for Entry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

/// An entry that cannot be written out as it stands.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EntryRenderError {
    /// A file's inline bytes are not text.
    #[error("inline file content must be valid UTF-8 to be written out")]
    NonUtf8Content,
}

/// Reads a field that must be present and a string.
fn required_text(payload: &DiscriminatedPayload, key: &str) -> Result<String, String> {
    match payload.field(key) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(format!("`{key}` must be a string")),
        None => Err(format!("`{key}` is required")),
    }
}

/// Reads a field that may be absent, but must be a string when it is not.
fn optional_text(payload: &DiscriminatedPayload, key: &str) -> Result<Option<String>, String> {
    match payload.field(key) {
        Some(Value::String(text)) => Ok(Some(text.clone())),
        None | Some(Value::Null) => Ok(None),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

/// Checks that a directory's children are themselves entry-shaped.
fn validate_dir(payload: &DiscriminatedPayload) -> Result<(), String> {
    match payload.field("children") {
        None | Some(Value::Null | Value::Object(_)) => Ok(()),
        Some(_) => Err("`children` must be a mapping".to_owned()),
    }
}

/// Checks that a file carries its content.
fn validate_file(payload: &DiscriminatedPayload) -> Result<(), String> {
    required_text(payload, "content").map(|_| ())
}

/// Checks that a copied file names its source.
fn validate_local_file(payload: &DiscriminatedPayload) -> Result<(), String> {
    required_text(payload, "src").map(|_| ())
}

/// Checks that a copied directory's source, if named, is a path.
fn validate_local_dir(payload: &DiscriminatedPayload) -> Result<(), String> {
    optional_text(payload, "src").map(|_| ())
}

/// Checks that a git checkout names a repository and a ref.
fn validate_git_repo(payload: &DiscriminatedPayload) -> Result<(), String> {
    required_text(payload, "repo")?;
    required_text(payload, "ref")?;
    optional_text(payload, "host")?;
    optional_text(payload, "subpath").map(|_| ())
}

/// Resolves where an entry lands, given the workspace root it is declared under.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidManifestPath`] for a path that is absolute — in either flavour — or
/// that contains a parent segment. A `..` is refused rather than normalized: the reference checks the
/// segments as written, so `pkg/../pkg/file` is refused even though it resolves back inside the
/// workspace. That is the stricter reading, and it is the one a manifest is held to, because an
/// entry path is written by hand rather than typed by a model mid-run.
pub fn resolve_workspace_path(workspace_root: &str, rel: &str) -> Result<PosixPath, SandboxError> {
    if let Some(windows_path) = windows_absolute_path(rel) {
        return Err(invalid_entry_path(&windows_path, "absolute"));
    }
    let rel_path = PosixPath::coerce(rel);
    if rel_path.is_absolute() {
        return Err(invalid_entry_path(rel_path.as_str(), "absolute"));
    }
    if rel_path.parts().contains(&"..") {
        return Err(invalid_entry_path(rel_path.as_str(), "escape_root"));
    }

    let root = PosixPath::coerce(workspace_root);
    Ok(if rel_path.parts().is_empty() {
        root
    } else {
        root.join(rel_path.as_str())
    })
}

/// Refuses an entry path that does not name somewhere inside the workspace.
pub(crate) fn invalid_entry_path(rel: &str, reason: &'static str) -> SandboxError {
    let message = if reason == "absolute" {
        format!("manifest path must be relative: {rel}")
    } else {
        format!("manifest path must not escape root: {rel}")
    };
    SandboxError::new(ErrorCode::InvalidManifestPath, OpName::Materialize, message)
        .with_context("rel", rel)
        .with_context("reason", reason)
}
