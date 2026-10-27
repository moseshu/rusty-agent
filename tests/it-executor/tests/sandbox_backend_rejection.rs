//! Unavailable native sandbox selections must never launch an unrestricted helper.

use ra_exec::filesystem::test_api::fs_sandbox::{
    ExecServerRuntimeOptions, FileSystemSandboxRunner,
};
use ra_filesystem::{FileSystemSandboxContext, WindowsSandboxSelection, models::PermissionProfile};
use ra_paths::PathUri;

#[cfg(windows)]
#[test]
fn unavailable_windows_backends_reject_restricted_filesystem_requests() {
    let paths = ExecServerRuntimeOptions::new(std::env::current_exe().unwrap(), None).unwrap();
    let runner = FileSystemSandboxRunner::new(paths);
    let cwd = PathUri::from_host_native_path(std::env::temp_dir()).unwrap();
    for selection in [
        WindowsSandboxSelection::Disabled,
        WindowsSandboxSelection::RestrictedToken,
        WindowsSandboxSelection::Elevated,
        WindowsSandboxSelection::Mxc,
    ] {
        let mut context = FileSystemSandboxContext::from_permission_profile(
            PermissionProfile::read_only(),
            cwd.clone(),
        );
        context.windows_sandbox_selection = selection;
        assert_eq!(
            runner.sandbox_command(&context).unwrap_err().message,
            "filesystem sandbox cannot be enforced on this executor",
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn mxc_selection_is_rejected_on_non_windows_hosts() {
    let paths = ExecServerRuntimeOptions::new(std::env::current_exe().unwrap(), None).unwrap();
    let runner = FileSystemSandboxRunner::new(paths);
    let cwd = PathUri::from_host_native_path(std::env::temp_dir()).unwrap();
    let mut context =
        FileSystemSandboxContext::from_permission_profile(PermissionProfile::read_only(), cwd);
    context.windows_sandbox_selection = WindowsSandboxSelection::Mxc;
    assert!(
        runner
            .sandbox_command(&context)
            .unwrap_err()
            .message
            .contains("Windows MXC sandbox is unavailable on this platform")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn missing_external_linux_sandbox_rejects_restricted_filesystem_requests() {
    let paths = ExecServerRuntimeOptions::new(std::env::current_exe().unwrap(), None).unwrap();
    let runner = FileSystemSandboxRunner::new(paths);
    let cwd = PathUri::from_host_native_path(std::env::temp_dir()).unwrap();
    let context =
        FileSystemSandboxContext::from_permission_profile(PermissionProfile::read_only(), cwd);
    assert!(
        runner
            .sandbox_command(&context)
            .unwrap_err()
            .message
            .contains("missing codex-linux-sandbox executable path")
    );
}
