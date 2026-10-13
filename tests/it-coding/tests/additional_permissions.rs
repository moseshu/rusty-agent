//! What a command asks for beyond its sandbox, and when the grants preapprove it: Codex's tests of
//! `core/src/tools/handlers/mod.rs` and `permission_preapproval_tests.rs`.
//!
//! Codex writes its paths as `PathUri`s; here they are native POSIX paths, so its Windows
//! temporary directory case is ported with POSIX paths.

use std::path::{Path, PathBuf};

use ra_coding::{
    additional_permissions_test_api::{
        implicit_granted_permissions, normalize_and_validate_additional_permissions,
        preapproved_permission_profile,
    },
    exec_policy::SandboxPermissions,
    permissions::{
        AdditionalPermissionProfile, FileSystemAccessMode, FileSystemPath, FileSystemPermissions,
        FileSystemSandboxEntry, FileSystemSandboxPolicyContext, FileSystemSpecialPath,
        NetworkPermissions, intersect_permission_profiles_with_context, merge_permission_profiles,
    },
};
use ra_protocol::server::protocol::AskForApproval;
use tempfile::tempdir;

fn network_permissions() -> AdditionalPermissionProfile {
    AdditionalPermissionProfile::new().with_network(NetworkPermissions::new(Some(true)))
}

fn write_permissions(path: &Path) -> AdditionalPermissionProfile {
    AdditionalPermissionProfile::new().with_file_system(
        FileSystemPermissions::from_read_write_roots(None, Some(vec![path.to_path_buf()])),
    )
}

fn file_system(entries: Vec<FileSystemSandboxEntry>) -> AdditionalPermissionProfile {
    AdditionalPermissionProfile::new().with_file_system(FileSystemPermissions::new(entries))
}

fn entry(path: impl Into<PathBuf>, access: FileSystemAccessMode) -> FileSystemSandboxEntry {
    FileSystemSandboxEntry::new(FileSystemPath::Path { path: path.into() }, access)
}

fn special(value: FileSystemSpecialPath, access: FileSystemAccessMode) -> FileSystemSandboxEntry {
    FileSystemSandboxEntry::new(FileSystemPath::special(value), access)
}

fn granular_without_permission_prompts() -> AskForApproval {
    AskForApproval::Granular {
        sandbox_approval: true,
        rules: true,
        skill_approval: true,
        request_permissions: false,
        mcp_elicitations: true,
    }
}

#[test]
fn preapproved_permissions_work_when_request_permissions_tool_is_enabled_without_exec_permission_approvals_feature()
 {
    let cwd = tempdir().unwrap();
    let roots = [cwd.path().to_path_buf()];
    let context = FileSystemSandboxPolicyContext::new(cwd.path(), &roots);

    let normalized = normalize_and_validate_additional_permissions(
        false,
        granular_without_permission_prompts(),
        SandboxPermissions::WithAdditionalPermissions,
        Some(network_permissions()),
        true,
        &context,
    )
    .expect("preapproved permissions should be allowed");

    assert_eq!(normalized, Some(network_permissions()));
}

#[test]
fn fresh_additional_permissions_still_require_exec_permission_approvals_feature() {
    let cwd = tempdir().unwrap();
    let roots = [cwd.path().to_path_buf()];
    let context = FileSystemSandboxPolicyContext::new(cwd.path(), &roots);

    let error = normalize_and_validate_additional_permissions(
        false,
        AskForApproval::OnRequest,
        SandboxPermissions::WithAdditionalPermissions,
        Some(network_permissions()),
        false,
        &context,
    )
    .expect_err("fresh inline permission requests should remain disabled");

    assert_eq!(
        error,
        "additional permissions are disabled; enable `features.exec_permission_approvals` before using `with_additional_permissions`"
    );
}

/// The rest of Codex's checks of what a call asks for inline, in the order it makes them.
#[test]
fn inline_permission_requests_are_checked_as_codex_checks_them() {
    let cwd = tempdir().unwrap();
    let roots = [cwd.path().to_path_buf()];
    let context = FileSystemSandboxPolicyContext::new(cwd.path(), &roots);
    let check = |policy, sandbox_permissions, permissions| {
        normalize_and_validate_additional_permissions(
            true,
            policy,
            sandbox_permissions,
            permissions,
            false,
            &context,
        )
    };

    assert_eq!(
        check(
            AskForApproval::UnlessTrusted,
            SandboxPermissions::WithAdditionalPermissions,
            Some(network_permissions()),
        ),
        Err("approval policy is UnlessTrusted; reject command — you cannot request additional permissions unless the approval policy is OnRequest".to_owned())
    );
    assert_eq!(
        check(
            AskForApproval::OnRequest,
            SandboxPermissions::WithAdditionalPermissions,
            None,
        ),
        Err("missing `additional_permissions`; provide at least one of `network` or `file_system` when using `with_additional_permissions`".to_owned())
    );
    assert_eq!(
        check(
            AskForApproval::OnRequest,
            SandboxPermissions::WithAdditionalPermissions,
            Some(AdditionalPermissionProfile::new()),
        ),
        Err("`additional_permissions` must include at least one requested permission in `network` or `file_system`".to_owned())
    );
    assert_eq!(
        check(
            AskForApproval::OnRequest,
            SandboxPermissions::UseDefault,
            Some(network_permissions()),
        ),
        Err("`additional_permissions` requires `sandbox_permissions` set to `with_additional_permissions`".to_owned())
    );
    assert_eq!(
        check(
            AskForApproval::OnRequest,
            SandboxPermissions::WithAdditionalPermissions,
            Some(network_permissions()),
        ),
        Ok(Some(network_permissions()))
    );
    assert_eq!(
        check(AskForApproval::Never, SandboxPermissions::UseDefault, None),
        Ok(None)
    );
}

#[test]
fn implicit_sticky_grants_bypass_inline_permission_validation() {
    let cwd = tempdir().unwrap();
    let granted_permissions = write_permissions(cwd.path());

    assert_eq!(
        implicit_granted_permissions(
            SandboxPermissions::UseDefault,
            None,
            SandboxPermissions::WithAdditionalPermissions,
            Some(granted_permissions.clone()),
            false,
        ),
        Some(granted_permissions)
    );
}

#[test]
fn explicit_inline_permissions_do_not_use_implicit_sticky_grant_path() {
    let cwd = tempdir().unwrap();
    let requested_permissions = write_permissions(cwd.path());

    assert_eq!(
        implicit_granted_permissions(
            SandboxPermissions::WithAdditionalPermissions,
            Some(&requested_permissions),
            SandboxPermissions::WithAdditionalPermissions,
            Some(requested_permissions.clone()),
            false,
        ),
        None
    );
}

#[test]
fn relative_deny_glob_grants_remain_preapproved_after_materialization() {
    let cwd = tempdir().unwrap();
    let roots = [cwd.path().to_path_buf()];
    let context = FileSystemSandboxPolicyContext::new(cwd.path(), &roots);
    let requested_permissions = file_system(vec![
        special(
            FileSystemSpecialPath::project_roots(None),
            FileSystemAccessMode::Write,
        ),
        FileSystemSandboxEntry::new(FileSystemPath::glob("**/*.env"), FileSystemAccessMode::Deny),
    ]);
    let stored_grant = intersect_permission_profiles_with_context(
        requested_permissions.clone(),
        requested_permissions.clone(),
        &context,
    );
    let effective_permissions =
        merge_permission_profiles(Some(&requested_permissions), Some(&stored_grant))
            .expect("merged permissions");

    assert_eq!(
        preapproved_permission_profile(&effective_permissions, &stored_grant, &context),
        Some(stored_grant)
    );
}

#[test]
fn symbolic_tmpdir_preapproval_requires_executor_metadata() {
    let cwd = PathBuf::from("/workspace");
    let roots = [cwd.clone()];
    let temporary_directory = PathBuf::from("/private/tmp/executor");
    let concrete = entry(&temporary_directory, FileSystemAccessMode::Write);
    let effective = file_system(vec![
        concrete.clone(),
        special(FileSystemSpecialPath::Tmpdir, FileSystemAccessMode::Write),
    ]);
    let granted = file_system(vec![concrete]);
    let context = FileSystemSandboxPolicyContext::new(&cwd, &roots);

    assert_eq!(
        preapproved_permission_profile(&effective, &granted, &context),
        None
    );

    let no_temporary_directories: [PathBuf; 0] = [];
    assert_eq!(
        preapproved_permission_profile(
            &effective,
            &granted,
            &context.with_temporary_directories(&no_temporary_directories),
        ),
        None
    );

    let temporary_directories = [temporary_directory];
    assert_eq!(
        preapproved_permission_profile(
            &effective,
            &granted,
            &context.with_temporary_directories(&temporary_directories),
        ),
        Some(granted)
    );
}

#[test]
fn preapproval_accepts_reordered_replay_of_one_accumulated_grant() {
    let cwd = tempdir().unwrap();
    let root = cwd.path();
    let roots = [root.to_path_buf()];
    let write = entry(root, FileSystemAccessMode::Write);
    let granted = file_system(vec![
        entry(root.join("secrets"), FileSystemAccessMode::Deny),
        write.clone(),
        entry(root.join("readonly"), FileSystemAccessMode::Read),
    ])
    .with_network(NetworkPermissions::new(Some(true)));
    let replay = file_system(vec![write]);
    let effective = merge_permission_profiles(Some(&replay), Some(&granted)).expect("permissions");
    let context = FileSystemSandboxPolicyContext::new(root, &roots);

    assert_eq!(
        preapproved_permission_profile(&effective, &granted, &context),
        Some(granted)
    );
}

#[test]
fn preapproval_requires_fresh_read_and_write_beneath_a_deny() {
    let cwd = tempdir().unwrap();
    let root = cwd.path();
    let roots = [root.to_path_buf()];
    let granted = file_system(vec![
        entry(root, FileSystemAccessMode::Write),
        entry(root.join("secrets"), FileSystemAccessMode::Deny),
    ]);
    let context = FileSystemSandboxPolicyContext::new(root, &roots);

    let preapproved = [FileSystemAccessMode::Read, FileSystemAccessMode::Write].map(|access| {
        let requested = file_system(vec![entry(root.join("secrets/token.txt"), access)]);
        let effective =
            merge_permission_profiles(Some(&requested), Some(&granted)).expect("permissions");
        preapproved_permission_profile(&effective, &granted, &context)
    });

    assert_eq!(preapproved, [None, None]);
}

#[test]
fn preapproval_fails_closed_when_materialization_rejects_both_profiles() {
    let cwd = tempdir().unwrap();
    let roots = [cwd.path().to_path_buf()];
    let invalid = file_system(vec![FileSystemSandboxEntry::new(
        FileSystemPath::glob("**/secrets"),
        FileSystemAccessMode::Write,
    )]);
    let context = FileSystemSandboxPolicyContext::new(cwd.path(), &roots);

    assert_eq!(
        preapproved_permission_profile(&invalid, &invalid, &context),
        None
    );
}

/// Preapproval needs the grant to give the network access asked for, and the very entries asked
/// for, not merely as many.
#[test]
fn preapproval_requires_the_same_network_and_entries() {
    let cwd = tempdir().unwrap();
    let root = cwd.path();
    let roots = [root.to_path_buf()];
    let context = FileSystemSandboxPolicyContext::new(root, &roots);
    let granted = write_permissions(&root.join("granted"));

    let with_network = granted
        .clone()
        .with_network(NetworkPermissions::new(Some(true)));
    assert_eq!(
        preapproved_permission_profile(&with_network, &granted, &context),
        None
    );
    assert_eq!(
        preapproved_permission_profile(&write_permissions(&root.join("other")), &granted, &context),
        None
    );
    assert_eq!(
        preapproved_permission_profile(&granted, &granted, &context),
        Some(granted)
    );
}
