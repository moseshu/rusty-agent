// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Sandbox selection and transformation: the cases of Codex's `sandboxing/src/manager_tests.rs`
//! that the native executor's manager covers.
//!
//! The local manager receives native paths at the execution boundary, so a foreign cwd is a path
//! this host cannot use rather than a `PathUri` of another convention, and a transformed request
//! is observed as its argv and effective permission profile. Linux helper, WSL1, Windows and MXC
//! direct-spawn cases, and the managed-network MITM bundle, are recorded with the slices that
//! port those backends.

use dunce::canonicalize;
use ra_exec::sandbox::manager_test_api as manager;
use ra_filesystem::models::{
    AdditionalPermissionProfile, FileSystemPermissions, NetworkPermissions, PermissionProfile,
};
use ra_filesystem::permissions::{
    FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
    FileSystemSpecialPath, NetworkSandboxPolicy,
};
use ra_filesystem::sandbox::SandboxType;
use ra_paths::AbsolutePathBuf;
use std::path::Path;
use tempfile::TempDir;

use manager::SandboxablePreference;

#[test]
fn danger_full_access_defaults_to_no_sandbox_without_network_requirements() {
    let sandbox = manager::select_initial(
        &PermissionProfile::Disabled,
        SandboxablePreference::Auto,
        SandboxType::None,
        /*has_managed_network_requirements*/ false,
    );
    assert_eq!(sandbox, SandboxType::None);
}

#[test]
fn danger_full_access_uses_platform_sandbox_with_network_requirements() {
    let expected =
        manager::platform_sandbox(/*windows_sandbox_enabled*/ false).unwrap_or(SandboxType::None);
    let sandbox = manager::select_initial(
        &PermissionProfile::Disabled,
        SandboxablePreference::Auto,
        SandboxType::None,
        /*has_managed_network_requirements*/ true,
    );
    assert_eq!(sandbox, expected);
}

#[test]
fn restricted_file_system_uses_platform_sandbox_without_managed_network() {
    let expected =
        manager::platform_sandbox(/*windows_sandbox_enabled*/ false).unwrap_or(SandboxType::None);
    let permissions = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
            missing_path_behavior: None,
        }]),
        NetworkSandboxPolicy::Enabled,
    );
    let sandbox = manager::select_initial(
        &permissions,
        SandboxablePreference::Auto,
        SandboxType::None,
        /*has_managed_network_requirements*/ false,
    );
    assert_eq!(sandbox, expected);
}

/// Codex's case carries a `PathUri` of the other platform's convention; here the native cwd is
/// one this host cannot resolve. An unsandboxed request keeps its unrestricted filesystem policy.
#[test]
fn unsandboxed_transform_preserves_foreign_cwd_and_unrestricted_file_system_policy() {
    let cwd = if cfg!(windows) {
        Path::new("/workspace/remote")
    } else {
        Path::new("C:/workspace/remote")
    };
    let permissions = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::unrestricted(),
        NetworkSandboxPolicy::Restricted,
    );
    let (argv, permission_profile) = manager::transform(
        "true",
        Vec::new(),
        &permissions,
        None,
        SandboxType::None,
        cwd,
    )
    .expect("transform");

    assert_eq!(argv, vec!["true".to_owned()]);
    assert_eq!(
        permission_profile.file_system_sandbox_policy(),
        FileSystemSandboxPolicy::unrestricted()
    );
    assert_eq!(
        permission_profile.network_sandbox_policy(),
        NetworkSandboxPolicy::Restricted
    );
}

#[cfg(target_os = "macos")]
#[test]
fn symlinked_workspace_reports_seatbelt_preparation_error() {
    use std::os::unix::fs::symlink;

    let temp_dir = TempDir::new().expect("create temp dir");
    let target = temp_dir.path().join("target");
    let workspace = temp_dir.path().join("workspace");
    std::fs::create_dir(&target).expect("create target");
    symlink(&target, &workspace).expect("create symlinked workspace");
    let permissions = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::workspace_write(
            &[],
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        ),
        NetworkSandboxPolicy::Restricted,
    );

    let error = manager::transform(
        "true",
        Vec::new(),
        &permissions,
        None,
        SandboxType::MacosSeatbelt,
        &workspace,
    )
    .expect_err("symlinked workspace should be rejected");

    assert!(
        matches!(
            &error,
            ra_exec::sandbox::SandboxError::Preparation { backend: "Seatbelt", detail }
                if detail.contains("symlinked writable roots are not supported")
        ),
        "{error}"
    );
    assert!(
        !error.to_string().contains("network proxy"),
        "filesystem error should not be attributed to network proxy: {error}"
    );
}

#[test]
fn transform_additional_permissions_preserves_denied_entries() {
    let cwd = AbsolutePathBuf::current_dir().expect("current dir");
    let temp_dir = TempDir::new().expect("create temp dir");
    let workspace_root = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let allowed_path = workspace_root.join("allowed");
    let denied_path = workspace_root.join("denied");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
            missing_path_behavior: None,
        },
        FileSystemSandboxEntry {
            path: denied_path.clone().into(),
            access: FileSystemAccessMode::Deny,
            missing_path_behavior: None,
        },
    ]);
    let permissions = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );
    let additional = AdditionalPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            /*read*/ None,
            Some(vec![allowed_path.clone()]),
        )),
        ..Default::default()
    };
    let (_, permission_profile) = manager::transform(
        "true",
        Vec::new(),
        &permissions,
        Some(&additional),
        SandboxType::None,
        cwd.as_path(),
    )
    .expect("transform");

    assert_eq!(
        permission_profile.file_system_sandbox_policy(),
        FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Read,
                missing_path_behavior: None,
            },
            FileSystemSandboxEntry {
                path: denied_path.into(),
                access: FileSystemAccessMode::Deny,
                missing_path_behavior: None,
            },
            FileSystemSandboxEntry {
                path: allowed_path.into(),
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            },
        ])
    );
    assert_eq!(
        permission_profile.network_sandbox_policy(),
        NetworkSandboxPolicy::Restricted
    );
}

#[test]
fn transform_additional_permissions_enable_network_for_external_sandbox() {
    let cwd = AbsolutePathBuf::current_dir().expect("current dir");
    let permissions = PermissionProfile::External {
        network: NetworkSandboxPolicy::Restricted,
    };
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let additional = AdditionalPermissionProfile {
        network: Some(NetworkPermissions {
            enabled: Some(true),
        }),
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![path]),
            Some(Vec::new()),
        )),
    };
    let (_, permission_profile) = manager::transform(
        "true",
        Vec::new(),
        &permissions,
        Some(&additional),
        SandboxType::None,
        cwd.as_path(),
    )
    .expect("transform");

    assert_eq!(
        permission_profile,
        PermissionProfile::External {
            network: NetworkSandboxPolicy::Enabled,
        }
    );
    assert_eq!(
        permission_profile.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );
}
