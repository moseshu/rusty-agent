// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Filesystem-only native helper preparation and owned execution.
#![allow(
    unreachable_pub,
    reason = "Private implementation items are exposed only by the feature-gated test facade."
)]
#![cfg_attr(
    feature = "test-api",
    allow(missing_docs, reason = "Hidden source-test facade access.")
)]
#![allow(
    clippy::needless_pass_by_value,
    reason = "Preserve the source ownership and public argument contract."
)]
#![allow(
    clippy::similar_names,
    reason = "Keep native descriptor and protocol names aligned with the source."
)]
use super::ExecServerRuntimeOptions;
#[cfg(target_os = "linux")]
use super::LinuxSandboxPidNamespace;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::fs_helper::CODEX_FS_HELPER_ARG1;
use super::fs_helper::{FsHelperPayload, FsHelperRequest, FsHelperResponse};
use super::wire_error::{internal_error, invalid_request};
#[cfg(target_os = "macos")]
use crate::native_process::DescriptorPolicy;
use crate::native_process::{Child, ChildStdin, Command, SpawnFallback};
use ra_executor_protocol::JSONRPCErrorError;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use ra_filesystem::WindowsSandboxSelection;
use ra_filesystem::{
    FileSystemSandboxContext,
    models::PermissionProfile,
    permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
        FileSystemSpecialPath, NetworkSandboxPolicy,
    },
};
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use ra_paths::canonicalize_preserving_symlinks;
use ra_paths::{AbsolutePathBuf, PathUri};
#[cfg(any(windows, feature = "test-api"))]
use ra_paths::{LegacyAppPathString, PathConvention};
use std::collections::HashMap;
use tokio::io::AsyncWriteExt;
#[cfg(windows)]
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
const FS_HELPER_ENV_ALLOWLIST: &[&str] = &["PATH", "TMPDIR", "TMP", "TEMP"];
#[cfg(debug_assertions)]
const FS_HELPER_BAZEL_BWRAP_ENV_ALLOWLIST: &[&str] = &[
    "CARGO_BIN_EXE_bwrap",
    "RUNFILES_DIR",
    "RUNFILES_MANIFEST_FILE",
    "RUNFILES_MANIFEST_ONLY",
    "TEST_SRCDIR",
    "TEST_WORKSPACE",
];
#[derive(Clone, Debug)]
pub struct SandboxExecRequest {
    pub command: Vec<String>,
    pub cwd: PathUri,
    pub env: HashMap<String, String>,
    pub arg0: Option<String>,
    #[allow(
        dead_code,
        reason = "Retain source launch metadata for diagnostics and external source tests."
    )]
    pub sandbox_policy_cwd: PathUri,
    #[allow(
        dead_code,
        reason = "Retain source launch metadata for diagnostics and external source tests."
    )]
    pub sandbox_override: ra_filesystem::sandbox::SandboxOverride,
}
#[derive(Debug, PartialEq, Eq)]
pub struct SandboxCwd {
    pub uri: PathUri,
    pub native: AbsolutePathBuf,
}
#[derive(Clone, Debug)]
pub struct FileSystemSandboxRunner {
    runtime_paths: ExecServerRuntimeOptions,
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos")),
        allow(
            dead_code,
            reason = "Native filesystem sandbox launch is not implemented on this target."
        )
    )]
    helper_env: HashMap<String, String>,
}
impl FileSystemSandboxRunner {
    #[must_use]
    pub fn new(runtime_paths: ExecServerRuntimeOptions) -> Self {
        Self {
            runtime_paths,
            helper_env: helper_env(),
        }
    }
    pub(crate) async fn run(
        &self,
        sandbox: &FileSystemSandboxContext,
        request: FsHelperRequest,
    ) -> Result<FsHelperPayload, JSONRPCErrorError> {
        let command = self.sandbox_command(sandbox)?;
        let request_json = serde_json::to_vec(&request).map_err(json_error)?;
        run_command(command, request_json).await
    }
    pub fn sandbox_command(
        &self,
        sandbox: &FileSystemSandboxContext,
    ) -> Result<SandboxExecRequest, JSONRPCErrorError> {
        let cwd = sandbox_cwd(sandbox)?;
        let native_workspace_roots = sandbox
            .workspace_roots
            .iter()
            .map(native_workspace_root)
            .collect::<Result<Vec<_>, _>>()?;
        let workspace_roots = native_workspace_roots.as_slice();
        sandbox
            .validate_file_system_paths_for_current_host()
            .map_err(|err| invalid_request(err.to_string()))?;
        let native_permissions = sandbox
            .permissions
            .clone()
            .materialize_project_roots_with_workspace_roots(workspace_roots);
        let mut file_system_policy = native_permissions.file_system_sandbox_policy();
        tracing::Span::current().record("permission_entries", file_system_policy.entries.len());
        let helper_read_roots = if sandbox.use_legacy_landlock {
            Vec::new()
        } else {
            helper_read_roots(&self.runtime_paths)
        };
        add_helper_runtime_permissions(
            &mut file_system_policy,
            &helper_read_roots,
            cwd.native.as_path(),
        );
        // Linux resolves aliases in the sandbox helper. Doing it here also probes
        // unrelated permission roots synchronously on the executor's runtime thread.
        #[cfg(not(target_os = "linux"))]
        normalize_file_system_policy_root_aliases(&mut file_system_policy)?;
        #[cfg(windows)]
        bind_windows_cwd_relative_deny_read_globs(&mut file_system_policy, &cwd.uri)?;
        let network_policy = NetworkSandboxPolicy::Restricted;
        let permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
            native_permissions.enforcement(),
            &file_system_policy,
            network_policy,
        );
        self.sandbox_exec_request(&permission_profile, &cwd, workspace_roots, sandbox)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn sandbox_exec_request(
        &self,
        permission_profile: &PermissionProfile,
        cwd: &SandboxCwd,
        _workspace_roots: &[AbsolutePathBuf],
        sandbox_context: &FileSystemSandboxContext,
    ) -> Result<SandboxExecRequest, JSONRPCErrorError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if sandbox_context.windows_sandbox_selection == WindowsSandboxSelection::Mxc {
            return Err(invalid_request(
                "failed to prepare fs sandbox: Windows MXC sandbox is unavailable on this platform"
                    .to_string(),
            ));
        }
        let helper = &self.runtime_paths.codex_self_exe;
        let helper_cwd = cwd
            .native
            .ancestors()
            .last()
            .ok_or_else(|| invalid_request("filesystem sandbox cwd has no root".to_string()))?;
        let command = vec![
            helper.as_path().to_string_lossy().into_owned(),
            CODEX_FS_HELPER_ARG1.to_string(),
        ];
        #[cfg(not(target_os = "linux"))]
        let arg0: Option<String> = None;
        #[cfg(target_os = "linux")]
        let arg0;
        #[cfg(target_os = "macos")]
        let command = {
            let args = super::helper_seatbelt::create_seatbelt_command_args_with_profile(
                command,
                &permission_profile.file_system_sandbox_policy(),
                cwd.native.as_path(),
                self.runtime_paths.allowed_symlinked_codex_home.as_ref(),
            )
            .map_err(|error| {
                invalid_request(format!(
                    "failed to prepare fs sandbox: failed to prepare Seatbelt sandbox: {error}"
                ))
            })?;
            std::iter::once(super::helper_seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE.to_string())
                .chain(args)
                .collect()
        };
        #[cfg(target_os = "linux")]
        let command = {
            let exe = self
                .runtime_paths
                .codex_linux_sandbox_exe
                .as_ref()
                .ok_or_else(|| {
                    invalid_request(
                        "failed to prepare fs sandbox: missing codex-linux-sandbox executable path"
                            .to_string(),
                    )
                })?;
            super::helper_linux::ensure_linux_bubblewrap_is_supported(
                &permission_profile.file_system_sandbox_policy(),
                sandbox_context.use_legacy_landlock,
                false,
                super::helper_linux::is_wsl1(),
            )
            .map_err(|error| invalid_request(format!("failed to prepare fs sandbox: {error}")))?;
            let mut args =
                super::helper_linux::create_linux_sandbox_command_args_for_permission_profile(
                    command,
                    helper_cwd.as_path(),
                    permission_profile,
                    cwd.native.as_path(),
                    sandbox_context.use_legacy_landlock,
                );
            if self.runtime_paths.linux_sandbox_pid_namespace == LinuxSandboxPidNamespace::Inherit {
                args.insert(0, "--inherit-pid-namespace".to_string());
            }
            arg0 = Some(super::helper_linux::linux_sandbox_arg0_override(
                exe.as_path(),
            ));
            std::iter::once(exe.as_path().to_string_lossy().into_owned())
                .chain(args)
                .collect()
        };
        Ok(SandboxExecRequest {
            command,
            cwd: PathUri::from_abs_path(&helper_cwd),
            env: self.helper_env.clone(),
            arg0,
            sandbox_policy_cwd: cwd.uri.clone(),
            sandbox_override: sandbox_context.sandbox_override,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[allow(
        clippy::unused_self,
        reason = "Preserve the source operation signature shared by native backends."
    )]
    pub fn sandbox_exec_request(
        &self,
        _permission_profile: &PermissionProfile,
        _cwd: &SandboxCwd,
        _workspace_roots: &[AbsolutePathBuf],
        _sandbox_context: &FileSystemSandboxContext,
    ) -> Result<SandboxExecRequest, JSONRPCErrorError> {
        Err(invalid_request(
            "filesystem sandbox cannot be enforced on this executor".to_string(),
        ))
    }
}
pub fn sandbox_cwd(sandbox: &FileSystemSandboxContext) -> Result<SandboxCwd, JSONRPCErrorError> {
    Ok(SandboxCwd {
        native: native_sandbox_cwd(&sandbox.cwd)?,
        uri: sandbox.cwd.clone(),
    })
}
fn native_sandbox_cwd(cwd: &PathUri) -> Result<AbsolutePathBuf, JSONRPCErrorError> {
    cwd.to_abs_path()
        .map_err(|err| invalid_request(err.to_string()))
}
fn native_workspace_root(root: &PathUri) -> Result<AbsolutePathBuf, JSONRPCErrorError> {
    root.to_abs_path().map_err(|err| {
        invalid_request(format!(
            "file system sandbox workspace root is not native to this exec-server host: {err}"
        ))
    })
}
#[must_use]
pub fn helper_read_roots(runtime_paths: &ExecServerRuntimeOptions) -> Vec<AbsolutePathBuf> {
    let mut roots = vec![runtime_paths.codex_self_exe.clone()];
    if let Some(path) = &runtime_paths.codex_linux_sandbox_exe
        && !roots.contains(path)
    {
        roots.push(path.clone());
    }
    roots
}
pub fn add_helper_runtime_permissions(
    file_system_policy: &mut FileSystemSandboxPolicy,
    helper_read_roots: &[AbsolutePathBuf],
    cwd: &std::path::Path,
) {
    if !file_system_policy.has_full_disk_read_access() {
        let minimal_read_entry = FileSystemSandboxEntry::new(
            FileSystemPath::Special {
                value: FileSystemSpecialPath::Minimal,
            },
            FileSystemAccessMode::Read,
        );
        if !file_system_policy.entries.contains(&minimal_read_entry) {
            file_system_policy.entries.push(minimal_read_entry);
        }
    }

    for helper_read_root in helper_read_roots {
        if file_system_policy.can_read_local_path_with_cwd(helper_read_root.as_path(), cwd) {
            continue;
        }

        file_system_policy.entries.push(FileSystemSandboxEntry::new(
            helper_read_root.clone().into(),
            FileSystemAccessMode::Read,
        ));
    }
}
#[cfg(not(target_os = "linux"))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "Preserve the source fallible operation signature shared by native backends."
)]
fn normalize_file_system_policy_root_aliases(
    file_system_policy: &mut FileSystemSandboxPolicy,
) -> Result<(), JSONRPCErrorError> {
    for entry in &mut file_system_policy.entries {
        // Alias normalization uses this executor's filesystem; leave foreign
        // or opaque PathUris unchanged.
        if let FileSystemPath::Path { path } = &mut entry.path
            && let Ok(native_path) = path.to_abs_path()
        {
            #[cfg(target_os = "macos")]
            {
                *path = native_path
                    .normalize_system_aliases()
                    .map_err(|error| {
                        invalid_request(format!("failed to normalize {path}: {error}"))
                    })?
                    .into();
            }
            #[cfg(not(target_os = "macos"))]
            {
                *path = normalize_top_level_alias(native_path).into();
            }
        }
    }
    Ok(())
}
#[must_use]
pub fn helper_env() -> HashMap<String, String> {
    helper_env_from_vars(std::env::vars_os())
}
pub fn helper_env_from_vars(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> HashMap<String, String> {
    vars.into_iter()
        .filter_map(|(key, value)| {
            let key = key.to_string_lossy();
            helper_env_key_is_allowed(&key)
                .then(|| (key.into_owned(), value.to_string_lossy().into_owned()))
        })
        .collect()
}
#[must_use]
pub fn helper_env_key_is_allowed(key: &str) -> bool {
    FS_HELPER_ENV_ALLOWLIST.contains(&key)
        // CoreFoundation consults this before falling back to user lookup during helper startup.
        || (cfg!(target_os = "macos") && key == "__CF_USER_TEXT_ENCODING")
        || bazel_bwrap_env_key_is_allowed(key)
        // MXC needs SystemDrive to resolve platform directories and LOCALAPPDATA
        // to create the sandboxed helper process.
        || (cfg!(windows)
            && ["PATH", "SystemDrive", "LOCALAPPDATA"]
                .iter()
                .any(|allowed| key.eq_ignore_ascii_case(allowed)))
}
async fn run_command(
    command: SandboxExecRequest,
    request_json: Vec<u8>,
) -> Result<FsHelperPayload, JSONRPCErrorError> {
    let mut child = spawn_command(command, ChildStdin::Piped)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| internal_error("failed to open fs sandbox helper stdin".to_string()))?;

    #[cfg(windows)]
    let mut request_json = request_json;
    #[cfg(windows)]
    request_json.push(b'\n');
    stdin.write_all(&request_json).await.map_err(io_error)?;

    #[cfg(windows)]
    let response = {
        stdin.flush().await.map_err(io_error)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal_error("failed to open fs sandbox helper stdout".to_string()))?;
        let stderr = drain_helper_stderr(&mut child);
        let response = read_helper_response(stdout).await;
        drop(stdin);
        reap_helper_after_response(child, stderr).await?;
        response?
    };

    #[cfg(not(windows))]
    let response = {
        stdin.shutdown().await.map_err(io_error)?;
        drop(stdin);
        wait_for_helper_output(child).await?.stdout
    };

    let response = serde_json::from_slice(&response).map_err(json_error)?;
    match response {
        FsHelperResponse::Ok(payload) => Ok(payload),
        FsHelperResponse::Error(error) => Err(error),
    }
}
#[cfg_attr(
    windows,
    allow(
        dead_code,
        reason = "Windows reads helper output with its platform-specific cancellation path."
    )
)]
pub(crate) async fn wait_for_helper_output(
    child: Child,
) -> Result<std::process::Output, JSONRPCErrorError> {
    let output = child.wait_with_output().await.map_err(io_error)?;
    if !output.status.success() {
        return Err(internal_error(format!(
            "fs sandbox helper failed with status {status}: {stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output)
}
pub(crate) fn spawn_command(
    SandboxExecRequest {
        command: argv,
        cwd,
        mut env,
        arg0,
        ..
    }: SandboxExecRequest,
    stdin: ChildStdin,
) -> Result<Child, JSONRPCErrorError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(invalid_request("fs sandbox command was empty".to_string()));
    };
    let mut command = Command::new(program);
    #[cfg(unix)]
    if let Some(arg0) = arg0 {
        command.arg0(arg0);
    }
    #[cfg(not(unix))]
    let _ = arg0;
    command.args(args);
    // TODO(anp): Keep PathUri through the filesystem helper launch boundary.
    let cwd = cwd.to_abs_path().map_err(io_error)?;
    command.current_dir(cwd.as_path());
    env.retain(|name, _| !is_non_inheritable_env_var(name));
    command.envs(env);
    command.stdin(stdin);
    // A helper is a known executable: native launch errors must not retry through fork.
    command.fallback(SpawnFallback::ReturnError);
    // macOS cannot receive passed fds with close-on-exec set atomically.
    #[cfg(target_os = "macos")]
    command.descriptor_policy(DescriptorPolicy::Explicit);
    command.spawn().map_err(io_error)
}
pub(crate) fn io_error(err: std::io::Error) -> JSONRPCErrorError {
    internal_error(err.to_string())
}
fn json_error(err: serde_json::Error) -> JSONRPCErrorError {
    internal_error(format!(
        "failed to encode or decode fs sandbox helper message: {err}"
    ))
}
#[cfg(debug_assertions)]
fn bazel_bwrap_env_key_is_allowed(key: &str) -> bool {
    option_env!("BAZEL_PACKAGE").is_some() && FS_HELPER_BAZEL_BWRAP_ENV_ALLOWLIST.contains(&key)
}

#[cfg(not(debug_assertions))]
fn bazel_bwrap_env_key_is_allowed(_key: &str) -> bool {
    false
}

fn is_non_inheritable_env_var(name: &str) -> bool {
    [
        "CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN",
        "NODE_REPL_AUTH_TOKEN",
        "CODEX_GUARDIAN_DECISIONS_API_KEY",
        "OPENAI_FEDERATION_RULE_ID",
        "OPENAI_IDENTITY_TOKEN_FILE",
        "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
    ]
    .iter()
    .any(|restricted| restricted.eq_ignore_ascii_case(name))
}
#[cfg(windows)]
pub(crate) async fn read_helper_response(
    stdout: impl tokio::io::AsyncRead + Unpin,
) -> Result<Vec<u8>, JSONRPCErrorError> {
    let mut response = Vec::new();
    let bytes_read = tokio::io::BufReader::new(stdout)
        .read_until(b'\n', &mut response)
        .await
        .map_err(io_error)?;
    if bytes_read == 0 {
        return Err(internal_error(
            "fs sandbox helper closed stdout without responding".to_string(),
        ));
    }
    Ok(response)
}
#[cfg(windows)]
pub(crate) fn drain_helper_stderr(
    child: &mut Child,
) -> tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>> {
    let stderr_pipe = child.stderr.take();
    tokio::spawn(async move {
        let mut stderr = Vec::new();
        if let Some(mut stderr_pipe) = stderr_pipe {
            (&mut stderr_pipe)
                .take(MAX_FS_HELPER_STDERR_BYTES)
                .read_to_end(&mut stderr)
                .await?;
            tokio::io::copy(&mut stderr_pipe, &mut tokio::io::sink()).await?;
        }
        Ok::<_, std::io::Error>(stderr)
    })
}
#[cfg(windows)]
pub(crate) async fn reap_helper_after_response(
    mut child: Child,
    stderr: tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>>,
) -> Result<(), JSONRPCErrorError> {
    let (status, stderr) = if let Ok(result) = tokio::time::timeout(FS_HELPER_EXIT_TIMEOUT, async {
        tokio::try_join!(child.wait(), async {
            stderr.await.map_err(std::io::Error::other)?
        })
    })
    .await
    {
        result.map_err(io_error)?
    } else {
        tokio::time::timeout(FS_HELPER_EXIT_TIMEOUT, child.kill())
            .await
            .map_err(|_| {
                internal_error("fs sandbox helper did not stop after its response".to_string())
            })?
            .map_err(io_error)?;
        return Ok(());
    };
    if status.success() {
        return Ok(());
    }

    Err(internal_error(format!(
        "fs sandbox helper failed with status {status}: {stderr}",
        stderr = String::from_utf8_lossy(&stderr).trim()
    )))
}

#[cfg(windows)]
const FS_HELPER_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(windows)]
const MAX_FS_HELPER_STDERR_BYTES: u64 = 4096;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn normalize_top_level_alias(path: AbsolutePathBuf) -> AbsolutePathBuf {
    let raw_path = path.to_path_buf();
    for ancestor in raw_path.ancestors() {
        if std::fs::symlink_metadata(ancestor).is_err() {
            continue;
        }
        let Ok(normalized_ancestor) = canonicalize_preserving_symlinks(ancestor) else {
            continue;
        };
        if normalized_ancestor == ancestor {
            continue;
        }
        let Ok(suffix) = raw_path.strip_prefix(ancestor) else {
            continue;
        };
        if let Ok(normalized_path) =
            AbsolutePathBuf::from_absolute_path(normalized_ancestor.join(suffix))
        {
            return normalized_path;
        }
    }
    path
}

#[cfg(any(windows, feature = "test-api"))]
pub fn bind_windows_cwd_relative_deny_read_globs(
    file_system_policy: &mut FileSystemSandboxPolicy,
    cwd: &PathUri,
) -> Result<(), JSONRPCErrorError> {
    // The Windows direct-spawn wrapper reevaluates the profile using the helper cwd.
    // Bind cwd-relative denials before moving the helper to the filesystem root.
    for entry in &mut file_system_policy.entries {
        if let FileSystemPath::GlobPattern { pattern } = &mut entry.path
            && entry.access == FileSystemAccessMode::Deny
            && PathConvention::Windows
                .home_relative_suffix(pattern)
                .is_none()
            && LegacyAppPathString::from_string(pattern.as_str())
                .to_path_uri(PathConvention::Windows)
                .is_err()
        {
            cwd.validate_glob_directory(PathConvention::Windows)
                .map_err(|err| invalid_request(err.to_string()))?;
            *pattern = cwd
                .join(pattern.as_str())
                .map_err(|err| invalid_request(err.to_string()))?
                .inferred_native_path_string();
        }
    }
    Ok(())
}
