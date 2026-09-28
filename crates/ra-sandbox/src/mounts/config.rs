//! Turning a provider's fields into what its pattern or volume driver runs with.
//!
//! Each provider knows how to describe itself to each tool that can attach it: the bucket and keys
//! a `mount-s3` invocation needs, the section an rclone configuration file has to contain, the
//! options a volume driver takes. None of it touches the sandbox except rclone, which may read an
//! existing configuration file out of the workspace to extend.
//!
//! Whether a field "is set" follows the reference exactly, and the reference asks two different
//! questions: most lines are written when a field is present at all, but a few — a credential pair,
//! a fallback between the provider's value and the pattern's default — are decided on whether the
//! value is non-empty. Each helper below says which one it asks.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use ra_core::sandbox::{
    AzureBlobMount, BoxMount, BoxSubType, ErrorCode, GcsMount, Mount, MountPattern, MountProvider,
    MountStrategy, PosixPath, R2Mount, RcloneOptions, S3FilesMount, S3Mount, SandboxError,
    SandboxResult, SandboxSession, SessionPath,
};

/// Matches any section header line.
static ANY_SECTION: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?m)^\s*\[.+\]\s*$").unwrap_or_else(|_| unreachable!("a literal"))
});

/// Where a GCS bucket is reached when the mount does not say.
const GCS_ENDPOINT: &str = "https://storage.googleapis.com";

/// What a pattern runs with, built from one provider's fields.
#[non_exhaustive]
pub enum MountPatternConfig {
    /// For `blobfuse2`.
    Fuse(FuseMountConfig),
    /// For `mount-s3`.
    Mountpoint(MountpointMountConfig),
    /// For rclone.
    Rclone(RcloneMountConfig),
    /// For `mount.s3files`.
    S3Files(S3FilesMountConfig),
}

impl std::fmt::Debug for MountPatternConfig {
    /// Names the variant only: every one of them may carry credentials.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Fuse(_) => "Fuse",
            Self::Mountpoint(_) => "Mountpoint",
            Self::Rclone(_) => "Rclone",
            Self::S3Files(_) => "S3Files",
        };
        formatter
            .debug_tuple("MountPatternConfig")
            .field(&name)
            .finish()
    }
}

/// What `blobfuse2` needs for one Azure container.
#[derive(Clone, PartialEq, Eq)]
pub struct FuseMountConfig {
    /// The storage account.
    pub account: String,
    /// The container inside it.
    pub container: String,
    /// Endpoint override.
    pub endpoint: Option<String>,
    /// Managed-identity client id.
    pub identity_client_id: Option<String>,
    /// Account key.
    pub account_key: Option<String>,
    /// The mount type this was built for.
    pub mount_type: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// What `mount-s3` needs for one bucket.
#[derive(Clone, PartialEq, Eq)]
pub struct MountpointMountConfig {
    /// The bucket.
    pub bucket: String,
    /// Access key.
    pub access_key_id: Option<String>,
    /// Secret for the access key.
    pub secret_access_key: Option<String>,
    /// Session token for temporary credentials.
    pub session_token: Option<String>,
    /// Object prefix to mount.
    pub prefix: Option<String>,
    /// Region.
    pub region: Option<String>,
    /// Endpoint.
    pub endpoint_url: Option<String>,
    /// The mount type this was built for.
    pub mount_type: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// What rclone needs for one remote.
#[derive(Clone, PartialEq, Eq)]
pub struct RcloneMountConfig {
    /// The remote's section name.
    pub remote_name: String,
    /// The path inside the remote.
    pub remote_path: String,
    /// Which provider family the remote is, which also names a derived remote.
    pub remote_kind: String,
    /// The mount type this was built for.
    pub mount_type: String,
    /// The configuration file's text, or `None` when only the remote's identity is needed.
    pub config_text: Option<String>,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// What `mount.s3files` needs for one file system.
#[derive(Clone, PartialEq, Eq)]
pub struct S3FilesMountConfig {
    /// The file system.
    pub file_system_id: String,
    /// A directory inside it.
    pub subpath: Option<String>,
    /// Mount target address.
    pub mount_target_ip: Option<String>,
    /// Access point.
    pub access_point: Option<String>,
    /// Region.
    pub region: Option<String>,
    /// Further helper options; `None` values are written as bare flags.
    pub extra_options: BTreeMap<String, Option<String>>,
    /// The mount type this was built for.
    pub mount_type: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

macro_rules! debug_without_values {
    ($($name:ident),* $(,)?) => {$(
        impl std::fmt::Debug for $name {
            /// Omits every value: the configuration exists to carry credentials to a tool.
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .field("mount_type", &self.mount_type)
                    .finish_non_exhaustive()
            }
        }
    )*};
}

debug_without_values!(
    FuseMountConfig,
    MountpointMountConfig,
    RcloneMountConfig,
    S3FilesMountConfig
);

/// What a Docker volume driver is asked for: the driver, its options, and whether read-only.
#[derive(Clone, PartialEq, Eq)]
pub struct DockerVolumeDriverConfig {
    /// The driver's name.
    pub driver: String,
    /// The options handed to it, the strategy's own `driver_options` taking precedence.
    pub options: BTreeMap<String, String>,
    /// Whether the volume is read-only.
    pub read_only: bool,
}

impl std::fmt::Debug for DockerVolumeDriverConfig {
    /// Names the driver and the option keys, never their values.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DockerVolumeDriverConfig")
            .field("driver", &self.driver)
            .field("options", &self.options.keys().collect::<Vec<_>>())
            .field("read_only", &self.read_only)
            .finish()
    }
}

/// Builds what `pattern` runs with for `mount`.
///
/// `include_config_text` is false when only the remote's identity is needed — detaching an rclone
/// mount — so an existing configuration file is not read and no text is synthesized.
///
/// # Errors
///
/// Returns [`ErrorCode::MountConfigInvalid`] when the provider cannot be attached with `pattern`,
/// for an R2 mount with half a credential pair, and for an rclone configuration file that is
/// missing its remote; the session's failure to read that file otherwise.
pub async fn build_in_container_mount_config(
    mount: &Mount,
    pattern: &MountPattern,
    session: &dyn SandboxSession,
    include_config_text: bool,
) -> SandboxResult<MountPatternConfig> {
    let mount_type = mount.type_name();
    if let MountProvider::R2(r2) = mount.provider()
        && credential_pair_is_split(r2)
    {
        return Err(split_r2_credentials(mount_type));
    }
    let MountPattern::Rclone(options) = pattern else {
        return direct_config(mount, pattern);
    };

    let session_id = session.state().session_id().simple().to_string();
    let (remote_kind, remote_path, lines) = rclone_remote(mount, options, &session_id)?;
    let remote_name = resolve_remote_name(options, &session_id, remote_kind, mount_type)?;
    let config_text = if !include_config_text {
        None
    } else if options.config_file_path.is_some() {
        let existing = read_rclone_config_text(options, session, &remote_name, mount_type).await?;
        Some(supplement_rclone_config_text(
            &existing,
            &remote_name,
            &lines,
            mount_type,
        )?)
    } else {
        Some(lines.join("\n") + "\n")
    };
    Ok(MountPatternConfig::Rclone(RcloneMountConfig {
        remote_name,
        remote_path,
        remote_kind: remote_kind.to_owned(),
        mount_type: mount_type.to_owned(),
        config_text,
        read_only: mount.is_read_only(),
    }))
}

/// The configuration for a pattern that needs nothing from the sandbox to build it.
fn direct_config(mount: &Mount, pattern: &MountPattern) -> SandboxResult<MountPatternConfig> {
    let mount_type = mount.type_name().to_owned();
    let read_only = mount.is_read_only();
    Ok(match (mount.provider(), pattern) {
        (MountProvider::S3(s3), MountPattern::Mountpoint(defaults)) => {
            MountPatternConfig::Mountpoint(MountpointMountConfig {
                bucket: s3.bucket.clone(),
                access_key_id: s3.access_key_id.clone(),
                secret_access_key: s3.secret_access_key.clone(),
                session_token: s3.session_token.clone(),
                prefix: either(s3.prefix.as_ref(), defaults.prefix.as_ref()),
                region: either(s3.region.as_ref(), defaults.region.as_ref()),
                endpoint_url: either(s3.endpoint_url.as_ref(), defaults.endpoint_url.as_ref()),
                mount_type,
                read_only,
            })
        }
        (MountProvider::Gcs(gcs), MountPattern::Mountpoint(defaults)) => {
            MountPatternConfig::Mountpoint(MountpointMountConfig {
                bucket: gcs.bucket.clone(),
                access_key_id: gcs.access_id.clone(),
                secret_access_key: gcs.secret_access_key.clone(),
                session_token: None,
                prefix: either(gcs.prefix.as_ref(), defaults.prefix.as_ref()),
                region: either(gcs.region.as_ref(), defaults.region.as_ref()),
                endpoint_url: Some(
                    either(gcs.endpoint_url.as_ref(), defaults.endpoint_url.as_ref())
                        .filter(|endpoint| !endpoint.is_empty())
                        .unwrap_or_else(|| GCS_ENDPOINT.to_owned()),
                ),
                mount_type,
                read_only,
            })
        }
        (MountProvider::AzureBlob(azure), MountPattern::Fuse(_)) => {
            MountPatternConfig::Fuse(FuseMountConfig {
                account: azure.account.clone(),
                container: azure.container.clone(),
                endpoint: azure.endpoint.clone(),
                identity_client_id: azure.identity_client_id.clone(),
                account_key: azure.account_key.clone(),
                mount_type,
                read_only,
            })
        }
        (MountProvider::S3Files(files), MountPattern::S3Files(defaults)) => {
            MountPatternConfig::S3Files(s3_files_config(files, defaults, &mount_type, read_only))
        }
        _ => return Err(invalid_pattern(mount.type_name())),
    })
}

/// An rclone remote's kind, the path inside it, and the lines its configuration section needs.
fn rclone_remote(
    mount: &Mount,
    options: &RcloneOptions,
    session_id: &str,
) -> SandboxResult<(&'static str, String, Vec<String>)> {
    let mount_type = mount.type_name();
    let name = |kind: &str| resolve_remote_name(options, session_id, kind, mount_type);
    Ok(match mount.provider() {
        MountProvider::S3(s3) => (
            "s3",
            join_remote_path(&s3.bucket, s3.prefix.as_deref()),
            s3_rclone_lines(s3, &name("s3")?),
        ),
        MountProvider::Gcs(gcs) => {
            // An HMAC-authenticated bucket goes through rclone's S3 backend, but keeps a remote name
            // of its own so it cannot collide with a real S3 mount in the same session.
            let hmac = gcs_uses_hmac(gcs);
            let kind = if hmac { "gcs_s3" } else { "gcs" };
            let remote = name(kind)?;
            let lines = if hmac {
                gcs_hmac_rclone_lines(gcs, &remote)
            } else {
                gcs_rclone_lines(gcs, &remote)
            };
            (
                kind,
                join_remote_path(&gcs.bucket, gcs.prefix.as_deref()),
                lines,
            )
        }
        MountProvider::AzureBlob(azure) => (
            "azureblob",
            azure.container.clone(),
            azure_rclone_lines(azure, &name("azureblob")?),
        ),
        MountProvider::Box(folder) => (
            "box",
            box_remote_path(folder),
            box_rclone_lines(folder, &name("box")?),
        ),
        MountProvider::R2(r2) => ("r2", r2.bucket.clone(), r2_rclone_lines(r2, &name("r2")?)),
        _ => return Err(invalid_pattern(mount_type)),
    })
}

fn invalid_pattern(mount_type: &str) -> SandboxError {
    SandboxError::mount_config("invalid mount_pattern type").with_context("type", mount_type)
}

fn split_r2_credentials(mount_type: &str) -> SandboxError {
    SandboxError::mount_config(
        "r2 credentials must include both access_key_id and secret_access_key",
    )
    .with_context("type", mount_type)
}

/// The volume-driver configuration a strategy attaches `mount` with, or `None` for a strategy the
/// sandbox attaches itself.
///
/// # Errors
///
/// Returns [`ErrorCode::MountConfigInvalid`] for a mount type with no volume driver, and for an R2
/// mount with half a credential pair; [`ErrorCode::SandboxConfigInvalid`] for a strategy a host
/// registered, whose driver configuration this crate cannot know.
pub fn docker_volume_driver_config(
    mount: &Mount,
    strategy: &MountStrategy,
) -> SandboxResult<Option<DockerVolumeDriverConfig>> {
    let (driver, driver_options) = match strategy {
        MountStrategy::InContainer { .. } => return Ok(None),
        MountStrategy::DockerVolume {
            driver,
            driver_options,
        } => (driver, driver_options),
        _ => {
            return Err(SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                ra_core::sandbox::OpName::Materialize,
                format!(
                    "cannot build a volume driver configuration for the `{}` strategy",
                    strategy.type_name()
                ),
            )
            .with_context("strategy_type", strategy.type_name()));
        }
    };
    let mut options = match mount.provider() {
        MountProvider::S3(s3) => s3_driver_options(s3, driver),
        MountProvider::Gcs(gcs) => gcs_driver_options(gcs, driver),
        MountProvider::AzureBlob(azure) => azure_driver_options(azure),
        MountProvider::Box(folder) => box_driver_options(folder),
        MountProvider::R2(r2) => {
            if credential_pair_is_split(r2) {
                return Err(split_r2_credentials(mount.type_name()));
            }
            r2_driver_options(r2)
        }
        _ => {
            return Err(SandboxError::mount_config(
                "docker-volume mounts are not supported for this mount type",
            )
            .with_context("mount_type", mount.type_name()));
        }
    };
    options.extend(
        driver_options
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    Ok(Some(DockerVolumeDriverConfig {
        driver: driver.clone(),
        options,
        read_only: mount.is_read_only(),
    }))
}

/// The remote's section name: the one the pattern pins, or one derived per session.
///
/// Derived names keep two mounts in one session from sharing a mutable configuration section.
///
/// # Errors
///
/// Returns [`ErrorCode::MountConfigInvalid`] when a name has to be derived and there is no kind to
/// derive it from.
pub fn resolve_remote_name(
    options: &RcloneOptions,
    session_id_hex: &str,
    remote_kind: &str,
    mount_type: &str,
) -> SandboxResult<String> {
    if let Some(name) = options
        .remote_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        return Ok(name.to_owned());
    }
    if remote_kind.is_empty() {
        return Err(
            SandboxError::mount_config("rclone mount requires remote_kind")
                .with_context("type", mount_type),
        );
    }
    Ok(format!("sandbox_{remote_kind}_{session_id_hex}"))
}

/// Reads the rclone configuration file a pattern names, checking it has the remote's section.
///
/// A relative path is measured from the workspace root, not from the process orchestrating the
/// session.
async fn read_rclone_config_text(
    options: &RcloneOptions,
    session: &dyn SandboxSession,
    remote_name: &str,
    mount_type: &str,
) -> SandboxResult<String> {
    let Some(config_file_path) = &options.config_file_path else {
        return Err(
            SandboxError::mount_config("rclone config_file_path is not set")
                .with_context("type", mount_type),
        );
    };
    // A path, as the reference's `config_file_path: Path` field holds it: a backslash is part of
    // the name. Only the manifest root is read as text.
    let declared = PosixPath::new(config_file_path.as_str());
    let path = if declared.is_absolute() {
        declared
    } else {
        PosixPath::coerce(&session.state().manifest().root).join(declared.as_str())
    };
    let bytes = match session.read(SessionPath::Posix(&path), None).await {
        Ok(bytes) => bytes,
        Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => return Err(error),
        Err(error) => {
            return Err(
                SandboxError::mount_config("failed to read rclone config file")
                    .with_context("type", mount_type)
                    .with_context("path", path.as_str())
                    .with_sandbox_cause(error),
            );
        }
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if text.trim().is_empty() {
        return Err(SandboxError::mount_config("rclone config file is empty")
            .with_context("type", mount_type)
            .with_context("path", path.as_str()));
    }
    if section_header(remote_name).find(&text).is_none() {
        return Err(
            SandboxError::mount_config("rclone config missing required remote section")
                .with_context("type", mount_type)
                .with_context("path", path.as_str())
                .with_context("remote_name", remote_name),
        );
    }
    Ok(text)
}

/// Appends the lines a provider needs to its section of an existing configuration.
///
/// The existing section keeps what it had; the provider's lines go after it, header excluded, and
/// everything around the section is left as it was. Duplicated keys are left for rclone to resolve,
/// last one winning, as the reference leaves them.
fn supplement_rclone_config_text(
    config_text: &str,
    remote_name: &str,
    required_lines: &[String],
    mount_type: &str,
) -> SandboxResult<String> {
    let Some(header) = section_header(remote_name).find(config_text) else {
        return Err(
            SandboxError::mount_config("rclone config missing required remote section")
                .with_context("type", mount_type)
                .with_context("remote_name", remote_name),
        );
    };
    let body_end = ANY_SECTION
        .find(&config_text[header.end()..])
        .map_or(config_text.len(), |next| header.end() + next.start());

    let before = &config_text[..header.start()];
    let body = config_text[header.start()..body_end].trim_end_matches('\n');
    let after = &config_text[body_end..];
    let supplement = required_lines.get(1..).unwrap_or_default().join("\n");
    Ok(format!("{before}{body}\n{supplement}\n{after}"))
}

/// Matches a line holding only the remote's section header, as the reference's pattern does.
fn section_header(remote_name: &str) -> regex::Regex {
    regex::Regex::new(&format!(r"(?m)^\s*\[{}\]\s*$", regex::escape(remote_name)))
        .unwrap_or_else(|_| unreachable!("an escaped name always compiles"))
}

/// A bucket or container with an optional object prefix after it.
fn join_remote_path(root: &str, prefix: Option<&str>) -> String {
    match prefix {
        None => root.to_owned(),
        Some(prefix) => format!("{root}/{}", prefix.trim_start_matches('/')),
    }
}

/// The provider's value when non-empty, otherwise the pattern's default.
///
/// The reference writes `provider or default`, so an empty string falls through too.
fn either(value: Option<&String>, default: Option<&String>) -> Option<String> {
    value.filter(|value| !value.is_empty()).or(default).cloned()
}

/// Whether a value is present and non-empty, which is what the reference's truthiness asks.
fn is_filled(value: Option<&String>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

// --- per provider --------------------------------------------------------------------------------

fn s3_rclone_lines(s3: &S3Mount, name: &str) -> Vec<String> {
    let mut lines = vec![
        format!("[{name}]"),
        "type = s3".to_owned(),
        format!("provider = {}", s3.s3_provider),
    ];
    if let Some(endpoint) = &s3.endpoint_url {
        lines.push(format!("endpoint = {endpoint}"));
    }
    if let Some(region) = &s3.region {
        lines.push(format!("region = {region}"));
    }
    lines.push("env_auth = false".to_owned());
    if is_filled(s3.access_key_id.as_ref()) && is_filled(s3.secret_access_key.as_ref()) {
        lines.push(format!(
            "access_key_id = {}",
            fill(s3.access_key_id.as_ref())
        ));
        lines.push(format!(
            "secret_access_key = {}",
            fill(s3.secret_access_key.as_ref())
        ));
        if let Some(token) = s3.session_token.as_ref().filter(|token| !token.is_empty()) {
            lines.push(format!("session_token = {token}"));
        }
    }
    lines
}

fn s3_driver_options(s3: &S3Mount, driver: &str) -> BTreeMap<String, String> {
    let mut options = BTreeMap::new();
    let mut put = |key: &str, value: &Option<String>| {
        if let Some(value) = value {
            options.insert(key.to_owned(), value.clone());
        }
    };
    if driver == "rclone" {
        put("type", &Some("s3".to_owned()));
        put("s3-provider", &Some(s3.s3_provider.clone()));
        put(
            "path",
            &Some(join_remote_path(&s3.bucket, s3.prefix.as_deref())),
        );
        put("s3-access-key-id", &s3.access_key_id);
        put("s3-secret-access-key", &s3.secret_access_key);
        put("s3-session-token", &s3.session_token);
        put("s3-endpoint", &s3.endpoint_url);
        put("s3-region", &s3.region);
    } else {
        put("bucket", &Some(s3.bucket.clone()));
        put("access_key_id", &s3.access_key_id);
        put("secret_access_key", &s3.secret_access_key);
        put("session_token", &s3.session_token);
        put("endpoint_url", &s3.endpoint_url);
        put("region", &s3.region);
        put("prefix", &s3.prefix);
    }
    options
}

/// Whether a GCS mount carries an HMAC pair, which selects rclone's S3 backend.
const fn gcs_uses_hmac(gcs: &GcsMount) -> bool {
    gcs.access_id.is_some() && gcs.secret_access_key.is_some()
}

fn gcs_rclone_lines(gcs: &GcsMount, name: &str) -> Vec<String> {
    let mut lines = vec![
        format!("[{name}]"),
        "type = google cloud storage".to_owned(),
    ];
    let service_account_file = is_filled(gcs.service_account_file.as_ref());
    let service_account_credentials = is_filled(gcs.service_account_credentials.as_ref());
    let access_token = is_filled(gcs.access_token.as_ref());
    if service_account_file {
        lines.push(format!(
            "service_account_file = {}",
            fill(gcs.service_account_file.as_ref())
        ));
    }
    if service_account_credentials {
        lines.push(format!(
            "service_account_credentials = {}",
            fill(gcs.service_account_credentials.as_ref())
        ));
    }
    if access_token {
        lines.push(format!(
            "access_token = {}",
            fill(gcs.access_token.as_ref())
        ));
    }
    if !(service_account_file || service_account_credentials || access_token) {
        lines.push("anonymous = true".to_owned());
    }
    lines.push("env_auth = false".to_owned());
    lines
}

fn gcs_hmac_rclone_lines(gcs: &GcsMount, name: &str) -> Vec<String> {
    let mut lines = vec![
        format!("[{name}]"),
        "type = s3".to_owned(),
        "provider = GCS".to_owned(),
        "env_auth = false".to_owned(),
        format!("access_key_id = {}", fill(gcs.access_id.as_ref())),
        format!(
            "secret_access_key = {}",
            fill(gcs.secret_access_key.as_ref())
        ),
        format!("endpoint = {}", gcs_endpoint(gcs)),
    ];
    if let Some(region) = gcs.region.as_ref().filter(|region| !region.is_empty()) {
        lines.push(format!("region = {region}"));
    }
    lines
}

fn gcs_endpoint(gcs: &GcsMount) -> String {
    either(gcs.endpoint_url.as_ref(), None).unwrap_or_else(|| GCS_ENDPOINT.to_owned())
}

fn gcs_driver_options(gcs: &GcsMount, driver: &str) -> BTreeMap<String, String> {
    let mut options = BTreeMap::new();
    let mut put = |key: &str, value: &Option<String>| {
        if let Some(value) = value {
            options.insert(key.to_owned(), value.clone());
        }
    };
    let path = Some(join_remote_path(&gcs.bucket, gcs.prefix.as_deref()));
    if driver == "rclone" && gcs_uses_hmac(gcs) {
        put("type", &Some("s3".to_owned()));
        put("path", &path);
        put("s3-provider", &Some("GCS".to_owned()));
        put("s3-access-key-id", &gcs.access_id);
        put("s3-secret-access-key", &gcs.secret_access_key);
        put("s3-endpoint", &Some(gcs_endpoint(gcs)));
        put("s3-region", &gcs.region);
    } else if driver == "rclone" {
        put("type", &Some("google cloud storage".to_owned()));
        put("path", &path);
        put("gcs-service-account-file", &gcs.service_account_file);
        put(
            "gcs-service-account-credentials",
            &gcs.service_account_credentials,
        );
        put("gcs-access-token", &gcs.access_token);
    } else {
        put("bucket", &Some(gcs.bucket.clone()));
        put("endpoint_url", &Some(gcs_endpoint(gcs)));
        put("access_key_id", &gcs.access_id);
        put("secret_access_key", &gcs.secret_access_key);
        put("region", &gcs.region);
        put("prefix", &gcs.prefix);
    }
    options
}

fn azure_rclone_lines(azure: &AzureBlobMount, name: &str) -> Vec<String> {
    let mut lines = vec![
        format!("[{name}]"),
        "type = azureblob".to_owned(),
        format!("account = {}", azure.account),
    ];
    if let Some(endpoint) = azure.endpoint.as_ref().filter(|value| !value.is_empty()) {
        lines.push(format!("endpoint = {endpoint}"));
    }
    if let Some(key) = azure.account_key.as_ref().filter(|value| !value.is_empty()) {
        lines.push(format!("key = {key}"));
    } else if let Some(client) = azure
        .identity_client_id
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        lines.push("use_msi = true".to_owned());
        lines.push(format!("msi_client_id = {client}"));
    } else {
        lines.push("use_msi = false".to_owned());
    }
    lines
}

fn azure_driver_options(azure: &AzureBlobMount) -> BTreeMap<String, String> {
    let mut options = BTreeMap::from([
        ("type".to_owned(), "azureblob".to_owned()),
        ("path".to_owned(), azure.container.clone()),
        ("azureblob-account".to_owned(), azure.account.clone()),
    ]);
    for (key, value) in [
        ("azureblob-endpoint", &azure.endpoint),
        ("azureblob-msi-client-id", &azure.identity_client_id),
        ("azureblob-key", &azure.account_key),
    ] {
        if let Some(value) = value {
            options.insert(key.to_owned(), value.clone());
        }
    }
    options
}

fn box_remote_path(folder: &BoxMount) -> String {
    folder
        .path
        .as_deref()
        .map(|path| path.trim_start_matches('/').to_owned())
        .unwrap_or_default()
}

/// The Box fields in the order both renderings write them, with the rclone key for each.
fn box_fields(folder: &BoxMount) -> [(&'static str, &Option<String>); 9] {
    [
        ("client_id", &folder.client_id),
        ("client_secret", &folder.client_secret),
        ("access_token", &folder.access_token),
        ("token", &folder.token),
        ("box_config_file", &folder.box_config_file),
        ("config_credentials", &folder.config_credentials),
        ("root_folder_id", &folder.root_folder_id),
        ("impersonate", &folder.impersonate),
        ("owned_by", &folder.owned_by),
    ]
}

fn box_rclone_lines(folder: &BoxMount, name: &str) -> Vec<String> {
    let mut lines = vec![format!("[{name}]"), "type = box".to_owned()];
    for (index, (key, value)) in box_fields(folder).into_iter().enumerate() {
        // The subject type sits between the configuration fields and the account fields.
        if index == 6 && folder.box_sub_type != BoxSubType::User {
            lines.push(format!("box_sub_type = {}", folder.box_sub_type.as_str()));
        }
        if let Some(value) = value {
            lines.push(format!("{key} = {value}"));
        }
    }
    lines
}

fn box_driver_options(folder: &BoxMount) -> BTreeMap<String, String> {
    let mut options = BTreeMap::from([
        ("type".to_owned(), "box".to_owned()),
        ("path".to_owned(), box_remote_path(folder)),
    ]);
    for (key, value) in box_fields(folder) {
        if let Some(value) = value {
            // `box_config_file` becomes `box-box-config-file`: every key gains the `box-` prefix.
            options.insert(format!("box-{}", key.replace('_', "-")), value.clone());
        }
    }
    if folder.box_sub_type != BoxSubType::User {
        options.insert(
            "box-box-sub-type".to_owned(),
            folder.box_sub_type.as_str().to_owned(),
        );
    }
    options
}

/// Whether an R2 mount has one half of its credential pair without the other.
const fn credential_pair_is_split(r2: &R2Mount) -> bool {
    r2.access_key_id.is_some() != r2.secret_access_key.is_some()
}

fn r2_endpoint(r2: &R2Mount) -> String {
    either(r2.custom_domain.as_ref(), None)
        .unwrap_or_else(|| format!("https://{}.r2.cloudflarestorage.com", r2.account_id))
}

fn r2_rclone_lines(r2: &R2Mount, name: &str) -> Vec<String> {
    let mut lines = vec![
        format!("[{name}]"),
        "type = s3".to_owned(),
        "provider = Cloudflare".to_owned(),
        format!("endpoint = {}", r2_endpoint(r2)),
        "acl = private".to_owned(),
        "env_auth = false".to_owned(),
    ];
    if is_filled(r2.access_key_id.as_ref()) && is_filled(r2.secret_access_key.as_ref()) {
        lines.push(format!(
            "access_key_id = {}",
            fill(r2.access_key_id.as_ref())
        ));
        lines.push(format!(
            "secret_access_key = {}",
            fill(r2.secret_access_key.as_ref())
        ));
    }
    lines
}

fn r2_driver_options(r2: &R2Mount) -> BTreeMap<String, String> {
    let mut options = BTreeMap::from([
        ("type".to_owned(), "s3".to_owned()),
        ("path".to_owned(), r2.bucket.clone()),
        ("s3-provider".to_owned(), "Cloudflare".to_owned()),
        ("s3-endpoint".to_owned(), r2_endpoint(r2)),
    ]);
    for (key, value) in [
        ("s3-access-key-id", &r2.access_key_id),
        ("s3-secret-access-key", &r2.secret_access_key),
    ] {
        if let Some(value) = value {
            options.insert(key.to_owned(), value.clone());
        }
    }
    options
}

fn s3_files_config(
    files: &S3FilesMount,
    defaults: &ra_core::sandbox::S3FilesOptions,
    mount_type: &str,
    read_only: bool,
) -> S3FilesMountConfig {
    let mut extra_options = defaults.extra_options.clone();
    extra_options.extend(
        files
            .extra_options
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    S3FilesMountConfig {
        file_system_id: files.file_system_id.clone(),
        subpath: files.subpath.clone(),
        mount_target_ip: either(
            files.mount_target_ip.as_ref(),
            defaults.mount_target_ip.as_ref(),
        ),
        access_point: either(files.access_point.as_ref(), defaults.access_point.as_ref()),
        region: either(files.region.as_ref(), defaults.region.as_ref()),
        extra_options,
        mount_type: mount_type.to_owned(),
        read_only,
    }
}

/// A value already checked to be present, rendered as the reference renders it.
fn fill(value: Option<&String>) -> &str {
    value.map_or("", String::as_str)
}
