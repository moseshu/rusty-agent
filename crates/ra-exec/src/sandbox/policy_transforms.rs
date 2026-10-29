// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Apply additional permissions without flattening filesystem exclusions.
#![allow(
    missing_docs,
    clippy::match_same_arms,
    reason = "Retain upstream sandbox policy transform contracts."
)]
use ra_filesystem::models::{
    AdditionalPermissionProfile, FileSystemPermissions, PermissionProfile,
};
use ra_filesystem::permissions::{
    FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxKind,
    FileSystemSandboxPolicy, NetworkSandboxPolicy,
};
fn merge_glob_scan_max_depth(
    left_entries: &[FileSystemSandboxEntry],
    left_depth: Option<usize>,
    right_entries: &[FileSystemSandboxEntry],
    right_depth: Option<usize>,
) -> Option<usize> {
    let left_depth = effective_glob_scan_depth(left_entries, left_depth);
    let right_depth = effective_glob_scan_depth(right_entries, right_depth);

    match (left_depth, right_depth) {
        (Some(GlobScanDepth::Unbounded), _) | (_, Some(GlobScanDepth::Unbounded)) => None,
        (Some(GlobScanDepth::Bounded(left)), Some(GlobScanDepth::Bounded(right))) => {
            Some(left.max(right))
        }
        (Some(GlobScanDepth::Bounded(depth)), None)
        | (None, Some(GlobScanDepth::Bounded(depth))) => Some(depth),
        (None, None) => None,
    }
}

fn effective_glob_scan_depth(
    entries: &[FileSystemSandboxEntry],
    depth: Option<usize>,
) -> Option<GlobScanDepth> {
    entries
        .iter()
        .any(|entry| {
            entry.access == FileSystemAccessMode::Deny
                && matches!(&entry.path, FileSystemPath::GlobPattern { .. })
        })
        .then_some(match depth {
            Some(depth) => GlobScanDepth::Bounded(depth),
            None => GlobScanDepth::Unbounded,
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobScanDepth {
    Bounded(usize),
    Unbounded,
}

fn merge_file_system_policy_with_additional_permissions(
    file_system_policy: &FileSystemSandboxPolicy,
    additional_permissions: &FileSystemPermissions,
) -> FileSystemSandboxPolicy {
    match file_system_policy.kind {
        FileSystemSandboxKind::Restricted => {
            let mut merged_policy = file_system_policy.clone();
            for entry in &additional_permissions.entries {
                if !merged_policy.entries.contains(entry) {
                    merged_policy.entries.push(entry.clone());
                }
            }
            merged_policy.glob_scan_max_depth = merge_glob_scan_max_depth(
                &file_system_policy.entries,
                file_system_policy.glob_scan_max_depth,
                &additional_permissions.entries,
                additional_permissions.glob_scan_max_depth.map(usize::from),
            );
            merged_policy
        }
        FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => {
            file_system_policy.clone()
        }
    }
}

#[must_use]
pub fn effective_file_system_sandbox_policy(
    file_system_policy: &FileSystemSandboxPolicy,
    additional_permissions: Option<&AdditionalPermissionProfile>,
) -> FileSystemSandboxPolicy {
    let Some(additional_permissions) = additional_permissions else {
        return file_system_policy.clone();
    };

    let Some(file_system_permissions) = additional_permissions.file_system.as_ref() else {
        return file_system_policy.clone();
    };
    if file_system_permissions.is_empty() {
        file_system_policy.clone()
    } else {
        merge_file_system_policy_with_additional_permissions(
            file_system_policy,
            file_system_permissions,
        )
    }
}

fn merge_network_access(
    base_network_access: bool,
    additional_permissions: &AdditionalPermissionProfile,
) -> bool {
    base_network_access
        || additional_permissions
            .network
            .as_ref()
            .and_then(|network| network.enabled)
            .unwrap_or(false)
}

#[must_use]
pub fn effective_network_sandbox_policy(
    network_policy: NetworkSandboxPolicy,
    additional_permissions: Option<&AdditionalPermissionProfile>,
) -> NetworkSandboxPolicy {
    if additional_permissions
        .is_some_and(|permissions| merge_network_access(network_policy.is_enabled(), permissions))
    {
        NetworkSandboxPolicy::Enabled
    } else if additional_permissions.is_some() {
        NetworkSandboxPolicy::Restricted
    } else {
        network_policy
    }
}

#[must_use]
pub fn effective_permission_profile(
    permission_profile: &PermissionProfile,
    additional_permissions: Option<&AdditionalPermissionProfile>,
) -> PermissionProfile {
    let (file_system_policy, network_policy) = permission_profile.to_runtime_permissions();
    let effective_file_system_policy =
        effective_file_system_sandbox_policy(&file_system_policy, additional_permissions);
    let effective_network_policy =
        effective_network_sandbox_policy(network_policy, additional_permissions);
    PermissionProfile::from_runtime_permissions_with_enforcement(
        permission_profile.enforcement(),
        &effective_file_system_policy,
        effective_network_policy,
    )
}

#[must_use]
pub fn should_require_platform_sandbox(
    file_system_policy: &FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    has_managed_network_requirements: bool,
) -> bool {
    if has_managed_network_requirements {
        return true;
    }

    if !network_policy.is_enabled() {
        return !matches!(
            file_system_policy.kind,
            FileSystemSandboxKind::ExternalSandbox
        );
    }

    match file_system_policy.kind {
        FileSystemSandboxKind::Restricted => !file_system_policy.has_full_disk_write_access(),
        FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => false,
    }
}
