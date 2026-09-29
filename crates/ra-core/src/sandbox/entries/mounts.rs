//! External storage exposed inside the workspace.
//!
//! A mount is a manifest entry, so it lands at a path like any other. What makes it its own thing is
//! the split between *what* to mount and *how* to attach it:
//!
//! - a **provider** ([`MountProvider`]) says which bucket, container or file system, and carries the
//!   credentials for it;
//! - a **strategy** ([`MountStrategy`]) says who attaches it — a command run inside the sandbox, or
//!   the container runtime before the session starts;
//! - a **pattern** ([`MountPattern`]) says which tool the in-container strategy uses.
//!
//! Not every combination exists. [`MountProvider::supported_patterns`] and
//! [`MountProvider::supported_drivers`] are the support matrix, and a mount is refused at
//! construction when its strategy is not one the provider supports — before anything runs, rather
//! than when a mount command fails inside a container.
//!
//! # A mount is always ephemeral
//!
//! Mounts are runtime-attached external filesystems, not durable workspace state. A snapshot that
//! carried one would be recording somebody else's storage as if it were the workspace's, so
//! [`Entry::mount`](crate::sandbox::entries::Entry::mount) fixes `ephemeral` at true and the
//! manifest excludes mount targets from what it persists.
//!
//! # Permissions are not honoured, and that is the reference's decision
//!
//! Mount permission bits are unreliable — the provider decides what the credentials can reach, and
//! the mount tool's own view of ownership rarely survives. The reference warns and overwrites them;
//! this fixes them at the entry default without a warning, because there is no value a caller could
//! set that would be honoured. Access is configured at the provider.
//!
//! # Declaration only
//!
//! Nothing here mounts anything. The commands each pattern runs, the rclone configuration it
//! synthesizes, teardown across a snapshot, and the credential boundary checked at activation belong
//! to the backend that owns a session.

use std::collections::BTreeMap;

use serde_json::{Map as JsonMap, Value};

use super::super::error::SandboxError;
use super::super::registry::{DiscriminatedPayload, RegistryKind, TypeRegistry};
use super::super::workspace_paths::{PosixPath, windows_absolute_path};
use super::invalid_entry_path;

/// The discriminator of an Amazon S3 mount.
pub const S3_MOUNT_TYPE: &str = "s3_mount";
/// The discriminator of a Google Cloud Storage mount.
pub const GCS_MOUNT_TYPE: &str = "gcs_mount";
/// The discriminator of an Azure Blob Storage mount.
pub const AZURE_BLOB_MOUNT_TYPE: &str = "azure_blob_mount";
/// The discriminator of a Box mount.
pub const BOX_MOUNT_TYPE: &str = "box_mount";
/// The discriminator of a Cloudflare R2 mount.
pub const R2_MOUNT_TYPE: &str = "r2_mount";
/// The discriminator of an Amazon S3 Files mount.
pub const S3_FILES_MOUNT_TYPE: &str = "s3_files_mount";

/// Every mount type this crate models.
pub const BUILTIN_MOUNT_TYPES: [&str; 6] = [
    AZURE_BLOB_MOUNT_TYPE,
    BOX_MOUNT_TYPE,
    GCS_MOUNT_TYPE,
    R2_MOUNT_TYPE,
    S3_FILES_MOUNT_TYPE,
    S3_MOUNT_TYPE,
];

/// The discriminator of a mount attached by a command inside the sandbox.
pub const IN_CONTAINER_STRATEGY_TYPE: &str = "in_container";
/// The discriminator of a mount attached by the container runtime.
pub const DOCKER_VOLUME_STRATEGY_TYPE: &str = "docker_volume";

/// The family that routes mount strategies.
#[must_use]
pub fn mount_strategy_kind() -> RegistryKind {
    RegistryKind::new("mount strategy", "MountStrategyBase")
}

/// A registry holding the mount strategies this crate models.
///
/// A host running on a platform with its own attach mechanism adds it here. A strategy nobody
/// registered is refused rather than carried: an unknown strategy is one nothing will ever attach,
/// and a typo in the discriminator would otherwise become a mount that silently never appears.
#[must_use]
pub fn builtin_mount_strategy_registry() -> TypeRegistry {
    let mut registry = TypeRegistry::new(mount_strategy_kind());
    for strategy_type in [IN_CONTAINER_STRATEGY_TYPE, DOCKER_VOLUME_STRATEGY_TYPE] {
        let registered = registry.register(strategy_type, "ra_core::sandbox::entries::mounts");
        debug_assert!(registered.is_ok(), "built-in strategies must be distinct");
    }
    registry
}

/// Which tool an in-container mount uses.
///
/// Closed, unlike the strategy above it: the reference's pattern field is a discriminated union of
/// exactly these four, so a payload naming anything else is refused rather than carried. A mount
/// type that needs a fifth tool is a change to this enum, not something a host can add from outside.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountPattern {
    /// A provider-specific FUSE driver, such as `blobfuse2` for Azure.
    Fuse(FuseOptions),
    /// The AWS Mountpoint client for S3.
    Mountpoint(MountpointOptions),
    /// rclone, in either FUSE or NFS mode.
    Rclone(RcloneOptions),
    /// The Amazon S3 Files mount helper.
    S3Files(S3FilesOptions),
}

impl MountPattern {
    /// The pattern's wire string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Fuse(_) => "fuse",
            Self::Mountpoint(_) => "mountpoint",
            Self::Rclone(_) => "rclone",
            Self::S3Files(_) => "s3files",
        }
    }
}

/// How a FUSE-mounted provider caches and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuseOptions {
    /// Whether accounts other than the mounting one may read the mount.
    pub allow_other: bool,
    /// Where the driver writes its log.
    pub log_type: String,
    /// How much the driver logs.
    pub log_level: String,
    /// Whether the cache holds blocks or whole files.
    pub cache_type: FuseCacheType,
    /// Where the cache lives, or `None` for the driver's default.
    pub cache_path: Option<String>,
    /// How large the cache may grow, in MiB.
    pub cache_size_mb: Option<u64>,
    /// Block size for the block cache, in MiB.
    pub block_cache_block_size_mb: u64,
    /// How long a cached block survives on disk, in seconds.
    pub block_cache_disk_timeout_sec: u64,
    /// How long a cached file stays valid, in seconds.
    pub file_cache_timeout_sec: u64,
    /// How large the file cache may grow, in MiB.
    pub file_cache_max_size_mb: Option<u64>,
    /// How long an attribute lookup stays cached, in seconds.
    pub attr_cache_timeout_sec: Option<u64>,
    /// How long a directory entry stays cached, in seconds.
    pub entry_cache_timeout_sec: Option<u64>,
    /// How long a failed lookup stays cached, in seconds.
    pub negative_entry_cache_timeout_sec: Option<u64>,
}

impl Default for FuseOptions {
    fn default() -> Self {
        Self {
            allow_other: true,
            log_type: "syslog".to_owned(),
            log_level: "log_debug".to_owned(),
            cache_type: FuseCacheType::BlockCache,
            cache_path: None,
            cache_size_mb: None,
            block_cache_block_size_mb: 16,
            block_cache_disk_timeout_sec: 3600,
            file_cache_timeout_sec: 120,
            file_cache_max_size_mb: None,
            attr_cache_timeout_sec: None,
            entry_cache_timeout_sec: None,
            negative_entry_cache_timeout_sec: None,
        }
    }
}

impl FuseOptions {
    /// The cache directory, checked to be a workspace-relative path.
    ///
    /// The cache is scratch state the session writes through its own workspace-scoped operations,
    /// so a path that is absolute — in either flavour — or that climbs out is refused. `None` when
    /// the default is to be used.
    ///
    /// # Errors
    ///
    /// Returns [`crate::sandbox::ErrorCode::MountConfigInvalid`] carrying the path as `cache_path`.
    pub fn checked_cache_path(&self) -> Result<Option<PosixPath>, SandboxError> {
        let Some(cache_path) = &self.cache_path else {
            return Ok(None);
        };
        let refused = |rendered: &str| {
            SandboxError::mount_config("blobfuse cache_path must be relative to the workspace root")
                .with_context("cache_path", rendered)
        };
        if let Some(windows_path) = windows_absolute_path(cache_path) {
            return Err(refused(&windows_path));
        }
        let posix = PosixPath::coerce(cache_path);
        if posix.is_absolute() || posix.parts().contains(&"..") {
            return Err(refused(posix.as_str()));
        }
        Ok(Some(posix))
    }
}

/// Whether a FUSE mount caches blocks or whole files.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FuseCacheType {
    /// Cache fixed-size blocks.
    #[default]
    BlockCache,
    /// Cache whole files.
    FileCache,
}

impl FuseCacheType {
    /// The cache type's wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BlockCache => "block_cache",
            Self::FileCache => "file_cache",
        }
    }
}

/// Defaults a Mountpoint mount falls back to when the provider does not say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountpointOptions {
    /// Object prefix to mount instead of the whole bucket.
    pub prefix: Option<String>,
    /// Region the bucket is in.
    pub region: Option<String>,
    /// Endpoint to reach the bucket through.
    pub endpoint_url: Option<String>,
}

/// How rclone attaches the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcloneOptions {
    /// Whether rclone mounts through FUSE or serves NFS.
    pub mode: RcloneMode,
    /// The remote's name, or `None` to derive one per session.
    pub remote_name: Option<String>,
    /// Extra arguments passed to the mount command.
    pub extra_args: Vec<String>,
    /// Address to serve NFS on.
    pub nfs_addr: Option<String>,
    /// Options passed to the NFS mount.
    pub nfs_mount_options: Option<Vec<String>>,
    /// An existing rclone configuration file to read the remote from.
    pub config_file_path: Option<String>,
}

impl Default for RcloneOptions {
    fn default() -> Self {
        Self {
            mode: RcloneMode::Fuse,
            remote_name: None,
            extra_args: Vec::new(),
            nfs_addr: None,
            nfs_mount_options: None,
            config_file_path: None,
        }
    }
}

/// Whether rclone mounts through FUSE or serves NFS.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RcloneMode {
    /// Mount through FUSE.
    #[default]
    Fuse,
    /// Serve NFS and mount that.
    Nfs,
}

impl RcloneMode {
    /// The mode's wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }
}

/// Defaults an S3 Files mount falls back to when the provider does not say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct S3FilesOptions {
    /// Address of a mount target to reach the file system through.
    pub mount_target_ip: Option<String>,
    /// Access point to mount through.
    pub access_point: Option<String>,
    /// Region the file system is in.
    pub region: Option<String>,
    /// Further options handed to the mount helper.
    pub extra_options: BTreeMap<String, Option<String>>,
}

/// Who attaches the mount.
///
/// Open, as the reference's is: a host running on a platform with its own attach mechanism
/// registers a strategy of its own, and one this crate does not model round-trips as
/// [`MountStrategy::Extension`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountStrategy {
    /// A command run inside the sandbox attaches it.
    InContainer {
        /// Which tool the command uses.
        pattern: MountPattern,
    },
    /// The container runtime attaches it before the session starts.
    DockerVolume {
        /// The volume driver's name.
        driver: String,
        /// Options handed to the driver.
        ///
        /// **Opaque authority.** Third-party driver options cannot be classified by name, so the
        /// whole field is treated as live credentials: allowed only where an executor is trusted
        /// with them, and dropped as a unit from anything durable.
        driver_options: BTreeMap<String, String>,
    },
    /// A strategy a host registered and this crate does not model.
    Extension(DiscriminatedPayload),
}

impl MountStrategy {
    /// The strategy's wire string.
    #[must_use]
    pub fn type_name(&self) -> &str {
        match self {
            Self::InContainer { .. } => IN_CONTAINER_STRATEGY_TYPE,
            Self::DockerVolume { .. } => DOCKER_VOLUME_STRATEGY_TYPE,
            Self::Extension(payload) => payload.type_name(),
        }
    }

    /// Attaches through a command run inside the sandbox.
    #[must_use]
    pub const fn in_container(pattern: MountPattern) -> Self {
        Self::InContainer { pattern }
    }

    /// Attaches through the container runtime's volume driver.
    #[must_use]
    pub fn docker_volume(driver: impl Into<String>) -> Self {
        Self::DockerVolume {
            driver: driver.into(),
            driver_options: BTreeMap::new(),
        }
    }
}

/// What is being mounted, and the credentials to reach it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountProvider {
    /// An Amazon S3 bucket.
    S3(S3Mount),
    /// A Google Cloud Storage bucket.
    Gcs(GcsMount),
    /// An Azure Blob Storage container.
    AzureBlob(AzureBlobMount),
    /// A Box folder.
    Box(BoxMount),
    /// A Cloudflare R2 bucket.
    R2(R2Mount),
    /// An Amazon S3 Files file system.
    S3Files(S3FilesMount),
    /// A mount type a host registered and this crate does not model.
    Extension(DiscriminatedPayload),
}

impl MountProvider {
    /// The provider's wire string, which is also the entry's type.
    #[must_use]
    pub fn type_name(&self) -> &str {
        match self {
            Self::S3(_) => S3_MOUNT_TYPE,
            Self::Gcs(_) => GCS_MOUNT_TYPE,
            Self::AzureBlob(_) => AZURE_BLOB_MOUNT_TYPE,
            Self::Box(_) => BOX_MOUNT_TYPE,
            Self::R2(_) => R2_MOUNT_TYPE,
            Self::S3Files(_) => S3_FILES_MOUNT_TYPE,
            Self::Extension(payload) => payload.type_name(),
        }
    }

    /// The in-container patterns this provider can be attached with.
    ///
    /// Half of the support matrix. An empty set for an extension is not a claim that it supports
    /// nothing — it is that this crate cannot know, so it does not check.
    #[must_use]
    pub fn supported_patterns(&self) -> &'static [&'static str] {
        match self {
            Self::S3(_) | Self::Gcs(_) => &["rclone", "mountpoint"],
            Self::AzureBlob(_) => &["rclone", "fuse"],
            Self::Box(_) | Self::R2(_) => &["rclone"],
            Self::S3Files(_) => &["s3files"],
            Self::Extension(_) => &[],
        }
    }

    /// The container-runtime volume drivers this provider can be attached with.
    ///
    /// The other half of the support matrix. S3 Files has none: it is mounted by a helper inside the
    /// sandbox and has no volume driver.
    #[must_use]
    pub fn supported_drivers(&self) -> &'static [&'static str] {
        match self {
            Self::S3(_) | Self::Gcs(_) => &["mountpoint", "rclone"],
            Self::AzureBlob(_) | Self::Box(_) | Self::R2(_) => &["rclone"],
            Self::S3Files(_) | Self::Extension(_) => &[],
        }
    }

    /// Whether this crate knows enough about the provider to check its strategy.
    #[must_use]
    pub const fn is_modelled(&self) -> bool {
        !matches!(self, Self::Extension(_))
    }
}

/// The S3 implementation a bucket is served by, when the manifest does not say.
pub const DEFAULT_S3_PROVIDER: &str = "AWS";

/// An Amazon S3 bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Mount {
    /// The bucket to mount.
    pub bucket: String,
    /// Access key, or `None` to use the sandbox's ambient credentials.
    pub access_key_id: Option<String>,
    /// Secret key for `access_key_id`.
    pub secret_access_key: Option<String>,
    /// Session token, for temporary credentials.
    pub session_token: Option<String>,
    /// Object prefix to mount instead of the whole bucket.
    pub prefix: Option<String>,
    /// Region the bucket is in.
    pub region: Option<String>,
    /// Endpoint to reach the bucket through.
    pub endpoint_url: Option<String>,
    /// Which S3 implementation this is, for tools that vary by vendor.
    pub s3_provider: String,
}

impl Default for S3Mount {
    /// Written out rather than derived: `s3_provider` defaults to [`DEFAULT_S3_PROVIDER`], and a
    /// derived `Default` would make it empty. That would leave a mount built in code and the same
    /// mount read from JSON disagreeing about which vendor's S3 they are talking to.
    fn default() -> Self {
        Self {
            bucket: String::new(),
            access_key_id: None,
            secret_access_key: None,
            session_token: None,
            prefix: None,
            region: None,
            endpoint_url: None,
            s3_provider: DEFAULT_S3_PROVIDER.to_owned(),
        }
    }
}

/// A Google Cloud Storage bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcsMount {
    /// The bucket to mount.
    pub bucket: String,
    /// HMAC access id, which selects the S3-compatible path.
    pub access_id: Option<String>,
    /// Secret for `access_id`.
    pub secret_access_key: Option<String>,
    /// Object prefix to mount instead of the whole bucket.
    pub prefix: Option<String>,
    /// Region the bucket is in.
    pub region: Option<String>,
    /// Endpoint to reach the bucket through.
    pub endpoint_url: Option<String>,
    /// Path to a service-account key file inside the sandbox.
    pub service_account_file: Option<String>,
    /// Service-account credentials, inline.
    pub service_account_credentials: Option<String>,
    /// A minted access token.
    pub access_token: Option<String>,
}

/// An Azure Blob Storage container.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AzureBlobMount {
    /// The storage account.
    pub account: String,
    /// The container inside it.
    pub container: String,
    /// Endpoint to reach the account through.
    pub endpoint: Option<String>,
    /// Managed-identity client id. **Authority even though it is not secret**: it selects an
    /// identity the sandbox may then borrow.
    pub identity_client_id: Option<String>,
    /// Account key.
    pub account_key: Option<String>,
}

/// A Box folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoxMount {
    /// Folder path to mount.
    pub path: Option<String>,
    /// OAuth client id.
    pub client_id: Option<String>,
    /// OAuth client secret.
    pub client_secret: Option<String>,
    /// A minted access token.
    pub access_token: Option<String>,
    /// A stored token document.
    pub token: Option<String>,
    /// Path to a Box configuration file inside the sandbox.
    pub box_config_file: Option<String>,
    /// Box configuration, inline.
    pub config_credentials: Option<String>,
    /// Whether the JWT subject is a user or an enterprise.
    pub box_sub_type: BoxSubType,
    /// Folder id to treat as the root.
    pub root_folder_id: Option<String>,
    /// Account to act as.
    pub impersonate: Option<String>,
    /// Account the content belongs to.
    pub owned_by: Option<String>,
}

/// Whether a Box mount authenticates as a user or as an enterprise.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BoxSubType {
    /// A user account.
    #[default]
    User,
    /// An enterprise account.
    Enterprise,
}

impl BoxSubType {
    /// The subject type's wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Enterprise => "enterprise",
        }
    }
}

/// A Cloudflare R2 bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct R2Mount {
    /// The bucket to mount.
    pub bucket: String,
    /// The Cloudflare account the bucket belongs to.
    pub account_id: String,
    /// Access key, which must be set together with `secret_access_key`.
    pub access_key_id: Option<String>,
    /// Secret key, which must be set together with `access_key_id`.
    pub secret_access_key: Option<String>,
    /// A custom domain to reach the bucket through.
    pub custom_domain: Option<String>,
}

/// An Amazon S3 Files file system.
///
/// Mounts one that already exists. Nothing here creates the file system, its mount target, the VPC
/// or the bucket configuration, and the sandbox has to be running somewhere a mount target is
/// reachable from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct S3FilesMount {
    /// The file system to mount.
    pub file_system_id: String,
    /// A directory inside it to mount instead of the whole file system.
    pub subpath: Option<String>,
    /// Address of a mount target to reach it through.
    pub mount_target_ip: Option<String>,
    /// Access point to mount through.
    pub access_point: Option<String>,
    /// Region the file system is in.
    pub region: Option<String>,
    /// Further options handed to the mount helper.
    pub extra_options: BTreeMap<String, Option<String>>,
}

/// External storage attached at a workspace path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    provider: MountProvider,
    strategy: MountStrategy,
    path: Option<String>,
    read_only: bool,
}

/// Why a mount could not be declared.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MountConfigError {
    /// The provider does not support the chosen in-container pattern.
    #[error("invalid mount_pattern type")]
    UnsupportedPattern {
        /// The mount type that was asked for it.
        mount_type: String,
        /// The pattern it was given.
        pattern: String,
    },
    /// The provider does not support the chosen volume driver.
    #[error("invalid Docker volume driver")]
    UnsupportedDriver {
        /// The mount type that was asked for it.
        mount_type: String,
        /// The driver it was given.
        driver: String,
    },
    /// The provider supports neither strategy, so it can never be attached.
    #[error("mount type must support at least one mount strategy")]
    NoSupportedStrategy {
        /// The mount type.
        mount_type: String,
    },
}

impl Mount {
    /// Declares a mount, checking the strategy against what the provider supports.
    ///
    /// # Errors
    ///
    /// Returns [`MountConfigError`] when the provider cannot be attached the way the strategy says.
    /// The check happens here rather than at activation so a mistake surfaces while the manifest is
    /// being written, not as a mount command failing inside a container minutes later.
    ///
    /// An extension provider is not checked: this crate does not know its support matrix, and
    /// refusing what it cannot evaluate would shut out every host-declared mount type.
    pub fn new(provider: MountProvider, strategy: MountStrategy) -> Result<Self, MountConfigError> {
        if provider.is_modelled()
            && provider.supported_patterns().is_empty()
            && provider.supported_drivers().is_empty()
        {
            return Err(MountConfigError::NoSupportedStrategy {
                mount_type: provider.type_name().to_owned(),
            });
        }
        match &strategy {
            MountStrategy::InContainer { pattern }
                if provider.is_modelled()
                    && !provider.supported_patterns().contains(&pattern.as_str()) =>
            {
                return Err(MountConfigError::UnsupportedPattern {
                    mount_type: provider.type_name().to_owned(),
                    pattern: pattern.as_str().to_owned(),
                });
            }
            MountStrategy::DockerVolume { driver, .. }
                if provider.is_modelled()
                    && !provider.supported_drivers().contains(&driver.as_str()) =>
            {
                return Err(MountConfigError::UnsupportedDriver {
                    mount_type: provider.type_name().to_owned(),
                    driver: driver.clone(),
                });
            }
            _ => {}
        }
        Ok(Self {
            provider,
            strategy,
            path: None,
            read_only: true,
        })
    }

    /// Attaches the mount somewhere other than the path the entry is declared at.
    #[must_use]
    pub fn at(mut self, mount_path: impl Into<String>) -> Self {
        self.path = Some(mount_path.into());
        self
    }

    /// The same mount, attached by a different strategy.
    ///
    /// Not re-checked against the support matrix: this is how an activation check asks about the
    /// strategy that is actually about to run, which may be a backend's replacement for the
    /// declared one, and refusing it here would answer a different question.
    #[must_use]
    pub(crate) fn with_strategy(mut self, strategy: MountStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Allows writing through the mount.
    #[must_use]
    pub const fn writable(mut self, writable: bool) -> Self {
        self.read_only = !writable;
        self
    }

    /// What is being mounted.
    #[must_use]
    pub const fn provider(&self) -> &MountProvider {
        &self.provider
    }

    /// Who attaches it.
    #[must_use]
    pub const fn strategy(&self) -> &MountStrategy {
        &self.strategy
    }

    /// Where it is attached, when that differs from where the entry is declared.
    #[must_use]
    pub fn mount_path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Whether the sandbox may only read through it.
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The mount's entry type.
    #[must_use]
    pub fn type_name(&self) -> &str {
        self.provider.type_name()
    }

    /// Resolves where this mount actually appears, given a workspace root and where it was declared.
    ///
    /// An explicit `mount_path` wins. A relative one is measured from the workspace root rather than
    /// from the declaration, so a manifest stays portable across backends whose roots differ.
    ///
    /// # Errors
    ///
    /// Returns [`crate::sandbox::ErrorCode::InvalidManifestPath`] when `mount_path` is written in
    /// Windows drive syntax.
    pub fn resolve_path_for_root(
        &self,
        manifest_root: &PosixPath,
        dest: &PosixPath,
    ) -> Result<PosixPath, SandboxError> {
        let Some(mount_path) = &self.path else {
            let Some(relative) = dest.relative_to(manifest_root) else {
                // Already absolute and outside the root: the caller resolved it somewhere else, and
                // re-anchoring would nest one root inside another.
                return Ok(if dest.is_absolute() {
                    dest.clone()
                } else {
                    manifest_root.join(dest.as_str())
                });
            };
            return Ok(if dest.is_absolute() {
                manifest_root.join(relative.as_str())
            } else {
                manifest_root.join(dest.as_str())
            });
        };

        if let Some(windows_path) = windows_absolute_path(mount_path) {
            return Err(invalid_entry_path(&windows_path, "absolute"));
        }
        let mount_path = PosixPath::coerce(mount_path);
        Ok(if mount_path.is_absolute() {
            mount_path
        } else {
            manifest_root.join(mount_path.as_str())
        })
    }

    /// Renders the mount's own fields, which sit beside the fields every entry carries.
    pub(crate) fn to_fields(&self) -> JsonMap<String, Value> {
        let mut fields = match &self.provider {
            MountProvider::S3(mount) => s3_fields(mount),
            MountProvider::Gcs(mount) => gcs_fields(mount),
            MountProvider::AzureBlob(mount) => azure_blob_fields(mount),
            MountProvider::Box(mount) => box_fields(mount),
            MountProvider::R2(mount) => r2_fields(mount),
            MountProvider::S3Files(mount) => s3_files_fields(mount),
            MountProvider::Extension(payload) => payload
                .fields()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        };
        fields.insert(
            "mount_path".to_owned(),
            self.path.clone().map_or(Value::Null, Value::from),
        );
        fields.insert("read_only".to_owned(), Value::from(self.read_only));
        fields.insert("mount_strategy".to_owned(), strategy_json(&self.strategy));
        fields
    }
}

impl Mount {
    /// Reads a mount out of an entry payload.
    ///
    /// The strategy is re-checked against the provider's support matrix, exactly as construction
    /// checks it: a payload that arrived over a wire is the case where an unsupported combination is
    /// most likely, and accepting it would defer the refusal to a mount command inside a container.
    pub(crate) fn from_fields(
        mount_type: &str,
        fields: &BTreeMap<String, Value>,
        strategies: &TypeRegistry,
    ) -> Result<Self, String> {
        let text = |key: &str| -> Result<Option<String>, String> {
            match fields.get(key) {
                Some(Value::String(value)) => Ok(Some(value.clone())),
                None | Some(Value::Null) => Ok(None),
                Some(_) => Err(format!("`{key}` must be a string")),
            }
        };
        let required = |key: &str| -> Result<String, String> {
            text(key)?.ok_or_else(|| format!("`{key}` is required"))
        };

        let provider = match mount_type {
            S3_MOUNT_TYPE => MountProvider::S3(S3Mount {
                bucket: required("bucket")?,
                access_key_id: text("access_key_id")?,
                secret_access_key: text("secret_access_key")?,
                session_token: text("session_token")?,
                prefix: text("prefix")?,
                region: text("region")?,
                endpoint_url: text("endpoint_url")?,
                s3_provider: text("s3_provider")?.unwrap_or_else(|| DEFAULT_S3_PROVIDER.to_owned()),
            }),
            GCS_MOUNT_TYPE => MountProvider::Gcs(GcsMount {
                bucket: required("bucket")?,
                access_id: text("access_id")?,
                secret_access_key: text("secret_access_key")?,
                prefix: text("prefix")?,
                region: text("region")?,
                endpoint_url: text("endpoint_url")?,
                service_account_file: text("service_account_file")?,
                service_account_credentials: text("service_account_credentials")?,
                access_token: text("access_token")?,
            }),
            AZURE_BLOB_MOUNT_TYPE => MountProvider::AzureBlob(AzureBlobMount {
                account: required("account")?,
                container: required("container")?,
                endpoint: text("endpoint")?,
                identity_client_id: text("identity_client_id")?,
                account_key: text("account_key")?,
            }),
            BOX_MOUNT_TYPE => MountProvider::Box(BoxMount {
                path: text("path")?,
                client_id: text("client_id")?,
                client_secret: text("client_secret")?,
                access_token: text("access_token")?,
                token: text("token")?,
                box_config_file: text("box_config_file")?,
                config_credentials: text("config_credentials")?,
                box_sub_type: match text("box_sub_type")?.as_deref() {
                    None | Some("user") => BoxSubType::User,
                    Some("enterprise") => BoxSubType::Enterprise,
                    Some(_) => return Err("`box_sub_type` must be user or enterprise".to_owned()),
                },
                root_folder_id: text("root_folder_id")?,
                impersonate: text("impersonate")?,
                owned_by: text("owned_by")?,
            }),
            R2_MOUNT_TYPE => MountProvider::R2(R2Mount {
                bucket: required("bucket")?,
                account_id: required("account_id")?,
                access_key_id: text("access_key_id")?,
                secret_access_key: text("secret_access_key")?,
                custom_domain: text("custom_domain")?,
            }),
            S3_FILES_MOUNT_TYPE => MountProvider::S3Files(S3FilesMount {
                file_system_id: required("file_system_id")?,
                subpath: text("subpath")?,
                mount_target_ip: text("mount_target_ip")?,
                access_point: text("access_point")?,
                region: text("region")?,
                extra_options: read_optional_map(fields.get("extra_options"))?,
            }),
            _ => {
                let mut payload = DiscriminatedPayload::new(mount_type);
                for (key, value) in fields {
                    if !MOUNT_COMMON_FIELDS.contains(&key.as_str()) {
                        payload = payload.with_field(key.clone(), value.clone());
                    }
                }
                MountProvider::Extension(payload)
            }
        };

        let strategy = read_strategy(
            strategies,
            fields
                .get("mount_strategy")
                .ok_or_else(|| "`mount_strategy` is required".to_owned())?,
        )?;
        let mut mount = Self::new(provider, strategy).map_err(|error| error.to_string())?;
        mount.path = text("mount_path")?;
        if let Some(read_only) = fields.get("read_only") {
            mount.read_only = read_only
                .as_bool()
                .ok_or_else(|| "`read_only` must be a boolean".to_owned())?;
        }
        Ok(mount)
    }
}

/// Fields a mount carries outside its provider, which therefore never join an extension's own.
const MOUNT_COMMON_FIELDS: [&str; 3] = ["mount_path", "read_only", "mount_strategy"];

/// Reads a strategy, routing it through the registry the host assembled.
fn read_strategy(strategies: &TypeRegistry, value: &Value) -> Result<MountStrategy, String> {
    // Admission runs the owner's validation and normalization before any fields are read.
    let payload = strategies.parse(value).map_err(|error| error.to_string())?;
    let fields = payload.fields();

    match payload.type_name() {
        IN_CONTAINER_STRATEGY_TYPE => {
            let pattern = fields
                .get("pattern")
                .ok_or_else(|| "in-container mount strategy requires a `pattern`".to_owned())?;
            Ok(MountStrategy::InContainer {
                pattern: read_pattern(pattern)?,
            })
        }
        DOCKER_VOLUME_STRATEGY_TYPE => {
            let driver = match fields.get("driver") {
                Some(Value::String(driver)) => driver.clone(),
                _ => return Err("docker-volume mount strategy requires a `driver`".to_owned()),
            };
            let mut driver_options = BTreeMap::new();
            if let Some(options) = fields.get("driver_options").filter(|v| !v.is_null()) {
                let Value::Object(options) = options else {
                    return Err("`driver_options` must be a mapping".to_owned());
                };
                for (key, value) in options {
                    let Value::String(value) = value else {
                        return Err("`driver_options` values must be strings".to_owned());
                    };
                    driver_options.insert(key.clone(), value.clone());
                }
            }
            Ok(MountStrategy::DockerVolume {
                driver,
                driver_options,
            })
        }
        _ => Ok(MountStrategy::Extension(payload)),
    }
}

/// Reads a pattern, refusing any tool outside the closed set.
fn read_pattern(value: &Value) -> Result<MountPattern, String> {
    let Value::Object(fields) = value else {
        return Err("mount pattern must be an object".to_owned());
    };
    let Some(Value::String(pattern_type)) = fields.get("type") else {
        return Err("mount pattern must include a string `type` field".to_owned());
    };

    match pattern_type.as_str() {
        "fuse" => read_fuse_pattern(fields),
        "mountpoint" => {
            let options = read_options(fields)?;
            Ok(MountPattern::Mountpoint(MountpointOptions {
                prefix: opt_str(options, "prefix")?,
                region: opt_str(options, "region")?,
                endpoint_url: opt_str(options, "endpoint_url")?,
            }))
        }
        "rclone" => read_rclone_pattern(fields),
        "s3files" => {
            let options = read_options(fields)?;
            Ok(MountPattern::S3Files(S3FilesOptions {
                mount_target_ip: opt_str(options, "mount_target_ip")?,
                access_point: opt_str(options, "access_point")?,
                region: opt_str(options, "region")?,
                extra_options: read_optional_map(options.get("extra_options"))?,
            }))
        }
        // The reference's pattern field is a closed discriminated union, so an unregistered tool is
        // refused rather than carried: a mount whose tool nothing can run is not a mount.
        other => Err(format!("unknown mount pattern type `{other}`")),
    }
}

/// Reads the options of a FUSE mount.
fn read_fuse_pattern(fields: &JsonMap<String, Value>) -> Result<MountPattern, String> {
    let defaults = FuseOptions::default();
    let options = FuseOptions {
        allow_other: opt_bool(fields, "allow_other")?.unwrap_or(defaults.allow_other),
        log_type: opt_str(fields, "log_type")?.unwrap_or(defaults.log_type),
        log_level: opt_str(fields, "log_level")?.unwrap_or(defaults.log_level),
        cache_type: match opt_str(fields, "cache_type")?.as_deref() {
            None | Some("block_cache") => FuseCacheType::BlockCache,
            Some("file_cache") => FuseCacheType::FileCache,
            Some(_) => return Err("`cache_type` must be block_cache or file_cache".to_owned()),
        },
        cache_path: opt_str(fields, "cache_path")?,
        cache_size_mb: opt_u64(fields, "cache_size_mb")?,
        block_cache_block_size_mb: opt_u64(fields, "block_cache_block_size_mb")?
            .unwrap_or(defaults.block_cache_block_size_mb),
        block_cache_disk_timeout_sec: opt_u64(fields, "block_cache_disk_timeout_sec")?
            .unwrap_or(defaults.block_cache_disk_timeout_sec),
        file_cache_timeout_sec: opt_u64(fields, "file_cache_timeout_sec")?
            .unwrap_or(defaults.file_cache_timeout_sec),
        file_cache_max_size_mb: opt_u64(fields, "file_cache_max_size_mb")?,
        attr_cache_timeout_sec: opt_u64(fields, "attr_cache_timeout_sec")?,
        entry_cache_timeout_sec: opt_u64(fields, "entry_cache_timeout_sec")?,
        negative_entry_cache_timeout_sec: opt_u64(fields, "negative_entry_cache_timeout_sec")?,
    };
    // Checked on the way in, as the reference checks it at construction: a cache directory that
    // cannot be used should stop the manifest, not the mount command minutes later.
    options
        .checked_cache_path()
        .map_err(|error| error.message().to_owned())?;
    Ok(MountPattern::Fuse(options))
}

/// Reads the options of an rclone mount.
fn read_rclone_pattern(fields: &JsonMap<String, Value>) -> Result<MountPattern, String> {
    Ok(MountPattern::Rclone(RcloneOptions {
        mode: match opt_str(fields, "mode")?.as_deref() {
            None | Some("fuse") => RcloneMode::Fuse,
            Some("nfs") => RcloneMode::Nfs,
            Some(_) => return Err("`mode` must be fuse or nfs".to_owned()),
        },
        remote_name: opt_str(fields, "remote_name")?,
        extra_args: read_string_list(fields.get("extra_args"))?.unwrap_or_default(),
        nfs_addr: opt_str(fields, "nfs_addr")?,
        nfs_mount_options: read_string_list(fields.get("nfs_mount_options"))?,
        config_file_path: opt_str(fields, "config_file_path")?,
    }))
}

/// Reads the nested `options` object a pattern may carry.
fn read_options(fields: &JsonMap<String, Value>) -> Result<&JsonMap<String, Value>, String> {
    static EMPTY: std::sync::LazyLock<JsonMap<String, Value>> =
        std::sync::LazyLock::new(JsonMap::new);
    match fields.get("options") {
        None | Some(Value::Null) => Ok(&EMPTY),
        Some(Value::Object(options)) => Ok(options),
        Some(_) => Err("mount pattern `options` must be an object".to_owned()),
    }
}

/// Reads an optional string, refusing a field that is present and is not one.
///
/// A field of the wrong type is not the same as an absent one. Reading `mode: 123` as "unset" would
/// silently pick the default tool, which is the sort of typo that turns into a mount nobody asked
/// for rather than an error somebody can fix.
fn opt_str(fields: &JsonMap<String, Value>, key: &str) -> Result<Option<String>, String> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

/// Reads an optional boolean, refusing a field that is present and is not one.
fn opt_bool(fields: &JsonMap<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("`{key}` must be a boolean")),
    }
}

/// Reads an optional non-negative integer, refusing a field that is present and is not one.
fn opt_u64(fields: &JsonMap<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
        Some(_) => Err(format!("`{key}` must be a non-negative integer")),
    }
}

/// Reads a list of strings, or `None` when the field is absent.
fn read_string_list(value: Option<&Value>) -> Result<Option<Vec<String>>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "mount pattern list values must be strings".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err("mount pattern list must be a list".to_owned()),
    }
}

/// Reads a mapping whose values may be absent.
fn read_optional_map(value: Option<&Value>) -> Result<BTreeMap<String, Option<String>>, String> {
    match value {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(fields)) => fields
            .iter()
            .map(|(key, value)| match value {
                Value::Null => Ok((key.clone(), None)),
                Value::String(value) => Ok((key.clone(), Some(value.clone()))),
                _ => Err("mount option values must be strings or null".to_owned()),
            })
            .collect(),
        Some(_) => Err("mount options must be a mapping".to_owned()),
    }
}

/// Renders a strategy, discriminator included.
fn strategy_json(strategy: &MountStrategy) -> Value {
    match strategy {
        MountStrategy::InContainer { pattern } => Value::Object(JsonMap::from_iter([
            ("type".to_owned(), Value::from("in_container")),
            ("pattern".to_owned(), pattern_json(pattern)),
        ])),
        MountStrategy::DockerVolume {
            driver,
            driver_options,
        } => Value::Object(JsonMap::from_iter([
            ("type".to_owned(), Value::from("docker_volume")),
            ("driver".to_owned(), Value::from(driver.clone())),
            (
                "driver_options".to_owned(),
                Value::Object(
                    driver_options
                        .iter()
                        .map(|(key, value)| (key.clone(), Value::from(value.clone())))
                        .collect(),
                ),
            ),
        ])),
        MountStrategy::Extension(payload) => payload.to_json(),
    }
}

/// Renders a pattern, discriminator included.
fn pattern_json(pattern: &MountPattern) -> Value {
    let mut fields = match pattern {
        MountPattern::Fuse(options) => fuse_fields(options),
        MountPattern::Mountpoint(options) => mountpoint_fields(options),
        MountPattern::Rclone(options) => rclone_fields(options),
        MountPattern::S3Files(options) => s3_files_pattern_fields(options),
    };
    fields.insert("type".to_owned(), Value::from(pattern.as_str()));
    Value::Object(fields)
}

/// Renders the options of a FUSE mount.
fn fuse_fields(options: &FuseOptions) -> JsonMap<String, Value> {
    JsonMap::from_iter([
        ("allow_other".to_owned(), Value::from(options.allow_other)),
        ("log_type".to_owned(), Value::from(options.log_type.clone())),
        (
            "log_level".to_owned(),
            Value::from(options.log_level.clone()),
        ),
        (
            "cache_type".to_owned(),
            Value::from(options.cache_type.as_str()),
        ),
        (
            "cache_path".to_owned(),
            optional(options.cache_path.clone()),
        ),
        ("cache_size_mb".to_owned(), optional(options.cache_size_mb)),
        (
            "block_cache_block_size_mb".to_owned(),
            Value::from(options.block_cache_block_size_mb),
        ),
        (
            "block_cache_disk_timeout_sec".to_owned(),
            Value::from(options.block_cache_disk_timeout_sec),
        ),
        (
            "file_cache_timeout_sec".to_owned(),
            Value::from(options.file_cache_timeout_sec),
        ),
        (
            "file_cache_max_size_mb".to_owned(),
            optional(options.file_cache_max_size_mb),
        ),
        (
            "attr_cache_timeout_sec".to_owned(),
            optional(options.attr_cache_timeout_sec),
        ),
        (
            "entry_cache_timeout_sec".to_owned(),
            optional(options.entry_cache_timeout_sec),
        ),
        (
            "negative_entry_cache_timeout_sec".to_owned(),
            optional(options.negative_entry_cache_timeout_sec),
        ),
    ])
}

/// Renders the options of a Mountpoint mount, which the reference nests under `options`.
fn mountpoint_fields(options: &MountpointOptions) -> JsonMap<String, Value> {
    JsonMap::from_iter([(
        "options".to_owned(),
        Value::Object(JsonMap::from_iter([
            ("prefix".to_owned(), optional(options.prefix.clone())),
            ("region".to_owned(), optional(options.region.clone())),
            (
                "endpoint_url".to_owned(),
                optional(options.endpoint_url.clone()),
            ),
        ])),
    )])
}

/// Renders the options of an rclone mount.
fn rclone_fields(options: &RcloneOptions) -> JsonMap<String, Value> {
    JsonMap::from_iter([
        ("mode".to_owned(), Value::from(options.mode.as_str())),
        (
            "remote_name".to_owned(),
            optional(options.remote_name.clone()),
        ),
        (
            "extra_args".to_owned(),
            Value::from(options.extra_args.clone()),
        ),
        ("nfs_addr".to_owned(), optional(options.nfs_addr.clone())),
        (
            "nfs_mount_options".to_owned(),
            options
                .nfs_mount_options
                .clone()
                .map_or(Value::Null, Value::from),
        ),
        (
            "config_file_path".to_owned(),
            optional(options.config_file_path.clone()),
        ),
    ])
}

/// Renders the options of an S3 Files mount, which the reference nests under `options`.
fn s3_files_pattern_fields(options: &S3FilesOptions) -> JsonMap<String, Value> {
    JsonMap::from_iter([(
        "options".to_owned(),
        Value::Object(JsonMap::from_iter([
            (
                "mount_target_ip".to_owned(),
                optional(options.mount_target_ip.clone()),
            ),
            (
                "access_point".to_owned(),
                optional(options.access_point.clone()),
            ),
            ("region".to_owned(), optional(options.region.clone())),
            (
                "extra_options".to_owned(),
                optional_map(&options.extra_options),
            ),
        ])),
    )])
}

/// Renders an optional value, writing `null` for absence.
fn optional<T: Into<Value>>(value: Option<T>) -> Value {
    value.map_or(Value::Null, Into::into)
}

/// Renders a mapping whose values may be absent.
fn optional_map(values: &BTreeMap<String, Option<String>>) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(key, value)| (key.clone(), optional(value.clone())))
            .collect(),
    )
}

/// Renders the fields of an Amazon S3 mount.
fn s3_fields(mount: &S3Mount) -> JsonMap<String, Value> {
    JsonMap::from_iter([
        ("bucket".to_owned(), Value::from(mount.bucket.clone())),
        (
            "access_key_id".to_owned(),
            optional(mount.access_key_id.clone()),
        ),
        (
            "secret_access_key".to_owned(),
            optional(mount.secret_access_key.clone()),
        ),
        (
            "session_token".to_owned(),
            optional(mount.session_token.clone()),
        ),
        ("prefix".to_owned(), optional(mount.prefix.clone())),
        ("region".to_owned(), optional(mount.region.clone())),
        (
            "endpoint_url".to_owned(),
            optional(mount.endpoint_url.clone()),
        ),
        (
            "s3_provider".to_owned(),
            Value::from(mount.s3_provider.clone()),
        ),
    ])
}

/// Renders the fields of a Google Cloud Storage mount.
fn gcs_fields(mount: &GcsMount) -> JsonMap<String, Value> {
    JsonMap::from_iter([
        ("bucket".to_owned(), Value::from(mount.bucket.clone())),
        ("access_id".to_owned(), optional(mount.access_id.clone())),
        (
            "secret_access_key".to_owned(),
            optional(mount.secret_access_key.clone()),
        ),
        ("prefix".to_owned(), optional(mount.prefix.clone())),
        ("region".to_owned(), optional(mount.region.clone())),
        (
            "endpoint_url".to_owned(),
            optional(mount.endpoint_url.clone()),
        ),
        (
            "service_account_file".to_owned(),
            optional(mount.service_account_file.clone()),
        ),
        (
            "service_account_credentials".to_owned(),
            optional(mount.service_account_credentials.clone()),
        ),
        (
            "access_token".to_owned(),
            optional(mount.access_token.clone()),
        ),
    ])
}

/// Renders the fields of an Azure Blob Storage mount.
fn azure_blob_fields(mount: &AzureBlobMount) -> JsonMap<String, Value> {
    JsonMap::from_iter([
        ("account".to_owned(), Value::from(mount.account.clone())),
        ("container".to_owned(), Value::from(mount.container.clone())),
        ("endpoint".to_owned(), optional(mount.endpoint.clone())),
        (
            "identity_client_id".to_owned(),
            optional(mount.identity_client_id.clone()),
        ),
        (
            "account_key".to_owned(),
            optional(mount.account_key.clone()),
        ),
    ])
}

/// Renders the fields of a Box mount.
fn box_fields(mount: &BoxMount) -> JsonMap<String, Value> {
    JsonMap::from_iter([
        ("path".to_owned(), optional(mount.path.clone())),
        ("client_id".to_owned(), optional(mount.client_id.clone())),
        (
            "client_secret".to_owned(),
            optional(mount.client_secret.clone()),
        ),
        (
            "access_token".to_owned(),
            optional(mount.access_token.clone()),
        ),
        ("token".to_owned(), optional(mount.token.clone())),
        (
            "box_config_file".to_owned(),
            optional(mount.box_config_file.clone()),
        ),
        (
            "config_credentials".to_owned(),
            optional(mount.config_credentials.clone()),
        ),
        (
            "box_sub_type".to_owned(),
            Value::from(mount.box_sub_type.as_str()),
        ),
        (
            "root_folder_id".to_owned(),
            optional(mount.root_folder_id.clone()),
        ),
        (
            "impersonate".to_owned(),
            optional(mount.impersonate.clone()),
        ),
        ("owned_by".to_owned(), optional(mount.owned_by.clone())),
    ])
}

/// Renders the fields of a Cloudflare R2 mount.
fn r2_fields(mount: &R2Mount) -> JsonMap<String, Value> {
    JsonMap::from_iter([
        ("bucket".to_owned(), Value::from(mount.bucket.clone())),
        (
            "account_id".to_owned(),
            Value::from(mount.account_id.clone()),
        ),
        (
            "access_key_id".to_owned(),
            optional(mount.access_key_id.clone()),
        ),
        (
            "secret_access_key".to_owned(),
            optional(mount.secret_access_key.clone()),
        ),
        (
            "custom_domain".to_owned(),
            optional(mount.custom_domain.clone()),
        ),
    ])
}

/// Renders the fields of an Amazon S3 Files mount.
fn s3_files_fields(mount: &S3FilesMount) -> JsonMap<String, Value> {
    JsonMap::from_iter([
        (
            "file_system_id".to_owned(),
            Value::from(mount.file_system_id.clone()),
        ),
        ("subpath".to_owned(), optional(mount.subpath.clone())),
        (
            "mount_target_ip".to_owned(),
            optional(mount.mount_target_ip.clone()),
        ),
        (
            "access_point".to_owned(),
            optional(mount.access_point.clone()),
        ),
        ("region".to_owned(), optional(mount.region.clone())),
        (
            "extra_options".to_owned(),
            optional_map(&mount.extra_options),
        ),
    ])
}

/// The fields of each built-in mount type that grant access, rather than merely describing it.
///
/// **Not the same question as "is it secret".** An Azure managed-identity client id is not a secret
/// and still selects an identity the sandbox may borrow; a path to a service-account file names
/// authority the sandbox can then read. A host deciding whether a manifest may cross a trust
/// boundary asks this, not whether a field looks like a password.
#[must_use]
pub fn authority_fields(mount_type: &str) -> &'static [&'static str] {
    match mount_type {
        AZURE_BLOB_MOUNT_TYPE => &["identity_client_id", "account_key"],
        BOX_MOUNT_TYPE => &[
            "client_secret",
            "access_token",
            "token",
            "box_config_file",
            "config_credentials",
        ],
        GCS_MOUNT_TYPE => &[
            "access_id",
            "secret_access_key",
            "service_account_file",
            "service_account_credentials",
            "access_token",
        ],
        R2_MOUNT_TYPE => &["access_key_id", "secret_access_key"],
        S3_MOUNT_TYPE => &["access_key_id", "secret_access_key", "session_token"],
        _ => &[],
    }
}

/// The fields that name a file the sandbox reads authority out of.
///
/// A subset of [`authority_fields`], called out because the value is a path rather than the secret
/// itself: redacting the path still leaves the file where it was.
#[must_use]
pub fn authority_file_fields(mount_type: &str) -> &'static [&'static str] {
    match mount_type {
        BOX_MOUNT_TYPE => &["box_config_file"],
        GCS_MOUNT_TYPE => &["service_account_file"],
        _ => &[],
    }
}

/// The fields a caller may write a URL into.
///
/// A URL can carry credentials in its userinfo or query, so these are checked for inline authority
/// even though the field is nominally an endpoint.
#[must_use]
pub fn url_fields(mount_type: &str) -> &'static [&'static str] {
    match mount_type {
        AZURE_BLOB_MOUNT_TYPE => &["endpoint"],
        GCS_MOUNT_TYPE | S3_MOUNT_TYPE => &["endpoint_url"],
        R2_MOUNT_TYPE => &["custom_domain"],
        _ => &[],
    }
}

/// The credential sets that must be complete when any of their fields is set.
///
/// Returns the fields that activate the set and the fields required once it is active. An S3 mount
/// given only a session token has no key to use it with, and the mount command would fall back to
/// whatever ambient credentials the sandbox happens to have — reaching storage the manifest never
/// named.
#[must_use]
pub fn credential_set(
    mount_type: &str,
) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match mount_type {
        GCS_MOUNT_TYPE => Some((
            &["access_id", "secret_access_key"],
            &["access_id", "secret_access_key"],
        )),
        R2_MOUNT_TYPE => Some((
            &["access_key_id", "secret_access_key"],
            &["access_key_id", "secret_access_key"],
        )),
        S3_MOUNT_TYPE => Some((
            &["access_key_id", "secret_access_key", "session_token"],
            &["access_key_id", "secret_access_key"],
        )),
        _ => None,
    }
}

/// Whether a URL carries credentials in its userinfo or query.
///
/// Deliberately blunt, and read the way the reference reads it through Python's `urlsplit`: any
/// `@` counts, any non-empty query counts, and so does a value `urlsplit` refuses to parse at all.
/// An endpoint that needs a query string is rare, and treating a rare false positive as authority
/// costs a caller one acknowledgement, while missing one leaks a credential into durable state.
///
/// A `?` after a `#` belongs to the fragment, not the query, and does not count; an empty query
/// (`https://host/?`) does not count either.
#[must_use]
pub fn url_carries_inline_authority(value: &str) -> bool {
    if value.contains('@') || netloc_is_malformed(value) {
        return true;
    }
    let before_fragment = value.split_once('#').map_or(value, |(head, _)| head);
    before_fragment
        .split_once('?')
        .is_some_and(|(_, query)| !query.is_empty())
}

/// Whether `urlsplit` would refuse the URL's network location.
///
/// It refuses a location with an unbalanced bracket, and one whose bracketed host is not an IPv6
/// address (or an `IPvFuture` literal), which is what a caller who pasted half an address gets.
fn netloc_is_malformed(value: &str) -> bool {
    let rest = match value.split_once(':') {
        Some((scheme, rest)) if is_url_scheme(scheme) => rest,
        _ => value,
    };
    let Some(after) = rest.strip_prefix("//") else {
        return false;
    };
    let netloc = &after[..after.find(['/', '?', '#']).unwrap_or(after.len())];
    let opens = netloc.contains('[');
    if opens != netloc.contains(']') {
        return true;
    }
    if !opens {
        return false;
    }
    let Some((before, bracketed)) = netloc.split_once('[') else {
        return false;
    };
    if !before.is_empty() {
        return true;
    }
    let (host, port) = bracketed.split_once(']').unwrap_or((bracketed, ""));
    if !port.is_empty() && !port.starts_with(':') {
        return true;
    }
    !bracketed_host_is_valid(host)
}

/// Whether text before the first `:` is a scheme, by the rule `urlsplit` applies.
fn is_url_scheme(candidate: &str) -> bool {
    candidate
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Whether a bracketed host is an address `urlsplit` accepts.
fn bracketed_host_is_valid(host: &str) -> bool {
    if let Some(future) = host.strip_prefix('v') {
        // `v<hex>.<anything>`, the IPvFuture form.
        return future.split_once('.').is_some_and(|(version, tail)| {
            !version.is_empty()
                && version.chars().all(|c| c.is_ascii_hexdigit())
                && !tail.is_empty()
        });
    }
    let address = host.split_once('%').map_or(host, |(address, _)| address);
    address.parse::<std::net::Ipv6Addr>().is_ok()
}
