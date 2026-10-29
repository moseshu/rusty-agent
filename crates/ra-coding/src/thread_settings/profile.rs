// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The canonical permission state a coding thread runs with, and its legacy wire projection.
//!
//! Codex's session keeps a [`Permissions`] value — the permission profile, its requirement
//! constraints and the active named profile — and derives the legacy `SandboxPolicy` it reports
//! for compatibility from it. A thread here does the same: the wire policy a host or client
//! supplies is lowered onto the canonical state, and the policy reported back is projected from it.

use std::path::Path;

use ra_config::{
    Constrained, NetworkAccess as WireNetworkAccess, SandboxPolicy as WireSandboxPolicy,
};
use ra_filesystem::legacy::{NetworkAccess, SandboxPolicy};
use ra_filesystem::models::{ActivePermissionProfile, PermissionProfile, SandboxEnforcement};
use ra_filesystem::permissions::{FileSystemSandboxPolicy, NetworkSandboxPolicy};
use ra_paths::AbsolutePathBuf;

use crate::config::{PermissionProfileSnapshot, Permissions};

/// Lowers a wire sandbox policy to the legacy policy the canonical state is projected from.
pub(crate) fn core_policy(policy: &WireSandboxPolicy) -> Result<SandboxPolicy, String> {
    Ok(match policy {
        WireSandboxPolicy::DangerFullAccess => SandboxPolicy::DangerFullAccess,
        WireSandboxPolicy::ReadOnly { network_access } => SandboxPolicy::ReadOnly {
            network_access: *network_access,
        },
        WireSandboxPolicy::ExternalSandbox { network_access } => SandboxPolicy::ExternalSandbox {
            network_access: match network_access {
                WireNetworkAccess::Enabled => NetworkAccess::Enabled,
                _ => NetworkAccess::Restricted,
            },
        },
        WireSandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => SandboxPolicy::WorkspaceWrite {
            writable_roots: writable_roots
                .iter()
                .map(|root| {
                    AbsolutePathBuf::from_absolute_path(root)
                        .map_err(|_| "sandboxPolicy.writableRoots must be absolute".to_owned())
                })
                .collect::<Result<_, _>>()?,
            network_access: *network_access,
            exclude_tmpdir_env_var: *exclude_tmpdir_env_var,
            exclude_slash_tmp: *exclude_slash_tmp,
        },
        // A wire policy this product does not know is confined, never granted authority.
        _ => SandboxPolicy::ReadOnly {
            network_access: false,
        },
    })
}

/// The wire form of a legacy policy.
pub(crate) fn wire_policy(policy: &SandboxPolicy) -> WireSandboxPolicy {
    match policy {
        SandboxPolicy::DangerFullAccess => WireSandboxPolicy::DangerFullAccess,
        SandboxPolicy::ReadOnly { network_access } => WireSandboxPolicy::ReadOnly {
            network_access: *network_access,
        },
        SandboxPolicy::ExternalSandbox { network_access } => WireSandboxPolicy::ExternalSandbox {
            network_access: if network_access.is_enabled() {
                WireNetworkAccess::Enabled
            } else {
                WireNetworkAccess::Restricted
            },
        },
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => WireSandboxPolicy::WorkspaceWrite {
            writable_roots: writable_roots
                .iter()
                .map(AbsolutePathBuf::to_path_buf)
                .collect(),
            network_access: *network_access,
            exclude_tmpdir_env_var: *exclude_tmpdir_env_var,
            exclude_slash_tmp: *exclude_slash_tmp,
        },
    }
}

/// Unconstrained permissions for a host that selects a legacy policy directly, without a product
/// configuration: the policy is the thread's, and no requirement limits what replaces it.
pub(crate) fn unconstrained_permissions(
    approval_policy: ra_config::AskForApproval,
    policy: &WireSandboxPolicy,
    cwd: &Path,
) -> Result<Permissions, String> {
    let mut permissions = Permissions::from_approval_and_profile(
        Constrained::allow_any(approval_policy),
        Constrained::allow_any(PermissionProfile::read_only()),
    )
    .map_err(|error| error.to_string())?;
    permissions
        .set_legacy_sandbox_policy(core_policy(policy)?, cwd)
        .map_err(|error| error.to_string())?;
    Ok(permissions)
}

/// Selects `policy` as the named profile `name` of a host-supplied catalog, keeping the thread's
/// constraints.
pub(crate) fn select_named_legacy_profile(
    permissions: &mut Permissions,
    name: &str,
    policy: &WireSandboxPolicy,
    cwd: &Path,
) -> Result<(), String> {
    let mut candidate = permissions.clone();
    set_session_legacy_policy(&mut candidate, policy, cwd)?;
    let snapshot = PermissionProfileSnapshot::active(
        candidate.permission_profile().clone(),
        ActivePermissionProfile::new(name),
    );
    candidate
        .set_permission_profile_from_session_snapshot(snapshot)
        .map_err(|error| error.to_string())?;
    *permissions = candidate;
    Ok(())
}

/// Preserve the current session's deny entries while replacing a named profile's allow rules,
/// as Codex's session projection does. Profile roots and identity stay attached to the snapshot.
pub(crate) fn preserve_session_denies(
    permissions: &Permissions,
    snapshot: &PermissionProfileSnapshot,
) -> PermissionProfileSnapshot {
    let requested = snapshot.permission_profile();
    let (mut file_system, network) = requested.to_runtime_permissions();
    file_system.preserve_deny_read_restrictions_from(
        &permissions
            .permission_profile()
            .file_system_sandbox_policy(),
    );
    let effective = PermissionProfile::from_runtime_permissions_with_enforcement(
        requested.enforcement(),
        &file_system,
        network,
    );
    match snapshot.active_permission_profile() {
        Some(active) => PermissionProfileSnapshot::active_with_profile_workspace_roots(
            effective,
            active,
            snapshot.profile_workspace_roots().to_vec(),
        ),
        None => PermissionProfileSnapshot::legacy(effective),
    }
}

/// Rebuild a legacy session policy with its existing deny entries and glob scan limit.
/// Unrestricted legacy modes retain Codex's unrestricted-mode semantics.
pub(crate) fn set_session_legacy_policy(
    permissions: &mut Permissions,
    policy: &WireSandboxPolicy,
    cwd: &Path,
) -> Result<(), String> {
    let policy = core_policy(policy)?;
    let file_system = FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
        &policy,
        cwd,
        &permissions
            .permission_profile()
            .file_system_sandbox_policy(),
    );
    let requested = PermissionProfile::from_runtime_permissions_with_enforcement(
        SandboxEnforcement::from_legacy_sandbox_policy(&policy),
        &file_system,
        NetworkSandboxPolicy::from(&policy),
    );
    // Keep the legacy workspace-root bookkeeping and managed constraints, then install the
    // session projection. The caller prepares a clone, so failures cannot publish partial state.
    permissions
        .set_legacy_sandbox_policy(policy, cwd)
        .map_err(|error| error.to_string())?;
    permissions
        .set_permission_profile(requested)
        .map_err(|error| error.to_string())
}
