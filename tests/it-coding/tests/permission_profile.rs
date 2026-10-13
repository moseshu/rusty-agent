//! The permission profile and its transforms, ported from Codex's
//! `sandboxing/src/policy_transforms_tests.rs` and the `FileSystemPermissions` cases of
//! `protocol/src/models.rs`.
//!
//! Codex writes its paths as `PathUri`s; here they are native POSIX paths, so its Windows cases
//! are not ported. Native literal paths are normalized on entry; context-aware checks refuse a
//! literal that cannot be resolved. Deserialization base and home scopes follow its
//! `AbsolutePathBufGuard` tests.

use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use ra_coding::permissions::{
    AbsolutePathBufGuard, AdditionalPermissionProfile as PermissionProfile, FileSystemAccessMode,
    FileSystemPath, FileSystemPermissions, FileSystemSandboxEntry, FileSystemSandboxPolicyContext,
    FileSystemSpecialPath, NetworkPermissions, intersect_permission_profiles_with_context,
    materialize_additional_permissions_with_context, merge_permission_profiles,
    normalize_additional_permissions, normalize_additional_permissions_with_context,
};
use serde_json::json;
use tempfile::TempDir;

fn canonical_temp_dir(temp_dir: &TempDir) -> PathBuf {
    std::fs::canonicalize(temp_dir.path()).expect("canonicalize temp dir")
}

fn intersect_permission_profiles_with_local_cwd(
    requested: PermissionProfile,
    granted: PermissionProfile,
    cwd: &Path,
) -> PermissionProfile {
    let workspace_roots = [cwd.to_path_buf()];
    let context = FileSystemSandboxPolicyContext::new(cwd, &workspace_roots);
    intersect_permission_profiles_with_context(requested, granted, &context)
}

fn path(path: impl Into<PathBuf>) -> FileSystemPath {
    FileSystemPath::Path { path: path.into() }
}

fn special(value: FileSystemSpecialPath) -> FileSystemPath {
    FileSystemPath::special(value)
}

fn project_roots(subpath: Option<&str>) -> FileSystemPath {
    special(FileSystemSpecialPath::project_roots(
        subpath.map(str::to_owned),
    ))
}

fn file_system(entries: Vec<FileSystemSandboxEntry>) -> PermissionProfile {
    file_system_with_depth(entries, None)
}

fn file_system_with_depth(
    entries: Vec<FileSystemSandboxEntry>,
    depth: Option<usize>,
) -> PermissionProfile {
    PermissionProfile::new().with_file_system(
        FileSystemPermissions::new(entries)
            .with_glob_scan_max_depth(depth.and_then(NonZeroUsize::new)),
    )
}

fn read_write(read: Option<Vec<PathBuf>>, write: Option<Vec<PathBuf>>) -> PermissionProfile {
    PermissionProfile::new()
        .with_file_system(FileSystemPermissions::from_read_write_roots(read, write))
}

fn network() -> NetworkPermissions {
    NetworkPermissions::new(Some(true))
}

#[test]
fn normalize_additional_permissions_preserves_network() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = canonical_temp_dir(&temp_dir);
    let permissions = normalize_additional_permissions(
        read_write(Some(vec![path.clone()]), Some(vec![path.clone()])).with_network(network()),
    )
    .expect("permissions");

    assert_eq!(permissions.network(), Some(&network()));
    assert_eq!(
        permissions.file_system(),
        Some(&FileSystemPermissions::from_read_write_roots(
            Some(vec![path.clone()]),
            Some(vec![path]),
        ))
    );
}

/// Codex's validated path representation makes literal paths normal before context checks.
#[test]
fn normalize_additional_permissions_normalizes_literal_paths_on_entry() {
    let permissions = file_system(vec![FileSystemSandboxEntry::new(
        path("/workspace/../out"),
        FileSystemAccessMode::Write,
    )]);

    let expected = read_write(None, Some(vec!["/out".into()]));
    assert_eq!(permissions, expected);
    assert_eq!(
        normalize_additional_permissions(permissions.clone()).expect("permissions"),
        expected
    );

    let cwd = PathBuf::from("/workspace");
    let workspace_roots = [cwd.clone()];
    let context = FileSystemSandboxPolicyContext::new(&cwd, &workspace_roots);
    assert_eq!(
        normalize_additional_permissions_with_context(permissions, &context),
        Ok(expected)
    );
}

#[test]
fn normalize_additional_permissions_preserves_symlinked_write_paths() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let real_root = temp_dir.path().join("real");
    let link_root = temp_dir.path().join("link");
    std::fs::create_dir_all(real_root.join("write")).expect("create write dir");
    std::os::unix::fs::symlink(&real_root, &link_root).expect("create symlinked root");

    let permissions = normalize_additional_permissions(read_write(
        Some(vec![]),
        Some(vec![link_root.join("write")]),
    ))
    .expect("permissions");

    assert_eq!(
        permissions.file_system(),
        Some(&FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![link_root.join("write")]),
        ))
    );
}

#[test]
fn normalize_additional_permissions_rejects_glob_read_grants() {
    let err = normalize_additional_permissions(file_system(vec![FileSystemSandboxEntry::new(
        FileSystemPath::glob("**/*.env"),
        FileSystemAccessMode::Read,
    )]))
    .expect_err("read glob permissions are unsupported");

    assert_eq!(
        err,
        "glob file system permissions only support deny-read entries"
    );
}

#[test]
fn normalize_additional_permissions_preserves_deny_globs() {
    let permissions = file_system_with_depth(
        vec![FileSystemSandboxEntry::new(
            FileSystemPath::glob("**/*.env"),
            FileSystemAccessMode::Deny,
        )],
        Some(2),
    );

    assert_eq!(
        normalize_additional_permissions(permissions.clone())
            .expect("deny glob permissions are supported"),
        permissions
    );
}

#[test]
fn normalize_additional_permissions_drops_empty_nested_profiles() {
    let permissions = normalize_additional_permissions(
        PermissionProfile::new()
            .with_network(NetworkPermissions::new(None))
            .with_file_system(FileSystemPermissions::default()),
    )
    .expect("permissions");

    assert_eq!(permissions, PermissionProfile::default());
}

#[test]
fn normalize_additional_permissions_keeps_one_of_each_entry() {
    let entry = FileSystemSandboxEntry::new(path("/workspace"), FileSystemAccessMode::Write);
    assert_eq!(
        normalize_additional_permissions(file_system(vec![entry.clone(), entry.clone()]))
            .expect("permissions"),
        file_system(vec![entry])
    );
}

#[test]
fn materialize_additional_permissions_preserves_authority_and_constraints() {
    use FileSystemAccessMode::{Deny, Read, Write};

    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = canonical_temp_dir(&temp_dir);
    let home = PathBuf::from("/home/user");
    let workspace_roots = [cwd.clone()];
    let context =
        FileSystemSandboxPolicyContext::new(&cwd, &workspace_roots).with_user_home_dir(&home);
    let deny_glob =
        |pattern: String| FileSystemSandboxEntry::new(FileSystemPath::glob(pattern), Deny);
    let profile = |entries| file_system_with_depth(entries, Some(3));
    let reopened = FileSystemSandboxEntry::new(path(cwd.join("private/reopened")), Write);
    let requested = profile(vec![
        FileSystemSandboxEntry::new(project_roots(Some(".")), Write),
        FileSystemSandboxEntry::new(project_roots(Some("private")), Deny),
        FileSystemSandboxEntry::skip_missing_path(project_roots(Some("readonly")), Read),
        FileSystemSandboxEntry::new(project_roots(Some("private/reopened")), Write),
        reopened.clone(),
        deny_glob("**/*.env".to_owned()),
        deny_glob("~".to_owned()),
        deny_glob("~/private/*.env".to_owned()),
    ]);
    let expected = profile(vec![
        FileSystemSandboxEntry::new(path(cwd.clone()), Write),
        FileSystemSandboxEntry::new(path(cwd.join("private")), Deny),
        FileSystemSandboxEntry::skip_missing_path(path(cwd.join("readonly")), Read),
        reopened,
        deny_glob(cwd.join("**/*.env").to_string_lossy().into_owned()),
        deny_glob("/home/user".to_owned()),
        deny_glob("/home/user/private/*.env".to_owned()),
    ]);

    assert_eq!(
        materialize_additional_permissions_with_context(requested, &context)
            .expect("materialized permissions"),
        expected
    );
}

#[test]
fn materialize_additional_permissions_ignores_empty_tmpdir_deny() {
    let cwd = PathBuf::from("/workspace");
    let workspace_roots = [cwd.clone()];
    let temporary_directories = [];
    let context = FileSystemSandboxPolicyContext::new(&cwd, &workspace_roots)
        .with_temporary_directories(&temporary_directories);
    let write = FileSystemSandboxEntry::new(path(cwd.clone()), FileSystemAccessMode::Write);
    let permissions = file_system(vec![
        write.clone(),
        FileSystemSandboxEntry::new(
            special(FileSystemSpecialPath::Tmpdir),
            FileSystemAccessMode::Deny,
        ),
    ]);

    assert_eq!(
        materialize_additional_permissions_with_context(permissions, &context)
            .expect("permissions"),
        file_system(vec![write])
    );
}

#[test]
fn materialize_additional_permissions_refuses_an_unresolvable_deny() {
    let cwd = PathBuf::from("/workspace");
    let workspace_roots = [cwd.clone()];
    let context = FileSystemSandboxPolicyContext::new(&cwd, &workspace_roots);

    assert_eq!(
        materialize_additional_permissions_with_context(
            file_system(vec![FileSystemSandboxEntry::new(
                special(FileSystemSpecialPath::Tmpdir),
                FileSystemAccessMode::Deny,
            )]),
            &context,
        ),
        Err("unable to resolve permission path in `/workspace`".into())
    );
}

#[test]
fn intersect_permission_profiles_preserves_explicit_empty_requested_reads() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = canonical_temp_dir(&temp_dir);
    let requested = read_write(Some(vec![]), Some(vec![path]));

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(
            requested.clone(),
            requested.clone(),
            temp_dir.path()
        ),
        requested
    );
}

#[test]
fn intersect_permission_profiles_drops_ungranted_nonempty_path_requests() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let requested = read_write(Some(vec![canonical_temp_dir(&temp_dir)]), None);

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(
            requested,
            PermissionProfile::default(),
            temp_dir.path()
        ),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_drops_explicit_empty_reads_without_grant() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let requested = read_write(Some(vec![]), Some(vec![canonical_temp_dir(&temp_dir)]));

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(
            requested,
            PermissionProfile::default(),
            temp_dir.path()
        ),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_preserves_parent_relative_project_root_restriction() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path().to_path_buf();
    let cwd = root.join("project");
    let requested = file_system(vec![
        FileSystemSandboxEntry::new(
            special(FileSystemSpecialPath::Root),
            FileSystemAccessMode::Write,
        ),
        FileSystemSandboxEntry::new(
            project_roots(Some("../outside")),
            FileSystemAccessMode::Read,
        ),
    ]);
    let granted = read_write(None, Some(vec![root.join("outside/child")]));

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested, granted, &cwd),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_preserves_denies_for_unresolved_tmpdir() {
    let cwd = PathBuf::from("/workspace");
    let workspace_roots = [cwd.clone()];
    let context = FileSystemSandboxPolicyContext::new(&cwd, &workspace_roots);
    let write = FileSystemSandboxEntry::new(
        special(FileSystemSpecialPath::Tmpdir),
        FileSystemAccessMode::Write,
    );
    let deny = FileSystemSandboxEntry::new(path("/tmp/private"), FileSystemAccessMode::Deny);
    let grant = file_system(vec![write.clone()]);
    let grant_with_deny = file_system(vec![write, deny]);

    for (requested, granted) in [
        (grant_with_deny.clone(), grant.clone()),
        (grant, grant_with_deny.clone()),
        (grant_with_deny.clone(), grant_with_deny.clone()),
    ] {
        assert_eq!(
            intersect_permission_profiles_with_context(requested, granted, &context),
            grant_with_deny
        );
    }
}

/// Codex's case for the POSIX conventions; its drive and UNC cases are Windows'.
#[test]
fn intersect_permission_profiles_preserves_rooted_first_segment_glob_deny() {
    use FileSystemAccessMode::{Deny, Write};

    for (grant_path, pattern) in [("/fooX", "/foo*/*"), ("/foo\\bar", r"/foo\\*/*")] {
        let cwd = PathBuf::from("/workspace");
        let workspace_roots = [cwd.clone()];
        let context = FileSystemSandboxPolicyContext::new(&cwd, &workspace_roots);
        let grant_entry = FileSystemSandboxEntry::new(path(grant_path), Write);
        let deny_entry = FileSystemSandboxEntry::new(FileSystemPath::glob(pattern), Deny);

        assert_eq!(
            intersect_permission_profiles_with_context(
                file_system(vec![grant_entry.clone(), deny_entry.clone()]),
                file_system(vec![grant_entry.clone()]),
                &context,
            ),
            file_system(vec![grant_entry, deny_entry]),
            "deny glob should survive for {pattern}",
        );
    }
}

#[test]
fn intersect_permission_profiles_preserves_opaque_child_deny() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    use FileSystemAccessMode::{Deny, Write};

    let temp_dir = TempDir::new().expect("create temp dir");
    let root = canonical_temp_dir(&temp_dir);
    let deny_path = root.join(OsString::from_vec(b"private-\xff".to_vec()));
    let requested_write = FileSystemSandboxEntry::new(path(root.clone()), Write);
    let requested_deny = FileSystemSandboxEntry::new(path(deny_path), Deny);
    let granted_write = FileSystemSandboxEntry::new(path(root.clone()), Write);

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(
            file_system(vec![requested_write, requested_deny.clone()]),
            file_system(vec![granted_write.clone()]),
            &root,
        ),
        file_system(vec![granted_write, requested_deny])
    );
}

#[test]
fn intersect_permission_profiles_accepts_child_path_granted_for_requested_cwd() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = canonical_temp_dir(&temp_dir);
    let requested = file_system(vec![FileSystemSandboxEntry::new(
        project_roots(None),
        FileSystemAccessMode::Write,
    )]);
    let granted = read_write(None, Some(vec![cwd.join("child")]));

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested, granted.clone(), &cwd),
        granted
    );
}

#[test]
fn intersect_permission_profiles_materializes_cwd_grant_for_reuse() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let request_cwd = temp_dir.path().join("request-cwd");
    let later_cwd = temp_dir.path().join("later-cwd");
    let cwd_write_permissions = file_system(vec![FileSystemSandboxEntry::new(
        project_roots(None),
        FileSystemAccessMode::Write,
    )]);

    let intersected = intersect_permission_profiles_with_local_cwd(
        cwd_write_permissions.clone(),
        cwd_write_permissions,
        &request_cwd,
    );

    assert_eq!(
        intersected,
        read_write(None, Some(vec![request_cwd.clone()]))
    );
    assert_eq!(
        intersect_permission_profiles_with_local_cwd(
            read_write(None, Some(vec![later_cwd.join("child")])),
            intersected,
            &later_cwd,
        ),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_deduplicates_materialized_grants() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = temp_dir.path().join("cwd");
    let permissions = file_system(vec![
        FileSystemSandboxEntry::new(project_roots(None), FileSystemAccessMode::Write),
        FileSystemSandboxEntry::new(path(cwd.clone()), FileSystemAccessMode::Write),
    ]);

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(permissions.clone(), permissions, &cwd),
        read_write(None, Some(vec![cwd]))
    );
}

#[test]
fn intersect_permission_profiles_materializes_cwd_deny_entries() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let request_cwd = temp_dir.path().join("request-cwd");
    let permissions = file_system(vec![
        FileSystemSandboxEntry::new(
            special(FileSystemSpecialPath::Root),
            FileSystemAccessMode::Write,
        ),
        FileSystemSandboxEntry::new(project_roots(None), FileSystemAccessMode::Deny),
    ]);

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(
            permissions.clone(),
            permissions,
            &request_cwd
        ),
        file_system(vec![
            FileSystemSandboxEntry::new(
                special(FileSystemSpecialPath::Root),
                FileSystemAccessMode::Write,
            ),
            FileSystemSandboxEntry::new(path(request_cwd), FileSystemAccessMode::Deny),
        ])
    );
}

#[test]
fn intersect_permission_profiles_drops_deny_entries_without_filesystem_grants() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = canonical_temp_dir(&temp_dir);
    let requested = file_system(vec![
        FileSystemSandboxEntry::new(project_roots(None), FileSystemAccessMode::Write),
        FileSystemSandboxEntry::new(path(cwd.join("secret")), FileSystemAccessMode::Deny),
    ])
    .with_network(network());
    let granted = PermissionProfile::new().with_network(network());

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested, granted.clone(), &cwd),
        granted
    );
}

#[test]
fn intersect_permission_profiles_rejects_concrete_grants_matched_by_requested_deny_globs() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = canonical_temp_dir(&temp_dir);
    let requested = file_system_with_depth(
        vec![
            FileSystemSandboxEntry::new(project_roots(None), FileSystemAccessMode::Write),
            FileSystemSandboxEntry::new(
                FileSystemPath::glob("**/*.env"),
                FileSystemAccessMode::Deny,
            ),
        ],
        Some(2),
    );
    let granted = read_write(None, Some(vec![cwd.join(".env")]));

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested, granted, &cwd),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_materializes_relative_deny_globs_for_reuse() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let request_cwd = temp_dir.path().join("request-cwd");
    let later_cwd = temp_dir.path().join("later-cwd");
    let permissions = file_system_with_depth(
        vec![
            FileSystemSandboxEntry::new(project_roots(None), FileSystemAccessMode::Write),
            FileSystemSandboxEntry::new(
                FileSystemPath::glob("**/*.env"),
                FileSystemAccessMode::Deny,
            ),
        ],
        Some(2),
    );

    let intersected = intersect_permission_profiles_with_local_cwd(
        permissions.clone(),
        permissions,
        &request_cwd,
    );

    assert_eq!(
        intersected,
        file_system_with_depth(
            vec![
                FileSystemSandboxEntry::new(path(request_cwd.clone()), FileSystemAccessMode::Write),
                FileSystemSandboxEntry::new(
                    FileSystemPath::glob(request_cwd.join("**/*.env").to_string_lossy()),
                    FileSystemAccessMode::Deny,
                ),
            ],
            Some(2),
        )
    );
    assert_eq!(
        intersect_permission_profiles_with_local_cwd(
            read_write(None, Some(vec![later_cwd.join("token.env")])),
            intersected,
            &later_cwd,
        ),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_drops_broader_cwd_grant_for_requested_child_path() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = canonical_temp_dir(&temp_dir);
    let requested = read_write(None, Some(vec![cwd.join("child")]));
    let granted = file_system(vec![FileSystemSandboxEntry::new(
        project_roots(None),
        FileSystemAccessMode::Write,
    )]);

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested, granted, &cwd),
        PermissionProfile::default()
    );
}

fn glob_scan_depth_case(granted_depth: Option<usize>) {
    let cwd = std::env::current_dir().expect("current dir");
    let root_write = FileSystemSandboxEntry::new(
        special(FileSystemSpecialPath::Root),
        FileSystemAccessMode::Write,
    );
    let deny_env_files =
        FileSystemSandboxEntry::new(FileSystemPath::glob("**/*.env"), FileSystemAccessMode::Deny);
    let requested =
        file_system_with_depth(vec![root_write.clone(), deny_env_files.clone()], Some(2));
    let granted = file_system_with_depth(vec![root_write.clone(), deny_env_files], granted_depth);

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested, granted, &cwd),
        file_system_with_depth(
            vec![
                root_write,
                FileSystemSandboxEntry::new(
                    FileSystemPath::glob(cwd.join("**/*.env").to_string_lossy()),
                    FileSystemAccessMode::Deny,
                ),
            ],
            granted_depth,
        )
    );
}

#[test]
fn intersect_permission_profiles_uses_granted_bounded_glob_scan_depth() {
    glob_scan_depth_case(Some(4));
}

#[test]
fn intersect_permission_profiles_uses_granted_unbounded_glob_scan_depth() {
    glob_scan_depth_case(None);
}

#[test]
fn intersect_permission_profiles_needs_network_on_both_sides() {
    let cwd = PathBuf::from("/workspace");
    let asked = PermissionProfile::new().with_network(network());
    let refused = PermissionProfile::new().with_network(NetworkPermissions::new(Some(false)));

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(asked.clone(), asked.clone(), &cwd),
        asked
    );
    assert_eq!(
        intersect_permission_profiles_with_local_cwd(asked.clone(), refused.clone(), &cwd),
        PermissionProfile::default()
    );
    assert_eq!(
        intersect_permission_profiles_with_local_cwd(refused, asked, &cwd),
        PermissionProfile::default()
    );
}

#[test]
fn merge_permission_profiles_adds_grants_together() {
    let read = FileSystemSandboxEntry::new(path("/data"), FileSystemAccessMode::Read);
    let write = FileSystemSandboxEntry::new(path("/out"), FileSystemAccessMode::Write);
    let deny =
        FileSystemSandboxEntry::new(FileSystemPath::glob("**/*.env"), FileSystemAccessMode::Deny);
    let base =
        file_system_with_depth(vec![read.clone(), deny.clone()], Some(2)).with_network(network());
    let added = file_system_with_depth(vec![write.clone(), read.clone(), deny.clone()], Some(5));

    assert_eq!(
        merge_permission_profiles(Some(&base), Some(&added)),
        Some(
            file_system_with_depth(vec![read.clone(), deny.clone(), write], Some(5))
                .with_network(network())
        )
    );
    assert_eq!(
        merge_permission_profiles(Some(&base), Some(&file_system(vec![deny.clone()]))),
        Some(file_system(vec![read, deny]).with_network(network())),
        "an unbounded deny glob unbounds the scan"
    );
    assert_eq!(
        merge_permission_profiles(Some(&base), None),
        Some(base.clone())
    );
    assert_eq!(merge_permission_profiles(None, Some(&base)), Some(base));
    assert_eq!(
        merge_permission_profiles(None, Some(&PermissionProfile::default())),
        None
    );
}

/// Codex's `file_system_permissions_with_glob_scan_depth_uses_canonical_json`.
#[test]
fn file_system_permissions_with_glob_scan_depth_uses_canonical_json() {
    let file_system_permissions = FileSystemPermissions::new(vec![FileSystemSandboxEntry::new(
        path("/tmp/allowed"),
        FileSystemAccessMode::Read,
    )])
    .with_glob_scan_max_depth(NonZeroUsize::new(2));

    let serialized = serde_json::to_value(&file_system_permissions).expect("serialize");

    assert_eq!(
        serialized,
        json!({
            "entries": [{"path": {"type": "path", "path": "/tmp/allowed"}, "access": "read"}],
            "glob_scan_max_depth": 2,
        })
    );
    assert_eq!(
        serde_json::from_value::<FileSystemPermissions>(serialized).expect("deserialize"),
        file_system_permissions
    );
}

/// Codex's `file_system_permissions_rejects_zero_glob_scan_depth`.
#[test]
fn file_system_permissions_rejects_zero_glob_scan_depth() {
    serde_json::from_value::<FileSystemPermissions>(json!({
        "entries": [],
        "glob_scan_max_depth": 0,
    }))
    .expect_err("zero glob scan depth should fail deserialization");
}

#[test]
fn file_system_permissions_of_paths_use_read_write_lists() {
    let permissions = FileSystemPermissions::from_read_write_roots(
        Some(vec!["/data".into()]),
        Some(vec!["/out".into()]),
    );

    let serialized = serde_json::to_value(&permissions).expect("serialize");

    assert_eq!(serialized, json!({"read": ["/data"], "write": ["/out"]}));
    assert_eq!(
        serde_json::from_value::<FileSystemPermissions>(serialized).expect("deserialize"),
        permissions
    );
    assert_eq!(
        serde_json::from_value::<FileSystemPermissions>(json!({"write": ["/out/./a/../b"]}))
            .expect("deserialize"),
        FileSystemPermissions::from_read_write_roots(None, Some(vec!["/out/b".into()])),
        "paths are normalized as they are read"
    );
}

#[test]
fn profile_entries_read_codex_spellings() {
    let profile: PermissionProfile = serde_json::from_value(json!({
        "network": {"enabled": true},
        "file_system": {"entries": [
            {"path": {"type": "special", "value": {"kind": "current_working_directory"}}, "access": "write"},
            {"path": {"type": "glob_pattern", "pattern": "**/*.env"}, "access": "none"},
            {"path": {"type": "special", "value": {"kind": "unknown", "path": "x"}}, "access": "read"},
        ]}
    }))
    .expect("deserialize");

    assert_eq!(
        profile,
        file_system(vec![
            FileSystemSandboxEntry::new(project_roots(None), FileSystemAccessMode::Write),
            FileSystemSandboxEntry::new(
                FileSystemPath::glob("**/*.env"),
                FileSystemAccessMode::Deny
            ),
            FileSystemSandboxEntry::new(
                special(FileSystemSpecialPath::Unknown {
                    path: "x".into(),
                    subpath: None
                }),
                FileSystemAccessMode::Read
            ),
        ])
        .with_network(network())
    );
}

/// Two entries naming one path tie on specificity, and the later access in read < write < deny
/// decides, as Codex's `resolve_access` orders them: a path requested for reading and for writing
/// may be granted for writing.
#[test]
fn intersect_permission_profiles_breaks_a_specificity_tie_by_access() {
    let cwd = PathBuf::from("/workspace");
    let requested = file_system(vec![
        FileSystemSandboxEntry::new(path("/workspace/out"), FileSystemAccessMode::Write),
        FileSystemSandboxEntry::new(path("/workspace/out"), FileSystemAccessMode::Read),
    ]);
    let granted = read_write(None, Some(vec!["/workspace/out".into()]));

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested, granted.clone(), &cwd),
        granted
    );
}

/// A requested deny glob that cannot be compiled denies every grant, as Codex's matcher fails
/// closed.
#[test]
fn intersect_permission_profiles_fails_closed_on_an_unusable_deny_glob() {
    let cwd = PathBuf::from("/workspace");
    let requested = file_system(vec![
        FileSystemSandboxEntry::new(project_roots(None), FileSystemAccessMode::Write),
        FileSystemSandboxEntry::new(FileSystemPath::glob("secret{a"), FileSystemAccessMode::Deny),
    ]);
    let granted = read_write(None, Some(vec!["/workspace/out".into()]));

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested, granted, &cwd),
        PermissionProfile::default()
    );
}

/// Denying the whole file system leaves more specific entries to decide, as Codex's matcher leaves
/// the root out of its denied roots.
#[test]
fn intersect_permission_profiles_lets_a_specific_write_under_a_root_deny() {
    let cwd = PathBuf::from("/workspace");
    let root_deny = FileSystemSandboxEntry::new(
        special(FileSystemSpecialPath::Root),
        FileSystemAccessMode::Deny,
    );
    let write = FileSystemSandboxEntry::new(path("/workspace/out"), FileSystemAccessMode::Write);
    let requested = file_system(vec![root_deny.clone(), write.clone()]);

    assert_eq!(
        intersect_permission_profiles_with_local_cwd(
            requested,
            file_system(vec![write.clone()]),
            &cwd
        ),
        file_system(vec![write, root_deny])
    );
}

/// A deny of the project roots that has no root to resolve against voids the request rather than
/// being dropped.
#[test]
fn intersect_permission_profiles_voids_a_project_root_deny_without_roots() {
    let cwd = PathBuf::from("/workspace");
    let context = FileSystemSandboxPolicyContext::new(&cwd, &[]);
    let write = FileSystemSandboxEntry::new(path("/workspace/out"), FileSystemAccessMode::Write);
    let requested = file_system(vec![
        write.clone(),
        FileSystemSandboxEntry::new(project_roots(None), FileSystemAccessMode::Deny),
    ]);

    assert_eq!(
        intersect_permission_profiles_with_context(requested, file_system(vec![write]), &context),
        PermissionProfile::default()
    );
}

/// Parent components must be resolved before checking whether a grant fits a requested root.
/// The constructor takes native paths where Codex takes already validated absolute paths.
#[test]
fn intersect_permission_profiles_refuses_a_grant_escaping_the_requested_root() {
    let cwd = Path::new("/workspace");
    for granted_path in ["/workspace/../outside", "/workspace/sub/../../outside"] {
        let requested = read_write(None, Some(vec![cwd.to_path_buf()]));
        let granted = read_write(None, Some(vec![granted_path.into()]));
        assert_eq!(
            granted.file_system().unwrap().entries()[0].path(),
            &path("/outside"),
            "the constructor establishes the lexical path invariant",
        );
        assert_eq!(
            intersect_permission_profiles_with_local_cwd(requested, granted, cwd),
            PermissionProfile::default(),
            "the effective grant lies outside the requested root",
        );
    }
}

/// Lexical normalization must preserve valid grants and keep deny entries aimed at the same
/// effective locations, without following symlinks or rejecting harmless parent components.
#[test]
fn intersect_permission_profiles_compares_normalized_allow_and_deny_paths() {
    let cwd = Path::new("/workspace");
    let requested = file_system(vec![
        FileSystemSandboxEntry::new(path("/workspace/child/.."), FileSystemAccessMode::Write),
        FileSystemSandboxEntry::new(
            path("/workspace/open/../private"),
            FileSystemAccessMode::Deny,
        ),
    ]);
    let denied = read_write(None, Some(vec!["/workspace/child/../private/data".into()]));
    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested.clone(), denied, cwd),
        PermissionProfile::default(),
    );
    let allowed = read_write(None, Some(vec!["/workspace/child/../open".into()]));
    assert_eq!(
        intersect_permission_profiles_with_local_cwd(requested, allowed, cwd),
        read_write(None, Some(vec!["/workspace/open".into()])),
    );
}

/// A raw native literal with no absolute location must not be accepted through the equality
/// fallback used by symbolic entries. An unresolvable deny must not disappear either.
#[test]
fn unresolved_native_literals_cannot_grant_or_discard_a_deny() {
    let cwd = PathBuf::from("/workspace");
    let roots = [cwd.clone()];
    let context = FileSystemSandboxPolicyContext::new(&cwd, &roots);
    let relative = read_write(None, Some(vec!["output".into()]));
    assert_eq!(
        intersect_permission_profiles_with_context(relative.clone(), relative.clone(), &context),
        PermissionProfile::default(),
    );
    assert!(normalize_additional_permissions_with_context(relative, &context).is_err());
    for invalid in ["private", "/workspace/private\0"] {
        let requested = file_system(vec![
            FileSystemSandboxEntry::new(path(&cwd), FileSystemAccessMode::Write),
            FileSystemSandboxEntry::new(path(invalid), FileSystemAccessMode::Deny),
        ]);
        assert!(
            materialize_additional_permissions_with_context(requested.clone(), &context).is_err()
        );
        assert_eq!(
            intersect_permission_profiles_with_context(
                requested,
                read_write(None, Some(vec![cwd.clone()])),
                &context,
            ),
            PermissionProfile::default(),
        );
    }
}

/// Like `AbsolutePathBuf::deserialize`, both wire shapes require an explicit base before
/// interpreting any non-absolute spelling, including a home-relative one.
#[test]
fn permission_deserialization_requires_a_base_for_relative_paths() {
    for relative in ["output", "../output", "~/output", "~", ""] {
        for value in [
            json!({"write": [relative]}),
            json!({"entries": [{"path": {"type": "path", "path": relative}, "access": "write"}]}),
        ] {
            assert!(
                serde_json::from_value::<FileSystemPermissions>(value).is_err(),
                "{relative:?} has no deserialization base",
            );
        }
    }
    assert_eq!(
        serde_json::from_value::<FileSystemPermissions>(json!({"write": ["/out/../output"]}))
            .unwrap(),
        FileSystemPermissions::from_read_write_roots(None, Some(vec!["/output".into()])),
        "an absolute path needs no ambient cwd",
    );
}

/// Codex's `guard_used_in_deserialization` and `explicit_home_directory_is_used_with_existing_path_guards`,
/// through both permission wire shapes rather than through the standalone path type.
#[test]
fn permission_deserialization_uses_the_explicit_base_and_home() {
    let base = Path::new("/thread/workspace");
    let home = Path::new("/thread/home");
    let expected = FileSystemPermissions::from_read_write_roots(
        Some(vec![home.join("keys")]),
        Some(vec![PathBuf::from("/thread/output")]),
    );
    AbsolutePathBufGuard::with_home_directory(home, || {
        let _guard = AbsolutePathBufGuard::new(base);
        for value in [
            json!({"read": ["~//keys"], "write": ["../output/./child/.."]}),
            json!({"entries": [
                {"path": {"type": "path", "path": "~/keys"}, "access": "read"},
                {"path": {"type": "path", "path": "../output/./child/.."}, "access": "write"},
            ]}),
        ] {
            let permissions: FileSystemPermissions = serde_json::from_value(value).unwrap();
            assert_eq!(permissions, expected);
            assert_eq!(
                serde_json::from_value::<FileSystemPermissions>(
                    serde_json::to_value(permissions).unwrap()
                )
                .unwrap(),
                expected,
                "resolved paths survive a serialization round trip",
            );
        }
        let absolute: FileSystemPermissions =
            serde_json::from_value(json!({"write": ["/elsewhere/output"]})).unwrap();
        assert_eq!(
            absolute,
            FileSystemPermissions::from_read_write_roots(
                None,
                Some(vec!["/elsewhere/output".into()])
            )
        );
    });
    assert!(serde_json::from_value::<FileSystemPermissions>(json!({"write": ["output"]})).is_err());
    assert!(serde_json::from_value::<FileSystemPermissions>(json!({"read": ["~/keys"]})).is_err());
}

/// A deserialization scope belongs to one thread, not to every concurrently loaded thread.
#[test]
fn permission_deserialization_scopes_do_not_leak_to_other_threads() {
    AbsolutePathBufGuard::with_home_directory(Path::new("/thread/home"), || {
        let _guard = AbsolutePathBufGuard::new(Path::new("/thread/workspace"));
        std::thread::spawn(|| {
            assert!(
                serde_json::from_value::<FileSystemPermissions>(json!({"write": ["output"]}))
                    .is_err()
            );
            assert!(
                serde_json::from_value::<FileSystemPermissions>(json!({"read": ["~/keys"]}))
                    .is_err()
            );
        })
        .join()
        .unwrap();
        let permissions: FileSystemPermissions =
            serde_json::from_value(json!({"write": ["output"]})).unwrap();
        assert_eq!(
            permissions,
            FileSystemPermissions::from_read_write_roots(
                None,
                Some(vec!["/thread/workspace/output".into()])
            )
        );
    });
}

/// Codex's nested home override restores the caller's home even when deserialization unwinds.
#[test]
fn permission_deserialization_restores_scopes_after_nested_calls_and_panic() {
    let initial_home = AbsolutePathBufGuard::home_directory();
    AbsolutePathBufGuard::with_home_directory(Path::new("/outer/home"), || {
        let _guard = AbsolutePathBufGuard::new(Path::new("/outer/workspace"));
        let value = json!({"write": ["~/output"]});
        let inner = AbsolutePathBufGuard::with_home_directory(Path::new("/inner/home"), || {
            serde_json::from_value::<FileSystemPermissions>(value.clone()).unwrap()
        });
        assert_eq!(
            inner,
            FileSystemPermissions::from_read_write_roots(
                None,
                Some(vec!["/inner/home/output".into()])
            )
        );
        let panic = std::panic::catch_unwind(|| {
            AbsolutePathBufGuard::with_home_directory(Path::new("/panic/home"), || {
                let _guard = AbsolutePathBufGuard::new(Path::new("/panic/workspace"));
                panic!("unwind the deserialization scopes");
            });
        });
        assert!(panic.is_err());
        // A nested base guard clears the base on drop, as Codex's guard does, rather than
        // restoring the outer base. The independently scoped home is restored.
        assert!(serde_json::from_value::<FileSystemPermissions>(value.clone()).is_err());
        let _guard = AbsolutePathBufGuard::new(Path::new("/outer/workspace"));
        let restored = serde_json::from_value::<FileSystemPermissions>(value).unwrap();
        assert_eq!(
            restored,
            FileSystemPermissions::from_read_write_roots(
                None,
                Some(vec!["/outer/home/output".into()])
            )
        );
    });
    assert_eq!(AbsolutePathBufGuard::home_directory(), initial_home);
    assert!(serde_json::from_value::<FileSystemPermissions>(json!({"write": ["output"]})).is_err());
}

/// Context roots also use native paths here, so materialization must establish the invariants
/// Codex's context-owned URI paths already have before a grant is reused elsewhere.
#[test]
fn materialize_additional_permissions_normalizes_native_context_roots() {
    let cwd = Path::new("/workspace");
    let roots = [PathBuf::from("/workspace/../project")];
    let temporary = [PathBuf::from("/workspace/../temporary")];
    let context =
        FileSystemSandboxPolicyContext::new(cwd, &roots).with_temporary_directories(&temporary);
    let profile = file_system(vec![
        FileSystemSandboxEntry::new(project_roots(None), FileSystemAccessMode::Write),
        FileSystemSandboxEntry::new(
            special(FileSystemSpecialPath::Tmpdir),
            FileSystemAccessMode::Read,
        ),
    ]);
    assert_eq!(
        materialize_additional_permissions_with_context(profile.clone(), &context).unwrap(),
        file_system(vec![
            FileSystemSandboxEntry::new(path("/project"), FileSystemAccessMode::Write),
            FileSystemSandboxEntry::new(path("/temporary"), FileSystemAccessMode::Read),
        ]),
    );
    let grant = read_write(None, Some(vec!["/project/output".into()]));
    assert_eq!(
        intersect_permission_profiles_with_context(profile, grant.clone(), &context),
        grant
    );
}

/// An unresolved context directory must not erase a symbolic deny while keeping the other grants.
#[test]
fn materialize_additional_permissions_refuses_an_unresolved_tmpdir_deny() {
    let cwd = PathBuf::from("/workspace");
    let roots = [cwd.clone()];
    let temporary = [PathBuf::from("relative-tmp")];
    let context =
        FileSystemSandboxPolicyContext::new(&cwd, &roots).with_temporary_directories(&temporary);
    let requested = file_system(vec![
        FileSystemSandboxEntry::new(path(&cwd), FileSystemAccessMode::Write),
        FileSystemSandboxEntry::new(
            special(FileSystemSpecialPath::Tmpdir),
            FileSystemAccessMode::Deny,
        ),
    ]);
    assert!(materialize_additional_permissions_with_context(requested.clone(), &context).is_err());
    assert_eq!(
        intersect_permission_profiles_with_context(
            requested,
            read_write(None, Some(vec![cwd.clone()])),
            &context
        ),
        PermissionProfile::default(),
    );
}
