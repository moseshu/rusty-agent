// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Effective filesystem profiles retain deny rules when grants broaden the allow side.
use dunce::canonicalize;
use ra_exec::sandbox::policy_transforms::{
    effective_file_system_sandbox_policy, should_require_platform_sandbox,
};
use ra_filesystem::{
    models::{AdditionalPermissionProfile as PermissionProfile, FileSystemPermissions},
    permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
        FileSystemSpecialPath, NetworkSandboxPolicy,
    },
};
use ra_paths::AbsolutePathBuf;
use tempfile::TempDir;
fn merge_file_system_policy_with_additional_permissions(
    base: &FileSystemSandboxPolicy,
    permissions: &FileSystemPermissions,
) -> FileSystemSandboxPolicy {
    effective_file_system_sandbox_policy(
        base,
        Some(&PermissionProfile {
            file_system: Some(permissions.clone()),
            ..PermissionProfile::default()
        }),
    )
}

#[test]
fn merge_file_system_policy_with_additional_permissions_preserves_unreadable_roots() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let allowed_path = cwd.join("allowed");
    let denied_path = cwd.join("denied");
    let merged_policy = merge_file_system_policy_with_additional_permissions(
        &FileSystemSandboxPolicy::restricted(vec![
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
        ]),
        &FileSystemPermissions::from_read_write_roots(
            Some(vec![allowed_path.clone()]),
            Some(Vec::new()),
        ),
    );

    assert!(merged_policy.entries.contains(&FileSystemSandboxEntry {
        path: denied_path.into(),
        access: FileSystemAccessMode::Deny,
        missing_path_behavior: None,
    }));
    assert!(merged_policy.entries.contains(&FileSystemSandboxEntry {
        path: allowed_path.into(),
        access: FileSystemAccessMode::Read,
        missing_path_behavior: None,
    }));
}

#[test]
fn merge_file_system_policy_with_additional_permissions_carries_bounded_glob_scan_depth() {
    let deny_env_files = FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
        missing_path_behavior: None,
    };
    let merged_policy = merge_file_system_policy_with_additional_permissions(
        &FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Write,
            missing_path_behavior: None,
        }]),
        &FileSystemPermissions {
            entries: vec![deny_env_files.clone()],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        },
    );

    assert_eq!(merged_policy, {
        let mut policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            },
            deny_env_files,
        ]);
        policy.glob_scan_max_depth = Some(2);
        policy
    });
}

#[test]
fn effective_file_system_sandbox_policy_returns_base_policy_without_additional_permissions() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let denied_path = cwd.join("denied");
    let base_policy = FileSystemSandboxPolicy::restricted(vec![
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
    ]);

    let effective_policy =
        effective_file_system_sandbox_policy(&base_policy, /*additional_permissions*/ None);

    assert_eq!(effective_policy, base_policy);
}

#[test]
fn effective_file_system_sandbox_policy_merges_additional_write_roots() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let allowed_path = cwd.join("allowed");
    let denied_path = cwd.join("denied");
    let base_policy = FileSystemSandboxPolicy::restricted(vec![
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
    let additional_permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![allowed_path.clone()]),
        )),
        ..Default::default()
    };

    let effective_policy =
        effective_file_system_sandbox_policy(&base_policy, Some(&additional_permissions));

    assert!(effective_policy.entries.contains(&FileSystemSandboxEntry {
        path: denied_path.into(),
        access: FileSystemAccessMode::Deny,
        missing_path_behavior: None,
    }));
    assert!(effective_policy.entries.contains(&FileSystemSandboxEntry {
        path: allowed_path.into(),
        access: FileSystemAccessMode::Write,
        missing_path_behavior: None,
    }));
}

#[test]
fn full_access_restricted_policy_skips_platform_sandbox_when_network_is_enabled() {
    let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
        missing_path_behavior: None,
    }]);

    assert!(!should_require_platform_sandbox(
        &policy,
        NetworkSandboxPolicy::Enabled,
        /*has_managed_network_requirements*/ false
    ));
}

#[test]
fn root_write_policy_with_carveouts_still_uses_platform_sandbox() {
    let blocked = AbsolutePathBuf::resolve_path_against_base(
        "blocked",
        std::env::current_dir().expect("current dir"),
    );
    let policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Write,
            missing_path_behavior: None,
        },
        FileSystemSandboxEntry {
            path: blocked.into(),
            access: FileSystemAccessMode::Deny,
            missing_path_behavior: None,
        },
    ]);

    assert!(should_require_platform_sandbox(
        &policy,
        NetworkSandboxPolicy::Enabled,
        /*has_managed_network_requirements*/ false
    ));
}

#[test]
fn full_access_restricted_policy_still_uses_platform_sandbox_for_restricted_network() {
    let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
        missing_path_behavior: None,
    }]);

    assert!(should_require_platform_sandbox(
        &policy,
        NetworkSandboxPolicy::Restricted,
        /*has_managed_network_requirements*/ false
    ));
}
