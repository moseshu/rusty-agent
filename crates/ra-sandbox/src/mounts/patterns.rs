//! The commands each in-container pattern runs, and the files it writes first.
//!
//! Four tools, each checked for before anything else happens: `mount-s3`, `blobfuse2`, rclone and
//! `mount.s3files`. Whatever a tool needs on disk — rclone's configuration, blobfuse's YAML,
//! Mountpoint's credentials — is written under a per-session directory inside the workspace, readable
//! by its owner only, and excluded from every later snapshot.
//!
//! # Credentials stay off the command line
//!
//! `mount-s3` takes its keys from the environment, so they go into an owner-only file the command
//! sources rather than into the command itself; an endpoint URL carrying credentials is passed the
//! same way. A failed Mountpoint or blobfuse command is reported with every such value replaced by
//! `REDACTED`, and marked redacted besides, so the boundary it leaves through replaces it entirely.
//!
//! # Detaching is best effort
//!
//! Unmounting ignores the command's exit status, as the reference does: a path that is already
//! unmounted is the outcome being asked for.

use ra_core::sandbox::{
    ExecRequest, ExecResult, FuseCacheType, FuseOptions, MountPattern, PosixPath, RcloneMode,
    RcloneOptions, SandboxError, SandboxResult, SandboxSession, SessionPath, ShellInvocation,
    url_carries_inline_authority,
};
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use super::config::{
    FuseMountConfig, MountPatternConfig, MountpointMountConfig, RcloneMountConfig,
    S3FilesMountConfig,
};
use super::protect;
use crate::shell::quote;

/// The environment variable a protected Mountpoint endpoint is passed through.
const ENDPOINT_VARIABLE: &str = "OPENAI_AGENTS_MOUNT_ENDPOINT_URL";

/// Attaches a mount at `path` with `pattern`, using the configuration its provider built.
///
/// `path` is where the mount appears, already resolved against the workspace root.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::MountMissingTool`] when the pattern's tool is not in the
/// sandbox, [`ra_core::sandbox::ErrorCode::MountFailed`] when its command fails, and
/// [`ra_core::sandbox::ErrorCode::MountConfigInvalid`] for a configuration built for another
/// pattern or one the pattern cannot use. A failure marked redacted, or one raised in a session
/// whose manifest carries mount authority, is replaced as
/// [`ra_core::sandbox::replace_protected_mount_error`] describes.
pub async fn apply_pattern(
    pattern: &MountPattern,
    session: &dyn SandboxSession,
    path: &PosixPath,
    config: &MountPatternConfig,
) -> SandboxResult<()> {
    let result = match (pattern, config) {
        (MountPattern::Fuse(options), MountPatternConfig::Fuse(config)) => {
            apply_fuse(options, session, path, config).await
        }
        (MountPattern::Mountpoint(_), MountPatternConfig::Mountpoint(config)) => {
            apply_mountpoint(session, path, config).await
        }
        (MountPattern::Rclone(options), MountPatternConfig::Rclone(config)) => {
            apply_rclone(options, session, path, config).await
        }
        (MountPattern::S3Files(_), MountPatternConfig::S3Files(config)) => {
            apply_s3_files(session, path, config).await
        }
        _ => Err(incompatible(pattern, config)),
    };
    protect(result, None, session)
}

/// Detaches a mount [`apply_pattern`] attached at `path`.
///
/// # Errors
///
/// As [`apply_pattern`], for a configuration built for another pattern, or the session's failure
/// to run the unmount command at all; the command's own exit status is not checked.
pub async fn unapply_pattern(
    pattern: &MountPattern,
    session: &dyn SandboxSession,
    path: &PosixPath,
    config: &MountPatternConfig,
) -> SandboxResult<()> {
    let result = match (pattern, config) {
        (MountPattern::Fuse(_), MountPatternConfig::Fuse(_))
        | (MountPattern::Mountpoint(_), MountPatternConfig::Mountpoint(_)) => {
            fuse_unmount(session, path).await
        }
        (MountPattern::Rclone(options), MountPatternConfig::Rclone(config)) => {
            unapply_rclone(options, session, path, config).await
        }
        (MountPattern::S3Files(_), MountPatternConfig::S3Files(_)) => {
            sh(session, format!("umount {} || true", quote(path.as_str())))
                .await
                .map(drop)
        }
        _ => Err(incompatible(pattern, config)),
    };
    protect(result, None, session)
}

fn incompatible(pattern: &MountPattern, config: &MountPatternConfig) -> SandboxError {
    let expected = match pattern {
        MountPattern::Fuse(_) => "FuseMountConfig",
        MountPattern::Mountpoint(_) => "MountpointMountConfig",
        MountPattern::Rclone(_) => "RcloneMountConfig",
        _ => "S3FilesMountConfig",
    };
    let actual = match config {
        MountPatternConfig::Fuse(_) => "FuseMountConfig",
        MountPatternConfig::Mountpoint(_) => "MountpointMountConfig",
        MountPatternConfig::Rclone(_) => "RcloneMountConfig",
        MountPatternConfig::S3Files(_) => "S3FilesMountConfig",
    };
    SandboxError::mount_config("mount pattern received incompatible runtime config")
        .with_context("expected", expected)
        .with_context("actual", actual)
}

// --- running things ------------------------------------------------------------------------------

/// Runs an argument vector with no shell in between.
async fn run(session: &dyn SandboxSession, command: Vec<String>) -> SandboxResult<ExecResult> {
    session
        .exec(ExecRequest::new(command).with_shell(ShellInvocation::None))
        .await
}

/// Lets the backend choose its default shell for a script.
async fn default_shell(session: &dyn SandboxSession, script: String) -> SandboxResult<ExecResult> {
    session.exec(ExecRequest::new([script])).await
}

/// Runs a script through `sh -lc`, as the reference's shell invocations do.
async fn sh(session: &dyn SandboxSession, script: String) -> SandboxResult<ExecResult> {
    run(session, vec!["sh".to_owned(), "-lc".to_owned(), script]).await
}

/// Where a workspace path lands for this session's commands.
async fn normalize(session: &dyn SandboxSession, path: &PosixPath) -> SandboxResult<PosixPath> {
    session
        .validate_path_access(SessionPath::Posix(path), false)
        .await
}

/// Creates a directory, and its parents.
async fn mkdir(session: &dyn SandboxSession, path: &PosixPath) -> SandboxResult<()> {
    let target = if path.is_absolute() {
        path.clone()
    } else {
        normalize(session, path).await?
    };
    session.mkdir(SessionPath::Posix(&target), true, None).await
}

/// Writes generated configuration or credentials readable by their owner only.
async fn write_sensitive(
    session: &dyn SandboxSession,
    path: &PosixPath,
    payload: Vec<u8>,
) -> SandboxResult<()> {
    let target = normalize(session, path).await?;
    session
        .write(SessionPath::Posix(&target), payload, None)
        .await?;
    let command = vec![
        "chmod".to_owned(),
        "0600".to_owned(),
        target.as_str().to_owned(),
    ];
    let result = run(session, command.clone()).await?;
    if result.ok() {
        Ok(())
    } else {
        Err(SandboxError::exec_nonzero(result, command))
    }
}

/// The session's id as the hex string generated paths are named with.
fn session_hex(session: &dyn SandboxSession) -> String {
    session.state().session_id().simple().to_string()
}

/// `fusermount3 -u`, falling back to `umount`.
async fn fuse_unmount(session: &dyn SandboxSession, path: &PosixPath) -> SandboxResult<()> {
    let quoted = quote(path.as_str());
    sh(
        session,
        format!("fusermount3 -u {quoted} || umount {quoted}"),
    )
    .await
    .map(drop)
}

/// Replaces every sensitive value in `text`, bare and shell-quoted, with `REDACTED`.
fn redact_sensitive_values(text: &str, sensitive: &[String]) -> String {
    let mut redacted = text.to_owned();
    for value in sensitive.iter().filter(|value| !value.is_empty()) {
        redacted = redacted.replace(value.as_str(), "REDACTED");
        let quoted = quote(value);
        if quoted != *value {
            redacted = redacted.replace(&quoted, "REDACTED");
        }
    }
    redacted
}

/// The values in a URL that carry authority: the URL itself, its user and password, its query and
/// each query value, each also percent-decoded.
fn inline_url_authority_values(url: &str) -> Vec<String> {
    if !url_carries_inline_authority(url) {
        return Vec::new();
    }
    let mut values = vec![url.to_owned()];
    let without_fragment = url.split('#').next().unwrap_or_default();
    let (before_query, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, ""), |(head, query)| (head, query));
    if let Some((_, after_scheme)) = before_query.split_once("//") {
        let netloc = after_scheme.split('/').next().unwrap_or_default();
        if let Some((userinfo, _)) = netloc.rsplit_once('@') {
            let (user, password) = userinfo
                .split_once(':')
                .map_or((userinfo, None), |(user, password)| (user, Some(password)));
            for value in std::iter::once(user).chain(password) {
                push_both(&mut values, value);
            }
        }
    }
    push_both(&mut values, query);
    for pair in query.split('&') {
        let value = pair.split_once('=').map_or("", |(_, value)| value);
        push_both(&mut values, &percent_decode(&value.replace('+', " ")));
    }
    let mut seen = std::collections::BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
    values
}

fn push_both(values: &mut Vec<String>, value: &str) {
    if !value.is_empty() {
        values.push(value.to_owned());
        values.push(percent_decode(value));
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = value.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Reads a file the command may have written, or nothing when it cannot be read.
async fn read_text_if_present(session: &dyn SandboxSession, path: &PosixPath) -> String {
    let Ok(target) = normalize(session, path).await else {
        return String::new();
    };
    session
        .read(SessionPath::Posix(&target), None)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

// --- Mountpoint ----------------------------------------------------------------------------------

async fn apply_mountpoint(
    session: &dyn SandboxSession,
    path: &PosixPath,
    config: &MountpointMountConfig,
) -> SandboxResult<()> {
    let bucket = config.bucket.as_str();
    let protected_endpoint = config
        .endpoint_url
        .as_deref()
        .filter(|url| url_carries_inline_authority(url));
    let endpoint_reference = format!("${{{ENDPOINT_VARIABLE}}}");

    if !default_shell(session, "command -v mount-s3 >/dev/null 2>&1".to_owned())
        .await?
        .ok()
    {
        return Err(SandboxError::mount_tool_missing("mount-s3").with_context("bucket", bucket));
    }
    mkdir(session, path).await?;

    let command = mountpoint_command(
        config,
        path,
        protected_endpoint.map(|_| &*endpoint_reference),
    );
    let environment = mountpoint_environment(config, protected_endpoint);

    let mut script = command
        .iter()
        .map(|part| {
            if *part == endpoint_reference {
                format!("\"{part}\"")
            } else {
                quote(part)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut sensitive: Vec<String> = environment.iter().map(|(_, value)| value.clone()).collect();
    sensitive.extend(
        protected_endpoint
            .map(inline_url_authority_values)
            .unwrap_or_default(),
    );

    let mut stderr_path = None;
    if !environment.is_empty() {
        let digest = Sha256::digest(format!("{bucket}\0{}", path.as_str()).as_bytes());
        let command_hash = digest.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
        let command_hash = &command_hash[..16];
        let config_dir =
            PosixPath::new(format!(".sandbox-mountpoint-env/{}", session_hex(session)));
        let env_path = config_dir.join(&format!("{command_hash}.env"));
        let stdout = config_dir.join(&format!("{command_hash}.stdout"));
        let stderr = config_dir.join(&format!("{command_hash}.stderr"));

        mkdir(session, &config_dir).await?;
        session.register_persist_workspace_skip_path(SessionPath::Posix(&config_dir))?;
        write_sensitive(session, &env_path, render_shell_exports(&environment)).await?;

        let env_target = normalize(session, &env_path).await?;
        let stdout_target = normalize(session, &stdout).await?;
        let stderr_target = normalize(session, &stderr).await?;
        script = format!(
            ". {} && exec {script} >{} 2>{}",
            quote(env_target.as_str()),
            quote(stdout_target.as_str()),
            quote(stderr_target.as_str())
        );
        stderr_path = Some(stderr);
    }

    let result = sh(session, script.clone()).await?;
    if result.ok() {
        return Ok(());
    }
    let mut stderr = String::from_utf8_lossy(&result.stderr).into_owned();
    if let Some(stderr_path) = &stderr_path {
        stderr.push_str(&read_text_if_present(session, stderr_path).await);
    }
    Err(SandboxError::mount_command(
        &redact_sensitive_values(&script, &sensitive),
        &redact_sensitive_values(&stderr, &sensitive),
    )
    .with_context("bucket", bucket)
    .with_data_redacted())
}

/// The `mount-s3` argument vector. A protected endpoint is named by `endpoint_reference`.
fn mountpoint_command(
    config: &MountpointMountConfig,
    path: &PosixPath,
    endpoint_reference: Option<&str>,
) -> Vec<String> {
    let mut command = vec!["mount-s3".to_owned()];
    if !has_credentials(config) {
        command.push("--no-sign-request".to_owned());
    }
    if config.read_only {
        command.push("--read-only".to_owned());
    } else if matches!(config.mount_type.as_str(), "s3_mount" | "gcs_mount") {
        command.extend(["--allow-overwrite".to_owned(), "--allow-delete".to_owned()]);
    }
    if let Some(region) = config.region.as_ref().filter(|value| !value.is_empty()) {
        command.extend(["--region".to_owned(), region.clone()]);
    }
    if let Some(endpoint) = config
        .endpoint_url
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        command.push("--endpoint-url".to_owned());
        command.push(endpoint_reference.map_or_else(|| endpoint.clone(), str::to_owned));
    }
    if config.mount_type == "gcs_mount" {
        // The GCS XML API refuses the upload checksums mount-s3 sends by default.
        command.extend(["--upload-checksums".to_owned(), "off".to_owned()]);
    }
    if let Some(prefix) = config.prefix.as_ref().filter(|value| !value.is_empty()) {
        command.extend(["--prefix".to_owned(), prefix.clone()]);
    }
    command.extend([config.bucket.clone(), path.as_str().to_owned()]);
    command
}

/// What `mount-s3` reads from its environment: the credentials, and a protected endpoint.
fn mountpoint_environment(
    config: &MountpointMountConfig,
    protected_endpoint: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut environment = Vec::new();
    if has_credentials(config) {
        environment.push(("AWS_ACCESS_KEY_ID", fill(config.access_key_id.as_ref())));
        environment.push((
            "AWS_SECRET_ACCESS_KEY",
            fill(config.secret_access_key.as_ref()),
        ));
        if let Some(token) = config
            .session_token
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            environment.push(("AWS_SESSION_TOKEN", token.clone()));
        }
    }
    if let Some(endpoint) = protected_endpoint {
        environment.push((ENDPOINT_VARIABLE, endpoint.to_owned()));
    }
    environment
}

/// Whether both halves of the key pair are present and non-empty.
fn has_credentials(config: &MountpointMountConfig) -> bool {
    filled(config.access_key_id.as_ref()) && filled(config.secret_access_key.as_ref())
}

/// `export NAME=value` lines, each value shell-quoted.
fn render_shell_exports(environment: &[(&str, String)]) -> Vec<u8> {
    let lines: Vec<String> = environment
        .iter()
        .map(|(name, value)| format!("export {name}={}", quote(value)))
        .collect();
    (lines.join("\n") + "\n").into_bytes()
}

// --- blobfuse ------------------------------------------------------------------------------------

async fn apply_fuse(
    options: &FuseOptions,
    session: &dyn SandboxSession,
    path: &PosixPath,
    config: &FuseMountConfig,
) -> SandboxResult<()> {
    let declared_cache = options.checked_cache_path()?;
    let (account, container) = (config.account.as_str(), config.container.as_str());

    if !default_shell(session, "command -v blobfuse2 >/dev/null 2>&1".to_owned())
        .await?
        .ok()
    {
        return Err(SandboxError::mount_tool_missing("blobfuse2")
            .with_context("account", account)
            .with_context("container", container));
    }

    let session_id = session_hex(session);
    // Scratch state stays inside the workspace, so the session's own workspace-scoped operations
    // can create and write it.
    let cache_dir = declared_cache.unwrap_or_else(|| {
        PosixPath::coerce(&format!(
            ".sandbox-blobfuse-cache/{session_id}/{account}/{container}"
        ))
    });
    let config_dir = PosixPath::coerce(&format!(".sandbox-blobfuse-config/{session_id}"));
    let config_path = config_dir.join(&format!(
        "{}.yaml",
        format!("{account}_{container}").replace('/', "_")
    ));
    let command_mount_path = normalize(session, path).await?;
    let command_cache_dir = normalize(session, &cache_dir).await?;
    if command_cache_dir.is_under(&command_mount_path) {
        return Err(SandboxError::mount_config(
            "blobfuse cache_path must be outside the mount path",
        )
        .with_context("mount_path", command_mount_path.as_str())
        .with_context("cache_path", command_cache_dir.as_str()));
    }

    mkdir(session, path).await?;
    mkdir(session, &cache_dir).await?;
    mkdir(session, &config_dir).await?;
    session.register_persist_workspace_skip_path(SessionPath::Posix(&cache_dir))?;
    session.register_persist_workspace_skip_path(SessionPath::Posix(&config_dir))?;
    let command_config_path = normalize(session, &config_path).await?;

    let yaml = blobfuse_yaml(options, config, &command_cache_dir);
    write_sensitive(session, &config_path, yaml.into_bytes()).await?;

    let mut command = vec!["blobfuse2".to_owned(), "mount".to_owned()];
    if config.read_only {
        command.push("--read-only".to_owned());
    }
    command.extend([
        "--config-file".to_owned(),
        command_config_path.as_str().to_owned(),
        path.as_str().to_owned(),
    ]);
    let result = run(session, command.clone()).await?;
    if result.ok() {
        return Ok(());
    }
    Err(
        SandboxError::mount_command(&command.join(" "), &String::from_utf8_lossy(&result.stderr))
            .with_context("account", account)
            .with_context("container", container)
            .with_data_redacted(),
    )
}

/// The blobfuse configuration file, line for line as the reference writes it.
fn blobfuse_yaml(options: &FuseOptions, config: &FuseMountConfig, cache_dir: &PosixPath) -> String {
    let block_cache = options.cache_type == FuseCacheType::BlockCache;
    // Zero has no portable meaning across the two cache types, so zero and unset both take the
    // type's default.
    let cache_size_mb = options
        .cache_size_mb
        .filter(|size| *size != 0)
        .unwrap_or(if block_cache { 50_000 } else { 4_096 });
    let file_cache_max_size_mb = options
        .file_cache_max_size_mb
        .filter(|size| *size != 0)
        .unwrap_or(cache_size_mb);
    let endpoint = config
        .endpoint
        .clone()
        .filter(|endpoint| !endpoint.is_empty())
        .unwrap_or_else(|| format!("https://{}.blob.core.windows.net", config.account));

    let mut lines: Vec<String> = Vec::new();
    if options.allow_other {
        lines.extend(["allow-other: true".to_owned(), String::new()]);
    }
    lines.extend([
        "logging:".to_owned(),
        format!("  type: {}", options.log_type),
        format!("  level: {}", options.log_level),
        String::new(),
        "components:".to_owned(),
        "  - libfuse".to_owned(),
        format!("  - {}", options.cache_type.as_str()),
        "  - attr_cache".to_owned(),
        "  - azstorage".to_owned(),
        String::new(),
    ]);
    let mut libfuse = Vec::new();
    if let Some(timeout) = options.entry_cache_timeout_sec {
        libfuse.push(format!("  entry-expiration-sec: {timeout}"));
    }
    if let Some(timeout) = options.negative_entry_cache_timeout_sec {
        libfuse.push(format!("  negative-entry-expiration-sec: {timeout}"));
    }
    if !libfuse.is_empty() {
        lines.push("libfuse:".to_owned());
        lines.extend(libfuse);
        lines.push(String::new());
    }
    if block_cache {
        lines.extend([
            "block_cache:".to_owned(),
            format!("  block-size-mb: {}", options.block_cache_block_size_mb),
            format!("  mem-size-mb: {cache_size_mb}"),
            format!("  path: {}", cache_dir.as_str()),
            format!("  disk-size-mb: {cache_size_mb}"),
            format!(
                "  disk-timeout-sec: {}",
                options.block_cache_disk_timeout_sec
            ),
            String::new(),
        ]);
    } else {
        lines.extend([
            "file_cache:".to_owned(),
            format!("  path: {}", cache_dir.as_str()),
            format!("  timeout-sec: {}", options.file_cache_timeout_sec),
            format!("  max-size-mb: {file_cache_max_size_mb}"),
            String::new(),
        ]);
    }
    lines.extend([
        "attr_cache:".to_owned(),
        format!(
            "  timeout-sec: {}",
            options.attr_cache_timeout_sec.unwrap_or(7200)
        ),
        String::new(),
        "azstorage:".to_owned(),
        "  type: block".to_owned(),
        format!("  account-name: {}", config.account),
        format!("  container: {}", config.container),
        format!("  endpoint: {endpoint}"),
    ]);
    if let Some(key) = config.account_key.as_ref().filter(|key| !key.is_empty()) {
        lines.extend([
            "  auth-type: key".to_owned(),
            format!("  account-key: {key}"),
        ]);
    } else {
        lines.push("  mode: msi".to_owned());
    }
    if let Some(client) = config
        .identity_client_id
        .as_ref()
        .filter(|client| !client.is_empty())
    {
        lines.push(format!("  identity-client-id: {client}"));
    }
    lines.push(String::new());
    lines.join("\n")
}

// --- S3 Files ------------------------------------------------------------------------------------

async fn apply_s3_files(
    session: &dyn SandboxSession,
    path: &PosixPath,
    config: &S3FilesMountConfig,
) -> SandboxResult<()> {
    if !default_shell(
        session,
        "command -v mount.s3files >/dev/null 2>&1".to_owned(),
    )
    .await?
    .ok()
    {
        return Err(SandboxError::mount_tool_missing("mount.s3files")
            .with_context("file_system_id", config.file_system_id.as_str()));
    }
    mkdir(session, path).await?;

    let device = match config
        .subpath
        .as_ref()
        .filter(|subpath| !subpath.is_empty())
    {
        Some(subpath) => format!("{}:{subpath}", config.file_system_id),
        None => config.file_system_id.clone(),
    };
    // The helper's own options first, then the ones this pattern sets; a key given both ways keeps
    // its place and takes the pattern's value, as assigning into a dict does.
    let mut options: Vec<(String, Option<String>)> = config
        .extra_options
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut set = |key: &str, value: Option<String>| {
        if let Some(existing) = options.iter_mut().find(|(name, _)| name == key) {
            existing.1 = value;
        } else {
            options.push((key.to_owned(), value));
        }
    };
    if config.read_only {
        set("ro", None);
    }
    for (key, value) in [
        ("mounttargetip", &config.mount_target_ip),
        ("accesspoint", &config.access_point),
        ("region", &config.region),
    ] {
        if let Some(value) = value.as_ref().filter(|value| !value.is_empty()) {
            set(key, Some(value.clone()));
        }
    }

    let mut command = vec!["mount".to_owned(), "-t".to_owned(), "s3files".to_owned()];
    if !options.is_empty() {
        let rendered = options
            .iter()
            .map(|(key, value)| match value {
                Some(value) => format!("{key}={value}"),
                None => key.clone(),
            })
            .collect::<Vec<_>>()
            .join(",");
        command.extend(["-o".to_owned(), rendered]);
    }
    command.extend([device, path.as_str().to_owned()]);

    let result = run(session, command.clone()).await?;
    if result.ok() {
        return Ok(());
    }
    Err(SandboxError::mount_command(
        &command
            .iter()
            .map(|part| quote(part))
            .collect::<Vec<_>>()
            .join(" "),
        &String::from_utf8_lossy(&result.stderr),
    )
    .with_context("file_system_id", config.file_system_id.as_str()))
}

// --- rclone --------------------------------------------------------------------------------------

async fn apply_rclone(
    options: &RcloneOptions,
    session: &dyn SandboxSession,
    path: &PosixPath,
    config: &RcloneMountConfig,
) -> SandboxResult<()> {
    let mount_type = config.mount_type.as_str();
    if !sh(
        session,
        "command -v rclone >/dev/null 2>&1 || test -x /usr/local/bin/rclone".to_owned(),
    )
    .await?
    .ok()
    {
        return Err(SandboxError::mount_tool_missing("rclone").with_context("type", mount_type));
    }
    let Some(config_text) = &config.config_text else {
        return Err(
            SandboxError::mount_config("rclone mount requires config_text")
                .with_context("type", mount_type),
        );
    };

    // The generated configuration is always a file of its own, so extending a provider's section
    // never edits a configuration the workspace shares.
    let config_dir = PosixPath::coerce(&format!(".sandbox-rclone-config/{}", session_hex(session)));
    let config_path = config_dir.join(&format!("{}.conf", config.remote_name));
    mkdir(session, path).await?;
    mkdir(session, &config_dir).await?;
    session.register_persist_workspace_skip_path(SessionPath::Posix(&config_dir))?;
    write_sensitive(session, &config_path, config_text.clone().into_bytes()).await?;
    let command_config_path = normalize(session, &config_path).await?;

    if options.mode == RcloneMode::Nfs {
        let nfs_addr = options
            .nfs_addr
            .clone()
            .filter(|addr| !addr.is_empty())
            .unwrap_or_else(|| "127.0.0.1:2049".to_owned());
        start_rclone_server(options, session, config, &command_config_path, &nfs_addr).await?;
        start_rclone_client(
            options,
            session,
            path,
            config,
            &command_config_path,
            Some(&nfs_addr),
        )
        .await
    } else {
        start_rclone_client(options, session, path, config, &command_config_path, None).await
    }
}

async fn start_rclone_server(
    options: &RcloneOptions,
    session: &dyn SandboxSession,
    config: &RcloneMountConfig,
    config_path: &PosixPath,
    nfs_addr: &str,
) -> SandboxResult<()> {
    let check = sh(
        session,
        "/usr/local/bin/rclone serve nfs --help >/dev/null 2>&1 || rclone serve nfs --help \
         >/dev/null 2>&1"
            .to_owned(),
    )
    .await?;
    if !check.ok() {
        return Err(SandboxError::mount_tool_missing("rclone serve nfs")
            .with_context("type", config.mount_type.as_str()));
    }
    let mut command = vec![
        "rclone".to_owned(),
        "serve".to_owned(),
        "nfs".to_owned(),
        format!("{}:{}", config.remote_name, config.remote_path),
        "--addr".to_owned(),
        nfs_addr.to_owned(),
        "--config".to_owned(),
        config_path.as_str().to_owned(),
    ];
    if config.read_only {
        command.push("--read-only".to_owned());
    }
    command.extend(options.extra_args.iter().cloned());
    let joined = command
        .iter()
        .map(|part| quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    // In the background, so the client can wait for it to come up.
    let result = sh(session, format!("{joined} &")).await?;
    if result.ok() {
        return Ok(());
    }
    Err(
        SandboxError::mount_command(&command.join(" "), &String::from_utf8_lossy(&result.stderr))
            .with_context("type", config.mount_type.as_str()),
    )
}

async fn start_rclone_client(
    options: &RcloneOptions,
    session: &dyn SandboxSession,
    path: &PosixPath,
    config: &RcloneMountConfig,
    config_path: &PosixPath,
    nfs_addr: Option<&str>,
) -> SandboxResult<()> {
    let mount_type = config.mount_type.as_str();
    if options.mode != RcloneMode::Nfs {
        let mut command = vec![
            "rclone".to_owned(),
            "mount".to_owned(),
            format!("{}:{}", config.remote_name, config.remote_path),
            path.as_str().to_owned(),
        ];
        if config.read_only {
            command.push("--read-only".to_owned());
        }
        command.extend([
            "--config".to_owned(),
            config_path.as_str().to_owned(),
            "--daemon".to_owned(),
        ]);
        command.extend(options.extra_args.iter().cloned());
        let result = run(session, command.clone()).await?;
        if result.ok() {
            return Ok(());
        }
        return Err(SandboxError::mount_command(
            &command.join(" "),
            &String::from_utf8_lossy(&result.stderr),
        )
        .with_context("type", mount_type));
    }

    let Some(nfs_addr) = nfs_addr else {
        return Err(
            SandboxError::mount_config("nfs_addr required for rclone nfs client")
                .with_context("type", mount_type),
        );
    };
    if !sh(session, "grep -w nfs /proc/filesystems".to_owned())
        .await?
        .ok()
    {
        tracing::warn!(
            "NFS client support not detected; attempting mount anyway. If it fails, use rclone \
             fuse mode or run on a kernel with NFS support."
        );
    }
    let (host, port) = nfs_addr
        .rsplit_once(':')
        .map_or((nfs_addr, "2049"), |(host, port)| (host, port));
    let host = if matches!(host, "0.0.0.0" | "::") {
        "127.0.0.1"
    } else {
        host
    };
    let mount_options = options
        .nfs_mount_options
        .clone()
        .filter(|options| !options.is_empty())
        .unwrap_or_else(|| {
            vec![
                "vers=4.1".to_owned(),
                "tcp".to_owned(),
                format!("port={port}"),
                "soft".to_owned(),
                "timeo=50".to_owned(),
                "retrans=1".to_owned(),
            ]
        });
    let timeout_prefix = if sh(session, "command -v timeout >/dev/null 2>&1".to_owned())
        .await?
        .ok()
    {
        "timeout 10s "
    } else {
        ""
    };
    let script = [
        "for i in 1 2 3; do".to_owned(),
        format!("{timeout_prefix}mount"),
        "-v".to_owned(),
        "-t".to_owned(),
        "nfs".to_owned(),
        "-o".to_owned(),
        quote(&mount_options.join(",")),
        format!("{}:/", quote(host)),
        quote(path.as_str()),
        "&& exit 0; sleep 1; done; exit 1".to_owned(),
    ]
    .join(" ");
    let result = sh(session, script.clone()).await?;
    if result.ok() {
        return Ok(());
    }
    Err(SandboxError::mount_command(
        &format!("sh -lc {script}"),
        &String::from_utf8_lossy(&result.stderr),
    )
    .with_context("type", mount_type))
}

async fn unapply_rclone(
    options: &RcloneOptions,
    session: &dyn SandboxSession,
    path: &PosixPath,
    config: &RcloneMountConfig,
) -> SandboxResult<()> {
    let quoted = quote(path.as_str());
    if options.mode == RcloneMode::Nfs {
        sh(session, format!("umount {quoted} >/dev/null 2>&1 || true")).await?;
    } else {
        sh(
            session,
            format!("fusermount3 -u {quoted} || umount {quoted}"),
        )
        .await?;
    }
    sh(
        session,
        format!(
            "pkill -f -- 'rclone (mount|serve nfs) {}:' >/dev/null 2>&1 || true",
            config.remote_name
        ),
    )
    .await
    .map(drop)
}

// --- small helpers -------------------------------------------------------------------------------

fn filled(value: Option<&String>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn fill(value: Option<&String>) -> String {
    value.cloned().unwrap_or_default()
}
