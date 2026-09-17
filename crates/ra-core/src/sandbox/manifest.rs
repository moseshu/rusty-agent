//! What a workspace is supposed to contain when a session starts.
//!
//! A manifest is declarative: it names a root, the accounts that should exist, and the content to
//! materialize into the workspace. A session applies it on start and, on resume, reapplies only the
//! parts that were never persisted.
//!
//! # What is typed here, and what is not yet
//!
//! The accounts, the root and the remote-mount allowlist are modelled. The entries, the environment
//! and the extra path grants are carried as they were written, because giving them shape means
//! deciding how an artifact is declared and how a path grant is bound — the substance of the task
//! that ports materialization, not something to settle in passing here. They round-trip exactly, so
//! a manifest written by a host that models them survives one that does not, and the typed fields
//! can be promoted one at a time without changing the wire format.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{Group, User};

/// The only manifest version this understands.
pub const MANIFEST_VERSION: u32 = 1;

/// The workspace root a manifest materializes into unless it says otherwise.
pub const DEFAULT_MANIFEST_ROOT: &str = "/workspace";

/// The commands a remote mount may run when nothing narrows the list.
///
/// Reading and moving files, and nothing that fetches, executes or escalates. A backend that needs
/// more says so in its own manifest rather than widening this.
pub const DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST: [&str; 18] = [
    "ls", "find", "stat", "cat", "less", "head", "tail", "du", "grep", "rg", "wc", "sort", "cut",
    "cp", "tee", "echo", "mkdir", "rm",
];

/// What a workspace should contain when a session starts.
///
/// Every field has a default, so `{}` is a valid manifest and means an empty workspace at the
/// default root. That is the reference's shape and it matters for configuration written by hand:
/// making any of these required would reject manifests the reference accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    /// Manifest format version.
    ///
    /// Only [`MANIFEST_VERSION`] is understood, and anything else is refused rather than read as if
    /// it were this one. A later format that reorganized entries would otherwise be materialized by
    /// rules written for a format it is not.
    #[serde(deserialize_with = "deserialize_version")]
    pub version: u32,
    /// Absolute path the entries materialize under.
    pub root: String,
    /// Content to materialize, keyed by workspace-relative path.
    ///
    /// Held as written until materialization is ported.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entries: BTreeMap<String, Value>,
    /// Environment the workspace is materialized with.
    ///
    /// Held as written until materialization is ported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Value>,
    /// Accounts that should exist in the sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<User>,
    /// Groups that should exist in the sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<Group>,
    /// Paths outside the workspace this manifest grants access to.
    ///
    /// Held as written until path grants are ported. **These are authority, not content**: a host
    /// that rebinds a persisted state must take them from a manifest it trusts rather than from
    /// whatever the payload carried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_path_grants: Vec<Value>,
    /// Commands a remote mount may run.
    pub remote_mount_command_allowlist: Vec<String>,
}

/// Reads the version, refusing any format this does not understand.
fn deserialize_version<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let version = u32::deserialize(deserializer)?;
    if version == MANIFEST_VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom(format!(
            "unsupported manifest version {version}; only {MANIFEST_VERSION} is understood"
        )))
    }
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            root: DEFAULT_MANIFEST_ROOT.to_owned(),
            entries: BTreeMap::new(),
            environment: None,
            users: Vec::new(),
            groups: Vec::new(),
            extra_path_grants: Vec::new(),
            remote_mount_command_allowlist: DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST
                .iter()
                .map(|command| (*command).to_owned())
                .collect(),
        }
    }
}

impl Manifest {
    /// An empty manifest rooted at the default workspace path.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Materializes under `root` instead of the default.
    #[must_use]
    pub fn with_root(mut self, root: impl Into<String>) -> Self {
        self.root = root.into();
        self
    }

    /// Adds an account that should exist in the sandbox.
    #[must_use]
    pub fn with_user(mut self, user: User) -> Self {
        self.users.push(user);
        self
    }

    /// Adds a group that should exist in the sandbox.
    #[must_use]
    pub fn with_group(mut self, group: Group) -> Self {
        self.groups.push(group);
        self
    }

    /// Declares one entry to materialize, as the materializing backend will read it.
    #[must_use]
    pub fn with_entry(mut self, path: impl Into<String>, entry: impl Into<Value>) -> Self {
        self.entries.insert(path.into(), entry.into());
        self
    }

    /// Narrows the commands a remote mount may run.
    #[must_use]
    pub fn with_remote_mount_command_allowlist(
        mut self,
        commands: impl IntoIterator<Item = String>,
    ) -> Self {
        self.remote_mount_command_allowlist = commands.into_iter().collect();
        self
    }

    /// Whether this manifest grants access to any path outside its workspace.
    ///
    /// The question a host asks before trusting a persisted state: a manifest with no extra grants
    /// carries no authority to rebind.
    #[must_use]
    pub fn grants_extra_paths(&self) -> bool {
        !self.extra_path_grants.is_empty()
    }
}
