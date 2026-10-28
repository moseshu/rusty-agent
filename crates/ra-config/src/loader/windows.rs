// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Preserve source product configuration and validation records."
)]
use super::windows_codex_system_dir;
use std::path::Path;

/// Coarse compatibility result for the default Windows system-config namespace.
#[derive(Clone, Copy)]
pub enum WindowsSystemConfigNamespaceProbe {
    Missing,
    Expected,
    UnexpectedType,
    UnexpectedOwner,
    StandardUserMutationAcl,
    CheckError,
}

/// Observes the common user-preclaimed-directory shape without changing config loading.
/// This is not a trust decision: it only checks directory owner and broad
/// standard-user mutation ACLs.
pub fn probe_windows_system_config_namespace() -> WindowsSystemConfigNamespaceProbe {
    let codex_dir = windows_codex_system_dir();
    let has_system_file = match (
        codex_dir.join("config.toml").try_exists(),
        codex_dir.join("requirements.toml").try_exists(),
    ) {
        (Ok(true), _) | (_, Ok(true)) => true,
        (Ok(false), Ok(false)) => false,
        _ => return WindowsSystemConfigNamespaceProbe::CheckError,
    };
    if !has_system_file {
        return WindowsSystemConfigNamespaceProbe::Missing;
    }
    let Some(openai_dir) = codex_dir.parent() else {
        return WindowsSystemConfigNamespaceProbe::CheckError;
    };
    let result = probe_directory(
        openai_dir,
        ra_exec::platform_config::path_has_standard_user_mutation_allow,
    );
    if !matches!(result, WindowsSystemConfigNamespaceProbe::Expected) {
        return result;
    }
    let result = probe_directory(
        codex_dir.as_path(),
        ra_exec::platform_config::path_or_child_file_has_standard_user_mutation_allow,
    );
    if !matches!(result, WindowsSystemConfigNamespaceProbe::Expected) {
        return result;
    }
    WindowsSystemConfigNamespaceProbe::Expected
}

fn probe_directory(
    path: &Path,
    mutation_allow: fn(&Path) -> anyhow::Result<bool>,
) -> WindowsSystemConfigNamespaceProbe {
    if !path.is_dir() {
        return WindowsSystemConfigNamespaceProbe::UnexpectedType;
    }
    match ra_exec::platform_config::path_has_trusted_system_owner(path) {
        Ok(true) => {}
        Ok(false) => return WindowsSystemConfigNamespaceProbe::UnexpectedOwner,
        Err(_) => return WindowsSystemConfigNamespaceProbe::CheckError,
    }
    match mutation_allow(path) {
        Ok(false) => WindowsSystemConfigNamespaceProbe::Expected,
        Ok(true) => WindowsSystemConfigNamespaceProbe::StandardUserMutationAcl,
        Err(_) => WindowsSystemConfigNamespaceProbe::CheckError,
    }
}
