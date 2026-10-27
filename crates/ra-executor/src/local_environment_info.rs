// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Metadata for the implemented local filesystem service.
use ra_executor_protocol::{EnvironmentCapabilities, EnvironmentInfo, ShellInfo};
use ra_paths::PathUri;
/// Returns metadata for the implemented local filesystem service.
#[must_use]
#[allow(
    unreachable_pub,
    reason = "The feature-gated source-test facade exposes the real native metadata constructor."
)]
pub fn local_environment_info() -> EnvironmentInfo {
    let windows_mxc = false;
    let cwd = std::env::current_dir().ok();
    let temporary_directories = EnvironmentInfo::local_temporary_directories();
    let normalize_temp_path = |path: std::ffi::OsString| {
        PathUri::from_host_native_path(&path).ok().or_else(|| {
            if cfg!(unix) {
                PathUri::from_host_native_path(cwd.as_ref()?.join(path)).ok()
            } else {
                None
            }
        })
    };
    let temp_dir = normalize_temp_path(std::env::temp_dir().into_os_string());

    EnvironmentInfo {
        shell: {
            let shell = ra_exec::shell_detect::default_user_shell();
            ShellInfo {
                name: shell.name().to_string(),
                path: shell.shell_path.to_string_lossy().into_owned(),
            }
        },
        executor_version: "0.0.0".to_string(),
        provider_id: None,
        cwd: cwd.and_then(|cwd| PathUri::from_host_native_path(cwd).ok()),
        user_home_dir: PathUri::from_host_native_path("~").ok(),
        platform_os: Some(std::env::consts::OS.to_string()),
        prepend_path_dirs: Vec::new(),
        temporary_directories: Some(temporary_directories),
        temp_dir,
        capabilities: EnvironmentCapabilities {
            network_proxy_launch: false,
            capability_discovery_sandbox: cfg!(any(target_os = "macos", target_os = "linux")),
            capability_discovery_v2: false,
            environment_config_read: false,
            http_header_env_vars: false,
            sandboxed_file_streaming: cfg!(any(target_os = "macos", target_os = "linux")),
            file_write_streaming: false,
            shell_snapshot_v2: false,
            windows_mxc,
            linux_root_write_preserves_devices: false,
            linux_approved_root_write_preserves_restrictions: false,
        },
    }
}
