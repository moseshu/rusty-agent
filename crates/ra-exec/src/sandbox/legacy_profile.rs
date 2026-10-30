//! Compatibility check for root-list backends that cannot enforce canonical exclusions.

use std::path::{Path, PathBuf};

use ra_filesystem::{
    models::PermissionProfile,
    permissions::{FileSystemAccessMode, FileSystemSandboxEntry, FileSystemSandboxPolicy},
};

use super::SandboxError;

pub(super) fn writable_roots(
    profile: &PermissionProfile,
    cwd: &Path,
) -> Result<Vec<PathBuf>, SandboxError> {
    let reject = |detail: &str| SandboxError::Preparation {
        backend: "permission-profile",
        detail: format!(
            "{detail}; the legacy command sandbox cannot enforce this profile, so the command was not run"
        ),
    };
    if !cwd.is_absolute() {
        return Err(reject("sandbox policy cwd must be absolute"));
    }
    let (file_system, network) = profile.to_runtime_permissions();
    if !super::policy_transforms::should_require_platform_sandbox(&file_system, network, false) {
        return Ok(Vec::new());
    }
    if file_system.has_denied_read_restrictions() {
        return Err(reject(
            "the permission profile contains denied-read restrictions",
        ));
    }
    // Workspace-write mounts the whole host read-only. A narrower read policy cannot be
    // represented by that mount, even when its writable roots would otherwise fit.
    if !file_system.has_full_disk_read_access() {
        return Err(reject("the permission profile restricts readable paths"));
    }
    let roots = file_system.get_writable_roots_with_cwd(cwd);
    if roots
        .iter()
        .any(|root| !root.protected_metadata_names.is_empty())
    {
        return Err(reject(
            "the permission profile requires protected metadata directories",
        ));
    }
    if roots.iter().any(|root| !root.read_only_subpaths.is_empty()) {
        return Err(reject(
            "the permission profile requires read-only paths within writable roots",
        ));
    }
    // Verify the complete projection, so unresolved or future policy entries cannot disappear
    // just because the current root resolver did not produce a mount for them.
    let mut represented = FileSystemSandboxPolicy::read_only();
    represented.entries.extend(roots.iter().map(|root| {
        FileSystemSandboxEntry::new(root.root.clone().into(), FileSystemAccessMode::Write)
    }));
    if !file_system.is_semantically_equivalent_to(&represented, cwd) {
        return Err(reject(
            "the permission profile cannot be represented as legacy roots",
        ));
    }
    Ok(roots
        .into_iter()
        .map(|root| root.root.to_path_buf())
        .collect())
}
