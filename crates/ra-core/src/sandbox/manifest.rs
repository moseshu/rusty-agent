//! What a workspace is supposed to contain when a session starts.
//!
//! A manifest is declarative: it names a root, the accounts that should exist, the content to
//! materialize into the workspace, and the paths outside it the session may reach. A session applies
//! it on start and, on resume, reapplies only the parts that were never persisted.
//!
//! # What is typed here, and what is not yet
//!
//! Every field is modelled: the root, the accounts, the entries, the environment, the path grants
//! and the remote-mount allowlist.
//!
//! # Reading one back needs a registry
//!
//! Entry types are an open family, so a manifest cannot be read through plain deserialization: the
//! set of types a payload may name is the host's assembly, not a property of this crate. Read the
//! JSON first and hand it to [`Manifest::parse`] with the registry the host built.
//!
//! # A rendered manifest leaves out what it does not set
//!
//! Entries, the environment, accounts, groups and grants are omitted when empty, where the reference
//! writes each of them out as an empty collection. Both sides default every field, so each reads the
//! other's manifests; the difference is only in how much of an empty manifest gets written down.
//!
//! One rule from the reference is **not** here: a manifest supplied as a *mapping* — rather than as
//! a `Manifest` the caller constructed — may not declare path grants at all, because a grant is
//! authority and a mapping is configuration. That check belongs where a run configuration coerces
//! its manifest argument, and lands with the task that ports the run configuration.
//! [`Manifest::parse`] matches the reference's model-level read, which does accept them.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use super::entries::mounts::Mount;
use super::entries::mounts::builtin_mount_strategy_registry;
use super::entries::{Entry, EntryContent, builtin_entry_registry, invalid_entry_path};
use super::environment::{Environment, builtin_env_value_registry};
use super::error::SandboxError;
use super::manifest_render::{MAX_MANIFEST_DESCRIPTION_CHARS, render_manifest_description};
use super::mount_security::{Provenance, manifest_mount_provenance};
use super::registry::{RegistryError, TypeRegistry};
use super::types::{Group, User};
use super::workspace_paths::{
    PathGrantError, PosixPath, SandboxPathGrant, SessionPath, windows_absolute_path,
};

/// The registries a manifest is read through.
///
/// Three open families meet in one manifest — entry types, mount strategies and environment value
/// types — and each is the host's to assemble. They travel together because reading a manifest needs
/// all three at once: an entry may be a mount, whose strategy is its own family, and the environment
/// beside it is a third.
pub struct ManifestRegistries {
    entries: TypeRegistry,
    mount_strategies: TypeRegistry,
    env_values: TypeRegistry,
}

impl ManifestRegistries {
    /// The families this crate models, and nothing else.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            entries: builtin_entry_registry(),
            mount_strategies: builtin_mount_strategy_registry(),
            env_values: builtin_env_value_registry(),
        }
    }

    /// Assembles the three families a host wants.
    #[must_use]
    pub const fn new(
        entries: TypeRegistry,
        mount_strategies: TypeRegistry,
        env_values: TypeRegistry,
    ) -> Self {
        Self {
            entries,
            mount_strategies,
            env_values,
        }
    }

    /// The entry types a manifest may declare.
    #[must_use]
    pub const fn entries(&self) -> &TypeRegistry {
        &self.entries
    }

    /// The mount strategies a mount may be attached with.
    #[must_use]
    pub const fn mount_strategies(&self) -> &TypeRegistry {
        &self.mount_strategies
    }

    /// The environment value types a manifest may reference.
    #[must_use]
    pub const fn env_values(&self) -> &TypeRegistry {
        &self.env_values
    }

    /// Adds an entry type this host knows.
    pub const fn entries_mut(&mut self) -> &mut TypeRegistry {
        &mut self.entries
    }

    /// Adds a mount strategy this host knows.
    pub const fn mount_strategies_mut(&mut self) -> &mut TypeRegistry {
        &mut self.mount_strategies
    }

    /// Adds an environment value type this host knows.
    pub const fn env_values_mut(&mut self) -> &mut TypeRegistry {
        &mut self.env_values
    }
}

impl Default for ManifestRegistries {
    fn default() -> Self {
        Self::builtin()
    }
}

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Manifest {
    /// Manifest format version.
    ///
    /// Only [`MANIFEST_VERSION`] is understood, and anything else is refused rather than read as if
    /// it were this one. A later format that reorganized entries would otherwise be materialized by
    /// rules written for a format it is not.
    pub version: u32,
    /// Absolute path the entries materialize under.
    pub root: String,
    /// Content to materialize, keyed by workspace-relative path.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub entries: BTreeMap<String, Entry>,
    /// Environment the workspace is materialized with.
    #[serde(skip_serializing_if = "Environment::is_empty")]
    pub environment: Environment,
    /// Accounts that should exist in the sandbox.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<User>,
    /// Groups that should exist in the sandbox.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<Group>,
    /// Paths outside the workspace this manifest grants access to.
    ///
    /// **These are authority, not content**: a host that rebinds a persisted state must take them
    /// from a manifest it trusts rather than from whatever the payload carried.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_path_grants: Vec<SandboxPathGrant>,
    /// Commands a remote mount may run.
    pub remote_mount_command_allowlist: Vec<String>,
    /// Which mounts a host has accepted the credential exposure of.
    ///
    /// Private, and never serialized: an acknowledgement is a decision the application made about
    /// this process, not a property of the workspace. Writing it out would let it travel to a host
    /// that never agreed to it, and reading it back in would let a payload grant itself one.
    #[serde(skip)]
    credential_exposure: CredentialExposurePolicy,
}

/// Which mount paths a host has accepted the credential exposure of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CredentialExposurePolicy {
    mount_scoped: BTreeSet<String>,
    broad: BTreeSet<String>,
}

/// How far the authority a mount exposes inside the container reaches.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountCredentialAuthority {
    /// Credentials that reach only what this mount is for.
    ///
    /// A bucket key: whoever is inside the container can read the bucket, and nothing else.
    MountScoped,
    /// Credentials that reach past this mount.
    ///
    /// A managed or workload identity, or an external credential file. Whoever is inside the
    /// container can use it for whatever else it is good for, which is why it is acknowledged
    /// separately: agreeing to expose a bucket key is not agreeing to expose an identity.
    Broad,
}

/// Why a credential exposure acknowledgement was rejected.
///
/// These reach whoever wrote the acknowledgement, which is application code making a security
/// decision — so they say which rule was broken rather than that something was wrong.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MountExposureError {
    /// No mount path was named.
    #[error("At least one in-container mount path is required.")]
    NoPaths,
    /// A path named the workspace root, or nothing at all.
    #[error("Mount credential exposure path must identify a non-root path.")]
    RootPath,
    /// A path was written with backslashes.
    #[error("Mount credential exposure paths must use '/' separators.")]
    Separators,
    /// A path used wildcard syntax.
    ///
    /// An acknowledgement names exact paths. A pattern would quietly widen as the manifest grew,
    /// which is the opposite of what an explicit security decision is for.
    #[error("Mount credential exposure paths must not contain wildcard syntax.")]
    Wildcard,
    /// A path climbed through a parent segment.
    #[error("Mount credential exposure paths must not contain parent segments.")]
    ParentSegments,
    /// The manifest holds a mount that is not one of the built-in kinds.
    ///
    /// Checked before anything is recorded: an acknowledgement is only meaningful for a mount whose
    /// configuration this crate knows how to read.
    #[error("custom mount implementations are not supported at the sandbox credential boundary")]
    CustomMount,
    /// The manifest holds a mount strategy that is not one of the built-in kinds.
    #[error("custom mount strategies are not supported at the sandbox credential boundary")]
    CustomStrategy,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            root: DEFAULT_MANIFEST_ROOT.to_owned(),
            entries: BTreeMap::new(),
            environment: Environment::new(),
            credential_exposure: CredentialExposurePolicy::default(),
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

    /// Declares one entry to materialize at a workspace-relative path.
    #[must_use]
    pub fn with_entry(mut self, path: impl Into<String>, entry: Entry) -> Self {
        self.entries.insert(path.into(), entry);
        self
    }

    /// Grants access to one path outside the workspace.
    #[must_use]
    pub fn with_path_grant(mut self, grant: SandboxPathGrant) -> Self {
        self.extra_path_grants.push(grant);
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

    /// Accepts that the named mounts expose mount-scoped credentials inside the container.
    ///
    /// The paths are the in-container mount paths, exactly — absolute, or relative to the workspace
    /// root. Whoever is inside the container can read whatever those credentials reach, and this is
    /// the application saying it knows that.
    ///
    /// Runtime-only. It is not serialized, so a manifest that travels somewhere else arrives
    /// without it and the receiving host has to make the decision for itself.
    ///
    /// # Errors
    ///
    /// Returns [`MountExposureError`] when no path is named, or a path is the root, written with
    /// backslashes, a wildcard, or contains a parent segment; and when the manifest holds a custom
    /// mount or mount strategy, whose exposure cannot be acknowledged at all.
    pub fn with_in_container_mount_credential_exposure_acknowledged(
        self,
        mount_paths: &[&str],
    ) -> Result<Self, MountExposureError> {
        self.acknowledging(MountCredentialAuthority::MountScoped, mount_paths)
    }

    /// Accepts that the named mounts expose *broad* credentials inside the container.
    ///
    /// Separate from the mount-scoped acknowledgement because it is a bigger decision: a managed or
    /// workload identity reaches past the mount it was configured for, so agreeing to one is not
    /// agreeing to the other.
    ///
    /// # Errors
    ///
    /// As [`Self::with_in_container_mount_credential_exposure_acknowledged`].
    pub fn with_in_container_mount_broad_credential_exposure_acknowledged(
        self,
        mount_paths: &[&str],
    ) -> Result<Self, MountExposureError> {
        self.acknowledging(MountCredentialAuthority::Broad, mount_paths)
    }

    /// Records an acknowledgement for one kind of authority.
    fn acknowledging(
        mut self,
        authority: MountCredentialAuthority,
        mount_paths: &[&str],
    ) -> Result<Self, MountExposureError> {
        if mount_paths.is_empty() {
            return Err(MountExposureError::NoPaths);
        }
        let mut acknowledged = BTreeSet::new();
        for mount_path in mount_paths {
            acknowledged.insert(
                self.exposure_key(mount_path, true)?
                    .ok_or(MountExposureError::RootPath)?,
            );
        }
        manifest_mount_provenance(&self).map_err(|provenance| match provenance {
            Provenance::CustomMount => MountExposureError::CustomMount,
            Provenance::CustomStrategy => MountExposureError::CustomStrategy,
        })?;
        let target = match authority {
            MountCredentialAuthority::MountScoped => &mut self.credential_exposure.mount_scoped,
            MountCredentialAuthority::Broad => &mut self.credential_exposure.broad,
        };
        target.extend(acknowledged);
        Ok(self)
    }

    /// Whether this manifest has accepted that a mount exposes credentials at a path.
    ///
    /// Accepts the path in either form: a mount acknowledged as `secrets` is the same mount as one
    /// acknowledged as `/workspace/secrets`, and which form the caller happens to hold should not
    /// decide whether the acknowledgement counts.
    #[must_use]
    pub fn acknowledges_in_container_mount_credential_exposure(
        &self,
        mount_path: &str,
        authority: MountCredentialAuthority,
    ) -> bool {
        let Ok(Some(key)) = self.exposure_key(mount_path, false) else {
            return false;
        };
        let acknowledged = match authority {
            MountCredentialAuthority::MountScoped => &self.credential_exposure.mount_scoped,
            MountCredentialAuthority::Broad => &self.credential_exposure.broad,
        };
        if acknowledged.contains(&key) {
            return true;
        }

        let root = self.normalized_root();
        let alternate = match key.split_once(':') {
            Some(("absolute", path)) => PosixPath::new(path)
                .relative_to(&root)
                .filter(|relative| !relative.parts().is_empty())
                .map(|relative| format!("relative:{relative}")),
            Some(("relative", path)) => Some(format!("absolute:{}", root.join(path))),
            _ => None,
        };
        alternate.is_some_and(|alternate| acknowledged.contains(&alternate))
    }

    /// Takes over another manifest's credential exposure acknowledgements, replacing this one's.
    ///
    /// How a manifest rebuilt from persisted state gets the decisions the application made about
    /// the trusted manifest it was rebound from: they were never persisted, so they can only come
    /// from there.
    pub(crate) fn copy_mount_credential_exposure_policy_from(&mut self, source: &Self) {
        self.credential_exposure = source.credential_exposure.clone();
    }

    /// Adds another manifest's credential exposure acknowledgements to this one's.
    ///
    /// How a manifest a capability rebuilt keeps the decisions the application made about the one
    /// it was handed: a capability that returns a fresh manifest cannot copy a private policy it
    /// never saw, and dropping the acknowledgement would refuse a mount the host had accepted.
    pub(crate) fn merge_mount_credential_exposure_policy_from(&mut self, source: &Self) {
        self.credential_exposure
            .mount_scoped
            .extend(source.credential_exposure.mount_scoped.iter().cloned());
        self.credential_exposure
            .broad
            .extend(source.credential_exposure.broad.iter().cloned());
    }

    /// The workspace root, forced absolute.
    fn normalized_root(&self) -> PosixPath {
        let root = PosixPath::coerce(&self.root).normalized();
        if root.is_absolute() {
            root
        } else {
            PosixPath::new(format!("/{root}"))
        }
    }

    /// Reduces a mount path to the form an acknowledgement is stored under.
    ///
    /// `Ok(None)` means the path named the root, which is only allowed when asking rather than
    /// acknowledging: a query about the root has no acknowledgement to find, while acknowledging
    /// the root would accept every mount under it at once.
    fn exposure_key(
        &self,
        mount_path: &str,
        reject_root: bool,
    ) -> Result<Option<String>, MountExposureError> {
        if mount_path.is_empty() {
            return if reject_root {
                Err(MountExposureError::RootPath)
            } else {
                Ok(None)
            };
        }
        if mount_path.contains('\\') {
            return Err(MountExposureError::Separators);
        }
        if reject_root {
            if mount_path.contains(['*', '?', '[', ']']) {
                return Err(MountExposureError::Wildcard);
            }
            if PosixPath::new(mount_path).parts().contains(&"..") {
                return Err(MountExposureError::ParentSegments);
            }
        }

        let path = PosixPath::new(mount_path);
        let root = self.normalized_root();
        if !path.is_absolute() {
            let normalized = path.normalized();
            if normalized.parts().is_empty() {
                return if reject_root {
                    Err(MountExposureError::RootPath)
                } else {
                    Ok(None)
                };
            }
            return Ok(Some(format!("relative:{normalized}")));
        }

        let normalized = path.normalized();
        if normalized.is_filesystem_root() || normalized == root {
            return if reject_root {
                Err(MountExposureError::RootPath)
            } else {
                Ok(None)
            };
        }
        Ok(Some(format!("absolute:{normalized}")))
    }

    /// Whether this manifest grants access to any path outside its workspace.
    ///
    /// The question a host asks before trusting a persisted state: a manifest with no extra grants
    /// carries no authority to rebind.
    #[must_use]
    pub fn grants_extra_paths(&self) -> bool {
        !self.extra_path_grants.is_empty()
    }

    /// Every entry in the manifest, with where it lands relative to the root.
    ///
    /// Pre-order: a directory comes before what is inside it, which is the order it has to be
    /// created in. The reference yields these lazily and validates each path as it goes; this walks
    /// the whole tree first, because every caller drains it and a half-validated manifest is not a
    /// useful thing to hand back.
    ///
    /// # Errors
    ///
    /// Returns a [`SandboxError`] for the first path that is absolute or climbs out of the
    /// workspace, nested paths included: a child declared as `../outside.txt` escapes just as surely
    /// as a top-level one, and it is the joined path that is checked.
    pub fn iter_entries(&self) -> Result<Vec<(PosixPath, &Entry)>, SandboxError> {
        let mut walked = Vec::new();
        walk_entries(None, &self.entries, &mut walked)?;
        Ok(walked)
    }

    /// The entries, having checked that every path in the manifest names somewhere inside it.
    ///
    /// # Errors
    ///
    /// As [`Self::iter_entries`].
    pub fn validated_entries(&self) -> Result<&BTreeMap<String, Entry>, SandboxError> {
        self.iter_entries()?;
        Ok(&self.entries)
    }

    /// The paths of entries that are not meant to survive a stop.
    ///
    /// Nested entries are included: an ephemeral file inside a persisted directory is ephemeral, and
    /// reading only the top level would carry it into a snapshot.
    ///
    /// # Errors
    ///
    /// As [`Self::iter_entries`].
    pub fn ephemeral_entry_paths(&self) -> Result<BTreeSet<PosixPath>, SandboxError> {
        Ok(self
            .iter_entries()?
            .into_iter()
            .filter(|(_, entry)| entry.is_ephemeral())
            .map(|(path, _)| path)
            .collect())
    }

    /// Every mount in the manifest, with where it actually attaches.
    ///
    /// Deepest first. A mount nested inside another has to be detached before the one it sits in,
    /// and attached after it, so the order is the one teardown needs rather than the one the entries
    /// were declared in.
    ///
    /// # Errors
    ///
    /// As [`Self::iter_entries`], and for a `mount_path` written in Windows drive syntax.
    pub fn mount_targets(&self) -> Result<Vec<(&Mount, PosixPath)>, SandboxError> {
        let root = PosixPath::coerce(&self.root);
        let mut targets = Vec::new();
        for (path, entry) in self.iter_entries()? {
            let EntryContent::Mount(mount) = entry.content() else {
                continue;
            };
            let resolved = mount.resolve_path_for_root(&root, &path)?;
            // A path that lands inside the workspace is re-measured from the root. One that names
            // somewhere else outright is kept as it was written — a mount is allowed to attach
            // outside the workspace, and whether a target is inside it is a question the
            // persistence exclusions ask separately.
            let target = normalize_in_workspace_path(&root, &resolved)?.unwrap_or(resolved);
            targets.push((mount.as_ref(), target));
        }
        targets.sort_by_key(|(_, path)| std::cmp::Reverse(path.parts().len()));
        Ok(targets)
    }

    /// The mounts, which are always ephemeral, with where they attach.
    ///
    /// # Errors
    ///
    /// As [`Self::mount_targets`].
    pub fn ephemeral_mount_targets(&self) -> Result<Vec<(&Mount, PosixPath)>, SandboxError> {
        self.mount_targets()
    }

    /// Every path a snapshot must leave out.
    ///
    /// The ephemeral entries, plus where each mount actually attaches — which is not always where it
    /// was declared. A mount declared at `logical` but attached at `actual` has to be excluded under
    /// *both* names: the declaration is what a reapply looks up, and the attach path is what a
    /// snapshot would otherwise walk into and record somebody else's storage from.
    ///
    /// # Errors
    ///
    /// As [`Self::mount_targets`].
    pub fn ephemeral_persistence_paths(&self) -> Result<BTreeSet<PosixPath>, SandboxError> {
        let root = PosixPath::coerce(&self.root);
        let mut skip = self.ephemeral_entry_paths()?;
        for (_, mount_path) in self.mount_targets()? {
            if let Some(relative) = mount_path.relative_to(&root)
                && !relative.parts().is_empty()
            {
                skip.insert(relative);
            }
        }
        Ok(skip)
    }

    /// Renders the manifest as a tree, for a model's instructions.
    ///
    /// `depth` bounds how far down it is shown; `None` shows all of it. The result is cut to
    /// [`MAX_MANIFEST_DESCRIPTION_CHARS`], and says so when it is.
    ///
    /// # Errors
    ///
    /// As [`Self::mount_targets`]: every path is validated before anything is drawn, so a manifest
    /// that could not be materialized is not described as though it could.
    pub fn describe(&self, depth: Option<usize>) -> Result<String, SandboxError> {
        self.describe_within(depth, Some(MAX_MANIFEST_DESCRIPTION_CHARS))
    }

    /// Renders the manifest as a tree, cut to a length this caller chooses.
    ///
    /// # Errors
    ///
    /// As [`Self::describe`].
    pub fn describe_within(
        &self,
        depth: Option<usize>,
        max_chars: Option<usize>,
    ) -> Result<String, SandboxError> {
        self.validated_entries()?;
        // A mount is drawn where it attaches, not where it was declared: that is the path the model
        // would have to use, and the two differ whenever `mount_path` is set.
        let root = PosixPath::coerce(&self.root);
        let mut mount_paths = BTreeMap::new();
        for (declared, entry) in &self.entries {
            if let EntryContent::Mount(mount) = entry.content() {
                let declared_path = PosixPath::coerce(declared);
                mount_paths.insert(
                    declared.clone(),
                    mount.resolve_path_for_root(&root, &declared_path)?,
                );
            }
        }
        render_manifest_description(&self.root, &self.entries, &mount_paths, depth, max_chars)
    }

    /// Reads a manifest, routing every entry through the types this host knows.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestParseError`] for a payload that is not an object, names a version this does
    /// not understand, carries a field of the wrong shape, declares an entry type nobody registered,
    /// or grants a path that is not absolute.
    pub fn parse(
        registries: &ManifestRegistries,
        value: &Value,
    ) -> Result<Self, ManifestParseError> {
        let Value::Object(fields) = value else {
            return Err(ManifestParseError::NotAnObject);
        };
        // An acknowledgement is a decision the application made, and a payload that could carry one
        // would be a payload that grants itself permission to expose credentials.
        if let Some(key) = fields
            .keys()
            .find(|key| EXPOSURE_POLICY_INPUT_KEYS.contains(&key.as_str()))
        {
            return Err(ManifestParseError::ExposurePolicyInInput { key: key.clone() });
        }
        let field = |key: &str| fields.get(key).filter(|value| !value.is_null());

        let mut manifest = Self::new();
        if let Some(version) = field("version") {
            let version = version
                .as_u64()
                .and_then(|version| u32::try_from(version).ok())
                .ok_or_else(|| ManifestParseError::UnsupportedVersion {
                    version: version.to_string(),
                })?;
            if version != MANIFEST_VERSION {
                return Err(ManifestParseError::UnsupportedVersion {
                    version: version.to_string(),
                });
            }
        }
        if let Some(root) = field("root") {
            root.as_str()
                .ok_or_else(|| ManifestParseError::invalid("root", "must be a string"))?
                .clone_into(&mut manifest.root);
        }
        if let Some(entries) = field("entries") {
            let entries = entries
                .as_object()
                .ok_or_else(|| ManifestParseError::invalid("entries", "must be a mapping"))?;
            for (path, entry) in entries {
                manifest.entries.insert(
                    path.clone(),
                    Entry::parse(registries, entry).map_err(|source| {
                        ManifestParseError::InvalidEntry {
                            path: path.clone(),
                            source,
                        }
                    })?,
                );
            }
        }
        if let Some(environment) = field("environment") {
            manifest.environment = Environment::parse(registries.env_values(), environment)
                .map_err(ManifestParseError::InvalidEnvironment)?;
        }
        if let Some(users) = field("users") {
            manifest.users = serde_json::from_value(users.clone())
                .map_err(|_| ManifestParseError::invalid("users", "must be a list of users"))?;
        }
        if let Some(groups) = field("groups") {
            manifest.groups = serde_json::from_value(groups.clone())
                .map_err(|_| ManifestParseError::invalid("groups", "must be a list of groups"))?;
        }
        if let Some(grants) = field("extra_path_grants") {
            let grants = grants.as_array().ok_or_else(|| {
                ManifestParseError::invalid("extra_path_grants", "must be a list of path grants")
            })?;
            manifest.extra_path_grants = grants
                .iter()
                .map(SandboxPathGrant::from_json)
                .collect::<Result<_, _>>()
                .map_err(ManifestParseError::InvalidGrant)?;
        }
        if let Some(allowlist) = field("remote_mount_command_allowlist") {
            manifest.remote_mount_command_allowlist = serde_json::from_value(allowlist.clone())
                .map_err(|_| {
                    ManifestParseError::invalid(
                        "remote_mount_command_allowlist",
                        "must be a list of commands",
                    )
                })?;
        }
        Ok(manifest)
    }
}

/// Spellings of the credential exposure policy that a payload may not carry.
///
/// Every casing and prefix the reference guards, because the point is to catch a payload *trying*
/// to set the policy rather than to define one canonical name for it: a check that only knew one
/// spelling would be a check somebody could go around by picking another.
const EXPOSURE_POLICY_INPUT_KEYS: [&str; 14] = [
    "in_container_mount_credential_exposure_allowed_paths",
    "_in_container_mount_credential_exposure_allowed_paths",
    "inContainerMountCredentialExposureAllowedPaths",
    "_inContainerMountCredentialExposureAllowedPaths",
    "in_container_mount_credential_exposure_acknowledged_paths",
    "_in_container_mount_credential_exposure_acknowledged_paths",
    "inContainerMountCredentialExposureAcknowledgedPaths",
    "_inContainerMountCredentialExposureAcknowledgedPaths",
    "in_container_mount_broad_credential_exposure_acknowledged_paths",
    "_in_container_mount_broad_credential_exposure_acknowledged_paths",
    "inContainerMountBroadCredentialExposureAcknowledgedPaths",
    "_inContainerMountBroadCredentialExposureAcknowledgedPaths",
    "mount_credential_exposure_policy",
    "_mount_credential_exposure_policy",
];

/// Walks one level of entries, and everything declared inside them.
fn walk_entries<'a>(
    prefix: Option<&PosixPath>,
    entries: &'a BTreeMap<String, Entry>,
    walked: &mut Vec<(PosixPath, &'a Entry)>,
) -> Result<(), SandboxError> {
    for (name, entry) in entries {
        let relative = coerce_entry_path(name)?;
        let path = prefix.map_or(relative.clone(), |prefix| prefix.join(relative.as_str()));
        validate_entry_path(&path)?;
        walked.push((path.clone(), entry));
        if let Some(children) = entry.children() {
            walk_entries(Some(&path), children, walked)?;
        }
    }
    Ok(())
}

/// Reads one declared path, refusing Windows drive syntax before it can be coerced.
fn coerce_entry_path(name: &str) -> Result<PosixPath, SandboxError> {
    if let Some(windows_path) = windows_absolute_path(name) {
        return Err(invalid_entry_path(&windows_path, "absolute"));
    }
    Ok(PosixPath::coerce(name))
}

/// Re-measures a path from the workspace root, or reports that it is not under it.
///
/// `Ok(None)` means the path names somewhere outside the workspace outright, which is allowed: a
/// mount may attach anywhere the backend can reach. What is refused is a path that *had to climb out
/// of the root* to get where it points — `/workspace/../../tmp` is not the same claim as `/tmp`,
/// even when they resolve to the same place, because only the first is a workspace-relative path
/// that escaped.
fn normalize_in_workspace_path(
    root: &PosixPath,
    path: &PosixPath,
) -> Result<Option<PosixPath>, SandboxError> {
    if let Some(windows_path) = windows_absolute_path(path.as_str()) {
        return Err(invalid_entry_path(&windows_path, "absolute"));
    }
    if !path.is_absolute() {
        let within = resolve_within_root(path, path)?;
        return Ok(Some(if within.parts().is_empty() {
            root.clone()
        } else {
            root.join(within.as_str())
        }));
    }

    let Some(relative) = path.relative_to(root) else {
        return Ok(None);
    };
    let within = resolve_within_root(&relative, path)?;
    Ok(Some(if within.parts().is_empty() {
        root.clone()
    } else {
        root.join(within.as_str())
    }))
}

/// Resolves a workspace-relative path, refusing one that climbs above the root.
///
/// `original` is what the refusal quotes, which is the path as the manifest wrote it rather than the
/// remainder this is working on.
fn resolve_within_root(
    relative: &PosixPath,
    original: &PosixPath,
) -> Result<PosixPath, SandboxError> {
    if relative.is_absolute() {
        return Err(invalid_entry_path(original.as_str(), "absolute"));
    }
    let mut components: Vec<&str> = Vec::new();
    for component in relative.parts() {
        if component == ".." {
            if components.pop().is_none() {
                return Err(invalid_entry_path(original.as_str(), "escape_root"));
            }
            continue;
        }
        components.push(component);
    }
    Ok(PosixPath::new(components.join("/")))
}

/// Reads a workspace-relative path, refusing one that is absolute or climbs out of the workspace.
///
/// The same rules a declared entry path is held to, for paths that arrive from elsewhere — a
/// session registering something it created at runtime.
pub(crate) fn validated_relative_path(path: SessionPath<'_>) -> Result<PosixPath, SandboxError> {
    if let Some(windows_path) = windows_absolute_path(path.as_str()) {
        return Err(invalid_entry_path(&windows_path, "absolute"));
    }
    let path = path.to_posix();
    validate_entry_path(&path)?;
    Ok(path)
}

/// Refuses a path that does not name somewhere inside the workspace.
fn validate_entry_path(path: &PosixPath) -> Result<(), SandboxError> {
    if let Some(windows_path) = windows_absolute_path(path.as_str()) {
        return Err(invalid_entry_path(&windows_path, "absolute"));
    }
    if path.is_absolute() {
        return Err(invalid_entry_path(path.as_str(), "absolute"));
    }
    if path.parts().contains(&"..") {
        return Err(invalid_entry_path(path.as_str(), "escape_root"));
    }
    Ok(())
}

/// Why a manifest could not be read.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestParseError {
    /// The payload was not an object, so it has no fields to read.
    #[error("manifest must be an object")]
    NotAnObject,
    /// The payload named a format this does not understand.
    #[error("unsupported manifest version {version}; only {MANIFEST_VERSION} is understood")]
    UnsupportedVersion {
        /// The version as the payload wrote it.
        version: String,
    },
    /// A field was present but not the shape a manifest expects.
    #[error("manifest field `{field}` {reason}")]
    InvalidField {
        /// The field that was rejected.
        field: &'static str,
        /// What was wrong with it.
        reason: &'static str,
    },
    /// An entry named a type nobody registered, or its owner refused the fields.
    #[error("manifest entry `{path}`: {source}")]
    InvalidEntry {
        /// The path the entry was declared at.
        path: String,
        /// The registry's refusal.
        #[source]
        source: RegistryError,
    },
    /// A path grant was not usable.
    #[error("manifest extra_path_grants: {0}")]
    InvalidGrant(#[source] PathGrantError),
    /// An environment member named a value type nobody registered, or was the wrong shape.
    #[error("manifest environment: {0}")]
    InvalidEnvironment(#[source] RegistryError),
    /// The payload tried to carry a credential exposure acknowledgement.
    #[error(
        "In-container mount credential exposure must be configured on a trusted Manifest \
         instance, not in manifest input (`{key}`)"
    )]
    ExposurePolicyInInput {
        /// The field that tried to carry it.
        key: String,
    },
}

impl ManifestParseError {
    /// Names a field that was present and the wrong shape.
    const fn invalid(field: &'static str, reason: &'static str) -> Self {
        Self::InvalidField { field, reason }
    }
}
