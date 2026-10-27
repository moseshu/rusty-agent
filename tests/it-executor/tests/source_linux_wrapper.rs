// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Linux wrapper support checks and exact helper invocation contracts.
use ra_exec::filesystem::test_api::helper_linux::*;
use ra_filesystem::{
    models::PermissionProfile,
    permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
        FileSystemSpecialPath, NetworkSandboxPolicy,
    },
};
use std::path::Path;

#[test]
fn wsl1_rejects_linux_bubblewrap_path() {
    let restricted = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry::new(
        FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        FileSystemAccessMode::Read,
    )]);
    for (policy, legacy, proxy) in [
        (&restricted, false, false),
        (&FileSystemSandboxPolicy::unrestricted(), false, true),
        (&FileSystemSandboxPolicy::unrestricted(), true, true),
    ] {
        assert!(matches!(
            ensure_linux_bubblewrap_is_supported(policy, legacy, proxy, true),
            Err(LinuxSandboxPreparationError::Wsl1UnsupportedForBubblewrap)
        ));
    }
}
#[test]
fn wsl1_allows_non_bubblewrap_linux_paths() {
    assert!(
        ensure_linux_bubblewrap_is_supported(
            &FileSystemSandboxPolicy::unrestricted(),
            false,
            false,
            true
        )
        .is_ok()
    );
    let restricted = FileSystemSandboxPolicy::restricted(Vec::new());
    assert!(ensure_linux_bubblewrap_is_supported(&restricted, true, false, true).is_ok());
}
#[test]
fn helper_arg0_preserves_alias_path_and_keeps_wrapper_mode_for_other_executables() {
    assert_eq!(
        linux_sandbox_arg0_override(Path::new("/opt/bin/codex-linux-sandbox")),
        "/opt/bin/codex-linux-sandbox"
    );
    for path in ["/opt/bin/codex", "/opt/bin/ra-fs-helper"] {
        assert_eq!(
            linux_sandbox_arg0_override(Path::new(path)),
            "codex-linux-sandbox"
        );
    }
}
#[test]
fn permission_profile_precedes_helper_flags_and_command_separator() {
    let profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(Vec::new()),
        NetworkSandboxPolicy::Restricted,
    );
    for legacy in [false, true] {
        let args = create_linux_sandbox_command_args_for_permission_profile(
            vec!["helper".into(), "--fs-helper".into()],
            Path::new("/"),
            &profile,
            Path::new("/workspace"),
            legacy,
        );
        assert_eq!(
            &args[..5],
            [
                "--sandbox-policy-cwd",
                "/workspace",
                "--command-cwd",
                "/",
                "--permission-profile"
            ]
        );
        assert_eq!(
            serde_json::from_str::<PermissionProfile>(&args[5]).unwrap(),
            profile
        );
        assert_eq!(
            &args[6..],
            if legacy {
                vec!["--use-legacy-landlock", "--", "helper", "--fs-helper"]
            } else {
                vec!["--", "helper", "--fs-helper"]
            }
        );
    }
}
#[test]
fn proc_version_distinguishes_wsl1_wsl2_and_native_linux() {
    for value in [
        "Linux version 4.4.0-22621-Microsoft",
        "Linux version 5.15.0-microsoft-standard-WSL1",
        "Linux version 5.15.0-wsl-microsoft-standard-WSL1",
    ] {
        assert!(proc_version_indicates_wsl1(value));
    }
    for value in [
        "Linux version 6.6.87.2-microsoft-standard-WSL2",
        "Linux version 6.6.87.2-wsl-microsoft-standard-WSL2",
        "Linux version 4.19.104-microsoft-standard",
        "Linux version 6.6.87.2-microsoft-standard-WSL3",
        "Linux version 6.8.0",
    ] {
        assert!(!proc_version_indicates_wsl1(value));
    }
}
