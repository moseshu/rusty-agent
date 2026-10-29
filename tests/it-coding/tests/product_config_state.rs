// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Internal session-state ownership and development-build OAuth selection.
use ra_coding::config::test_api::{
    inherit_permission_profile_state, resolve_mcp_oauth_credentials_store_mode,
    saved_permission_profile_snapshot,
};
use ra_coding::config::{PermissionProfileSnapshot, Permissions, ProfileWorkspaceRoot};
use ra_config::{AskForApproval, Constrained, OAuthCredentialsStoreMode};
use ra_filesystem::models::{ActivePermissionProfile, PermissionProfile};

#[test]
fn local_dev_builds_force_file_mcp_oauth_store_modes() {
    for mode in [
        OAuthCredentialsStoreMode::Keyring,
        OAuthCredentialsStoreMode::Auto,
    ] {
        assert_eq!(
            resolve_mcp_oauth_credentials_store_mode(mode, "0.0.0"),
            OAuthCredentialsStoreMode::File
        );
        assert_eq!(
            resolve_mcp_oauth_credentials_store_mode(mode, "0.3.0"),
            mode
        );
        assert_eq!(
            resolve_mcp_oauth_credentials_store_mode(mode, "1.2.3"),
            mode
        );
    }
    assert_eq!(
        resolve_mcp_oauth_credentials_store_mode(OAuthCredentialsStoreMode::File, "0.0.0"),
        OAuthCredentialsStoreMode::File
    );
}

#[test]
fn inherited_permission_state_retains_identity_and_constraints_without_shared_mutation() {
    let read_only = PermissionProfile::read_only();
    let mut source = Permissions::from_approval_and_profile(
        Constrained::allow_any(AskForApproval::OnRequest),
        Constrained::allow_only(read_only.clone()),
    )
    .expect("source permissions");
    let root = ProfileWorkspaceRoot::from(
        ra_paths::PathUri::parse("file:///workspace/profile").expect("profile root URI"),
    );
    let snapshot = PermissionProfileSnapshot::active_with_profile_workspace_roots(
        read_only.clone(),
        ActivePermissionProfile::new("managed-read"),
        vec![root],
    );
    source
        .set_permission_profile_from_session_snapshot(snapshot)
        .expect("active source profile");
    let mut target = Permissions::from_approval_and_profile(
        Constrained::allow_any(AskForApproval::Never),
        Constrained::allow_any(PermissionProfile::workspace_write()),
    )
    .expect("target permissions");
    inherit_permission_profile_state(&mut target, &source);
    assert_eq!(
        saved_permission_profile_snapshot(&target),
        saved_permission_profile_snapshot(&source)
    );
    assert_eq!(target.permission_profile(), &read_only);
    assert_eq!(
        target.active_permission_profile(),
        Some(ActivePermissionProfile::new("managed-read"))
    );
    assert!(
        target
            .set_permission_profile(PermissionProfile::workspace_write())
            .is_err()
    );
    assert_eq!(target.approval_policy.value(), AskForApproval::Never);
    source
        .set_permission_profile_from_session_snapshot(PermissionProfileSnapshot::legacy(read_only))
        .expect("source update");
    assert!(source.active_permission_profile().is_none());
    assert_eq!(
        target.active_permission_profile(),
        Some(ActivePermissionProfile::new("managed-read"))
    );
}
