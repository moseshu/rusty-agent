// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Permission profiles used by executor filesystem contexts.
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use crate::legacy::SandboxPolicy;
use crate::permissions::{
    FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxKind,
    FileSystemSandboxPolicy, FileSystemSpecialPath, NetworkSandboxPolicy,
    RawFileSystemSandboxEntry,
};
use ra_paths::{AbsolutePathBuf, PathUri};
use serde::{Deserialize, Deserializer, Serialize, ser::Serializer};
use std::{io, num::NonZeroUsize, path::Path};
#[derive(Debug, Clone, Default, Eq, Hash, PartialEq)]
pub struct FileSystemPermissions {
    pub entries: Vec<FileSystemSandboxEntry>,
    pub glob_scan_max_depth: Option<NonZeroUsize>,
}

#[derive(Debug, Clone, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyReadWriteRoots {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<Vec<AbsolutePathBuf>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<Vec<AbsolutePathBuf>>,
}

impl FileSystemPermissions {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn from_read_write_roots(
        read: Option<Vec<AbsolutePathBuf>>,
        write: Option<Vec<AbsolutePathBuf>>,
    ) -> Self {
        Self::from_paths(read, write)
    }

    #[must_use]
    pub fn from_read_write_path_uris(
        read: Option<Vec<PathUri>>,
        write: Option<Vec<PathUri>>,
    ) -> Self {
        Self::from_paths(read, write)
    }

    fn from_paths<P>(read: Option<Vec<P>>, write: Option<Vec<P>>) -> Self
    where
        P: Into<FileSystemPath>,
    {
        let mut entries = Vec::new();
        if let Some(read) = read {
            entries.extend(
                read.into_iter().map(|path| {
                    FileSystemSandboxEntry::new(path.into(), FileSystemAccessMode::Read)
                }),
            );
        }
        if let Some(write) = write {
            entries.extend(
                write.into_iter().map(|path| {
                    FileSystemSandboxEntry::new(path.into(), FileSystemAccessMode::Write)
                }),
            );
        }
        Self {
            entries,
            glob_scan_max_depth: None,
        }
    }

    #[must_use]
    pub fn legacy_read_write_roots(&self) -> Option<LegacyReadWriteRoots> {
        self.as_legacy_permissions()
    }

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
            let path = path.to_abs_path().ok()?;
            match entry.access {
                FileSystemAccessMode::Read => read.push(path),
                FileSystemAccessMode::Write => write.push(path),
                FileSystemAccessMode::Deny => return None,
            }
        }

        Some(LegacyReadWriteRoots {
            read: (!read.is_empty()).then_some(read),
            write: (!write.is_empty()).then_some(write),
        })
    }
}

#[derive(Debug, Clone, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalFileSystemPermissions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    entries: Vec<RawFileSystemSandboxEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    glob_scan_max_depth: Option<NonZeroUsize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FileSystemPermissionsDe {
    Canonical(CanonicalFileSystemPermissions),
    Legacy(LegacyReadWriteRoots),
}

impl Serialize for FileSystemPermissions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(legacy) = self.as_legacy_permissions() {
            legacy.serialize(serializer)
        } else {
            CanonicalFileSystemPermissions {
                entries: self
                    .entries
                    .clone()
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()
                    .map_err(serde::ser::Error::custom)?,
                glob_scan_max_depth: self.glob_scan_max_depth,
            }
            .serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for FileSystemPermissions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match FileSystemPermissionsDe::deserialize(deserializer)? {
            FileSystemPermissionsDe::Canonical(CanonicalFileSystemPermissions {
                entries,
                glob_scan_max_depth,
            }) => Ok(Self {
                entries: entries
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()
                    .map_err(serde::de::Error::custom)?,
                glob_scan_max_depth,
            }),
            FileSystemPermissionsDe::Legacy(LegacyReadWriteRoots { read, write }) => {
                Ok(Self::from_read_write_roots(read, write))
            }
        }
    }
}

#[derive(Debug, Clone, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct NetworkPermissions {
    pub enabled: Option<bool>,
}

impl NetworkPermissions {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
    }
}

/// Partial permission overlay used for per-command requests and approved
/// session/turn grants.
#[derive(Debug, Clone, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct AdditionalPermissionProfile {
    pub network: Option<NetworkPermissions>,
    pub file_system: Option<FileSystemPermissions>,
}

impl AdditionalPermissionProfile {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.network.is_none() && self.file_system.is_none()
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEnforcement {
    /// Codex owns sandbox construction for this profile.
    #[default]
    Managed,
    /// No outer filesystem sandbox should be applied.
    Disabled,
    /// Filesystem isolation is enforced by an external caller.
    External,
}

impl SandboxEnforcement {
    #[must_use]
    pub fn from_legacy_sandbox_policy(sandbox_policy: &SandboxPolicy) -> Self {
        match sandbox_policy {
            SandboxPolicy::DangerFullAccess => Self::Disabled,
            SandboxPolicy::ExternalSandbox { .. } => Self::External,
            SandboxPolicy::ReadOnly { .. } | SandboxPolicy::WorkspaceWrite { .. } => Self::Managed,
        }
    }
}

/// Filesystem permissions for profiles where Codex owns sandbox construction.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ManagedFileSystemPermissions {
    /// Apply a managed filesystem sandbox from the listed entries.
    Restricted {
        entries: Vec<FileSystemSandboxEntry>,
        glob_scan_max_depth: Option<NonZeroUsize>,
    },
    /// Apply a managed sandbox that allows all filesystem access.
    Unrestricted,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SerializedManagedFileSystemPermissions {
    #[serde(rename_all = "snake_case")]
    Restricted {
        entries: Vec<RawFileSystemSandboxEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        glob_scan_max_depth: Option<NonZeroUsize>,
    },
    Unrestricted,
}

impl Serialize for ManagedFileSystemPermissions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Restricted {
                entries,
                glob_scan_max_depth,
            } => SerializedManagedFileSystemPermissions::Restricted {
                entries: entries
                    .clone()
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()
                    .map_err(serde::ser::Error::custom)?,
                glob_scan_max_depth: *glob_scan_max_depth,
            }
            .serialize(serializer),
            Self::Unrestricted => {
                SerializedManagedFileSystemPermissions::Unrestricted.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for ManagedFileSystemPermissions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(
            match SerializedManagedFileSystemPermissions::deserialize(deserializer)? {
                SerializedManagedFileSystemPermissions::Restricted {
                    entries,
                    glob_scan_max_depth,
                } => Self::Restricted {
                    entries: entries
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()
                        .map_err(serde::de::Error::custom)?,
                    glob_scan_max_depth,
                },
                SerializedManagedFileSystemPermissions::Unrestricted => Self::Unrestricted,
            },
        )
    }
}

impl ManagedFileSystemPermissions {
    fn from_sandbox_policy(file_system_sandbox_policy: &FileSystemSandboxPolicy) -> Self {
        match file_system_sandbox_policy.kind {
            FileSystemSandboxKind::Restricted => Self::Restricted {
                entries: file_system_sandbox_policy.entries.clone(),
                glob_scan_max_depth: file_system_sandbox_policy
                    .glob_scan_max_depth
                    .and_then(NonZeroUsize::new),
            },
            FileSystemSandboxKind::Unrestricted => Self::Unrestricted,
            FileSystemSandboxKind::ExternalSandbox => unreachable!(
                "external filesystem policies are represented by PermissionProfile::External"
            ),
        }
    }

    pub fn to_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        match self {
            Self::Restricted {
                entries,
                glob_scan_max_depth,
            } => FileSystemSandboxPolicy {
                kind: FileSystemSandboxKind::Restricted,
                glob_scan_max_depth: glob_scan_max_depth.map(usize::from),
                entries: entries.clone(),
            },
            Self::Unrestricted => FileSystemSandboxPolicy::unrestricted(),
        }
    }
}

/// Reserved identifier for the built-in read-only permission profile.
pub const BUILT_IN_PERMISSION_PROFILE_READ_ONLY: &str = ":read-only";

/// Reserved identifier for the built-in workspace-write permission profile.
pub const BUILT_IN_PERMISSION_PROFILE_WORKSPACE: &str = ":workspace";

/// Reserved identifier for the built-in full-access permission profile.
pub const BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS: &str = ":danger-full-access";

/// Canonical active runtime permissions for a conversation, turn, or command.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionProfile {
    /// Codex owns sandbox construction for this profile.
    #[serde(rename_all = "snake_case")]
    Managed {
        file_system: ManagedFileSystemPermissions,
        network: NetworkSandboxPolicy,
    },
    /// Do not apply an outer sandbox.
    Disabled,
    /// Filesystem isolation is enforced by an external caller.
    #[serde(rename_all = "snake_case")]
    External { network: NetworkSandboxPolicy },
}

/// Metadata for the named or implicit built-in permissions profile that
/// produced the active `PermissionProfile`.
///
/// The runtime must honor `PermissionProfile`; this sidecar exists so clients
/// can display stable profile identity without trying to reverse-engineer a
/// name from the compiled permissions.
#[derive(Debug, Clone, Eq, PartialEq, Deserialize, Serialize)]
pub struct ActivePermissionProfile {
    /// Profile identifier from `default_permissions` or the implicit built-in
    /// default, such as `:workspace` or a user-defined `[permissions.<id>]`
    /// profile.
    pub id: String,

    /// Optional parent profile identifier from the selected permissions
    /// profile's `extends` setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
}

impl ActivePermissionProfile {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            extends: None,
        }
    }

    #[must_use]
    pub fn read_only() -> Self {
        Self::new(BUILT_IN_PERMISSION_PROFILE_READ_ONLY)
    }
}

impl Default for PermissionProfile {
    fn default() -> Self {
        Self::Managed {
            file_system: ManagedFileSystemPermissions::Restricted {
                entries: Vec::new(),
                glob_scan_max_depth: None,
            },
            network: NetworkSandboxPolicy::Restricted,
        }
    }
}

impl PermissionProfile {
    /// Managed read-only filesystem access with restricted network access.
    #[must_use]
    pub fn read_only() -> Self {
        let file_system = FileSystemSandboxPolicy::read_only();
        Self::Managed {
            file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
            network: NetworkSandboxPolicy::Restricted,
        }
    }

    /// Intersects managed filesystem permissions with read-only access and restricts network.
    ///
    /// Returns `None` when filesystem enforcement belongs to an external caller.
    #[must_use]
    pub fn intersect_with_read_only(&self) -> Option<Self> {
        let mut file_system = self.file_system_sandbox_policy();
        match file_system.kind {
            FileSystemSandboxKind::Restricted => {
                for entry in &mut file_system.entries {
                    entry.access = match entry.access {
                        FileSystemAccessMode::Read | FileSystemAccessMode::Write => {
                            FileSystemAccessMode::Read
                        }
                        FileSystemAccessMode::Deny => FileSystemAccessMode::Deny,
                    };
                }
            }
            FileSystemSandboxKind::Unrestricted => {
                file_system = FileSystemSandboxPolicy::read_only();
            }
            FileSystemSandboxKind::ExternalSandbox => return None,
        }
        Some(Self::from_runtime_permissions(
            &file_system,
            NetworkSandboxPolicy::Restricted,
        ))
    }

    /// Managed workspace-write filesystem access with restricted network
    /// access.
    ///
    /// The returned profile contains symbolic `:workspace_roots` entries that
    /// must be resolved against the active permission root before enforcement.
    #[must_use]
    pub fn workspace_write() -> Self {
        Self::workspace_write_with(
            &[],
            NetworkSandboxPolicy::Restricted,
            /*exclude_tmpdir_env_var*/ false,
            /*exclude_slash_tmp*/ false,
        )
    }

    /// Managed workspace-write filesystem access with the legacy
    /// `sandbox_workspace_write` knobs applied directly to the profile.
    ///
    /// The returned profile contains symbolic `:workspace_roots` entries that
    /// must be resolved against the active permission root before enforcement.
    #[must_use]
    pub fn workspace_write_with(
        writable_roots: &[AbsolutePathBuf],
        network: NetworkSandboxPolicy,
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    ) -> Self {
        let file_system = FileSystemSandboxPolicy::workspace_write(
            writable_roots,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        );
        Self::Managed {
            file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
            network,
        }
    }

    /// Legacy workspace-write settings interpreted for executor-owned paths.
    #[must_use]
    pub fn workspace_write_with_path_uris(
        writable_roots: &[PathUri],
        network: NetworkSandboxPolicy,
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    ) -> Self {
        let file_system = FileSystemSandboxPolicy::workspace_write_with_path_uris(
            writable_roots,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        );
        Self::Managed {
            file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
            network,
        }
    }

    #[must_use]
    pub fn materialize_project_roots_with_workspace_roots(
        self,
        workspace_roots: &[AbsolutePathBuf],
    ) -> Self {
        self.materialize_project_roots_with(|policy| {
            policy.materialize_project_roots_with_workspace_roots(workspace_roots)
        })
    }

    #[must_use]
    pub fn materialize_project_roots_with_path_uris(self, workspace_roots: &[PathUri]) -> Self {
        self.materialize_project_roots_with(|policy| {
            policy.materialize_project_roots_with_path_uris(workspace_roots)
        })
    }

    fn materialize_project_roots_with(
        self,
        materialize: impl FnOnce(FileSystemSandboxPolicy) -> FileSystemSandboxPolicy,
    ) -> Self {
        match self {
            Self::Managed {
                file_system,
                network,
            } => {
                let file_system = materialize(file_system.to_sandbox_policy());
                Self::Managed {
                    file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
                    network,
                }
            }
            Self::Disabled => Self::Disabled,
            Self::External { network } => Self::External { network },
        }
    }

    #[must_use]
    pub fn from_runtime_permissions(
        file_system_sandbox_policy: &FileSystemSandboxPolicy,
        network_sandbox_policy: NetworkSandboxPolicy,
    ) -> Self {
        let enforcement = match file_system_sandbox_policy.kind {
            FileSystemSandboxKind::Restricted | FileSystemSandboxKind::Unrestricted => {
                SandboxEnforcement::Managed
            }
            FileSystemSandboxKind::ExternalSandbox => SandboxEnforcement::External,
        };
        Self::from_runtime_permissions_with_enforcement(
            enforcement,
            file_system_sandbox_policy,
            network_sandbox_policy,
        )
    }

    #[must_use]
    pub fn from_runtime_permissions_with_enforcement(
        enforcement: SandboxEnforcement,
        file_system_sandbox_policy: &FileSystemSandboxPolicy,
        network_sandbox_policy: NetworkSandboxPolicy,
    ) -> Self {
        match file_system_sandbox_policy.kind {
            FileSystemSandboxKind::ExternalSandbox => Self::External {
                network: network_sandbox_policy,
            },
            FileSystemSandboxKind::Unrestricted if enforcement == SandboxEnforcement::Disabled => {
                Self::Disabled
            }
            FileSystemSandboxKind::Restricted | FileSystemSandboxKind::Unrestricted => {
                Self::Managed {
                    file_system: ManagedFileSystemPermissions::from_sandbox_policy(
                        file_system_sandbox_policy,
                    ),
                    network: network_sandbox_policy,
                }
            }
        }
    }

    #[must_use]
    pub fn from_legacy_sandbox_policy(sandbox_policy: &SandboxPolicy) -> Self {
        Self::from_runtime_permissions_with_enforcement(
            SandboxEnforcement::from_legacy_sandbox_policy(sandbox_policy),
            &FileSystemSandboxPolicy::from(sandbox_policy),
            NetworkSandboxPolicy::from(sandbox_policy),
        )
    }

    #[must_use]
    pub fn from_legacy_sandbox_policy_for_cwd(sandbox_policy: &SandboxPolicy, cwd: &Path) -> Self {
        Self::from_runtime_permissions_with_enforcement(
            SandboxEnforcement::from_legacy_sandbox_policy(sandbox_policy),
            &FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(sandbox_policy, cwd),
            NetworkSandboxPolicy::from(sandbox_policy),
        )
    }

    #[must_use]
    pub fn enforcement(&self) -> SandboxEnforcement {
        match self {
            Self::Managed { .. } => SandboxEnforcement::Managed,
            Self::Disabled => SandboxEnforcement::Disabled,
            Self::External { .. } => SandboxEnforcement::External,
        }
    }

    #[must_use]
    pub fn file_system_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        match self {
            Self::Managed { file_system, .. } => file_system.to_sandbox_policy(),
            Self::Disabled => FileSystemSandboxPolicy::unrestricted(),
            Self::External { .. } => FileSystemSandboxPolicy::external_sandbox(),
        }
    }

    #[must_use]
    pub fn network_sandbox_policy(&self) -> NetworkSandboxPolicy {
        match self {
            Self::Managed { network, .. } | Self::External { network } => *network,
            Self::Disabled => NetworkSandboxPolicy::Enabled,
        }
    }

    pub fn to_legacy_sandbox_policy(&self, cwd: &Path) -> io::Result<SandboxPolicy> {
        match self {
            Self::Managed {
                file_system,
                network,
            } => file_system
                .to_sandbox_policy()
                .to_legacy_sandbox_policy(*network, cwd),
            Self::Disabled => Ok(SandboxPolicy::DangerFullAccess),
            Self::External { network } => Ok(SandboxPolicy::ExternalSandbox {
                network_access: if network.is_enabled() {
                    crate::legacy::NetworkAccess::Enabled
                } else {
                    crate::legacy::NetworkAccess::Restricted
                },
            }),
        }
    }

    #[must_use]
    pub fn to_runtime_permissions(&self) -> (FileSystemSandboxPolicy, NetworkSandboxPolicy) {
        (
            self.file_system_sandbox_policy(),
            self.network_sandbox_policy(),
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TaggedPermissionProfile {
    #[serde(rename_all = "snake_case")]
    Managed {
        file_system: ManagedFileSystemPermissions,
        network: NetworkSandboxPolicy,
    },
    Disabled,
    #[serde(rename_all = "snake_case")]
    External {
        network: NetworkSandboxPolicy,
    },
}

impl From<TaggedPermissionProfile> for PermissionProfile {
    fn from(value: TaggedPermissionProfile) -> Self {
        match value {
            TaggedPermissionProfile::Managed {
                file_system,
                network,
            } => Self::Managed {
                file_system,
                network,
            },
            TaggedPermissionProfile::Disabled => Self::Disabled,
            TaggedPermissionProfile::External { network } => Self::External { network },
        }
    }
}

/// Pre-tagged shape written to rollout files before `PermissionProfile`
/// represented enforcement explicitly.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPermissionProfile {
    network: Option<NetworkPermissions>,
    file_system: Option<FileSystemPermissions>,
}

impl From<LegacyPermissionProfile> for PermissionProfile {
    fn from(value: LegacyPermissionProfile) -> Self {
        let file_system = value.file_system.map_or_else(
            || ManagedFileSystemPermissions::Restricted {
                entries: Vec::new(),
                glob_scan_max_depth: None,
            },
            |permissions| ManagedFileSystemPermissions::Restricted {
                entries: permissions.entries,
                glob_scan_max_depth: permissions.glob_scan_max_depth,
            },
        );
        let network_sandbox_policy = if value
            .network
            .as_ref()
            .and_then(|network| network.enabled)
            .unwrap_or(false)
        {
            NetworkSandboxPolicy::Enabled
        } else {
            NetworkSandboxPolicy::Restricted
        };
        Self::Managed {
            file_system,
            network: network_sandbox_policy,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum PermissionProfileDe {
    Tagged(TaggedPermissionProfile),
    Legacy(LegacyPermissionProfile),
}

impl<'de> Deserialize<'de> for PermissionProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match PermissionProfileDe::deserialize(deserializer)? {
            PermissionProfileDe::Tagged(tagged) => tagged.into(),
            PermissionProfileDe::Legacy(legacy) => legacy.into(),
        })
    }
}

impl From<NetworkSandboxPolicy> for NetworkPermissions {
    fn from(value: NetworkSandboxPolicy) -> Self {
        Self {
            enabled: Some(value.is_enabled()),
        }
    }
}

impl From<&FileSystemSandboxPolicy> for FileSystemPermissions {
    fn from(value: &FileSystemSandboxPolicy) -> Self {
        let entries = match value.kind {
            FileSystemSandboxKind::Restricted => value.entries.clone(),
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => {
                vec![FileSystemSandboxEntry::new(
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    },
                    FileSystemAccessMode::Write,
                )]
            }
        };
        Self {
            entries,
            glob_scan_max_depth: value.glob_scan_max_depth.and_then(NonZeroUsize::new),
        }
    }
}

impl From<&FileSystemPermissions> for FileSystemSandboxPolicy {
    fn from(value: &FileSystemPermissions) -> Self {
        let mut policy = FileSystemSandboxPolicy::restricted(value.entries.clone());
        policy.glob_scan_max_depth = value.glob_scan_max_depth.map(usize::from);
        policy
    }
}
