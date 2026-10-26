// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Callback-local descriptor access using step settings and live originating-turn grants.

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use ra_core::{
    context::RunContext,
    error::{Error, Result},
};
use ra_paths::canonicalize_preserving_symlinks;
use ra_tools::skills_ns::{SkillExecutorAccessPolicy, SkillFilePermissions};

use crate::{
    permissions::{
        AdditionalPermissionProfile, FileSystemPermissions, is_read_denied,
        merge_permission_profiles, normalize_additional_permissions_with_context, resolve_access,
    },
    request_permissions::PolicyPaths,
    thread_settings::CodingThreadSettings,
};

pub(super) struct CodingSkillAccessPolicy;

impl SkillExecutorAccessPolicy for CodingSkillAccessPolicy {
    fn capture(&self, context: &RunContext) -> Result<Option<Arc<dyn SkillFilePermissions>>> {
        let Some(settings) = context.app_context::<CodingThreadSettings>() else {
            return Ok(None);
        };
        let owning_turn = ra_runtime::agent::tool::current_root_run_id()
            .unwrap_or_else(|| context.run_id().clone());
        let paths = PolicyPaths::new(settings.cwd());
        let context = paths.context();
        let grants = merge_permission_profiles(
            settings.granted_session_permissions().as_ref(),
            settings.granted_turn_permissions(&owning_turn).as_ref(),
        );
        let file_system = if let Some(base) = settings.file_system_read_policy() {
            let base = AdditionalPermissionProfile::new().with_file_system(base.clone());
            let merged =
                merge_permission_profiles(Some(&base), grants.as_ref()).unwrap_or_default();
            Some(
                normalize_additional_permissions_with_context(merged, &context)
                    .map_err(Error::config)?
                    .file_system()
                    .cloned()
                    .unwrap_or_default(),
            )
        } else {
            // All legacy sandbox policies allow full disk reads. Additional grants do not
            // restrict an unrestricted base, as effective_file_system_sandbox_policy specifies.
            None
        };
        let permissions = ReadPermissions {
            file_system,
            cwd: settings.cwd().to_path_buf(),
            home: context.user_home_dir().map(Path::to_path_buf),
            temporary: context.temporary_directories().unwrap_or_default().to_vec(),
            sandbox: settings.sandbox_policy().clone(),
            network_access: network_access(settings.sandbox_policy())
                || grants
                    .as_ref()
                    .and_then(AdditionalPermissionProfile::network)
                    .is_some_and(|network| network.enabled() == Some(true)),
        };
        Ok(Some(Arc::new(permissions)))
    }
}

fn network_access(policy: &ra_protocol::server::protocol::SandboxPolicy) -> bool {
    use ra_protocol::server::protocol::{NetworkAccess, SandboxPolicy};
    match policy {
        SandboxPolicy::DangerFullAccess => true,
        SandboxPolicy::ReadOnly { network_access }
        | SandboxPolicy::WorkspaceWrite { network_access, .. } => *network_access,
        SandboxPolicy::ExternalSandbox { network_access } => {
            *network_access == NetworkAccess::Enabled
        }
        _ => false,
    }
}

#[derive(PartialEq, Eq)]
struct ReadPermissions {
    file_system: Option<FileSystemPermissions>,
    cwd: PathBuf,
    home: Option<PathBuf>,
    temporary: Vec<PathBuf>,
    sandbox: ra_protocol::server::protocol::SandboxPolicy,
    network_access: bool,
}

impl SkillFilePermissions for ReadPermissions {
    fn same_permissions(&self, other: &dyn SkillFilePermissions) -> bool {
        (other as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .is_some_and(|other| self == other)
    }

    fn check_read(&self, file: &File, requested_path: &Path) -> io::Result<()> {
        let Some(policy) = &self.file_system else {
            return Ok(());
        };
        // Inspect the descriptor that will be read, rather than canonicalizing a pathname and
        // opening it again. RootedFileSystem has already constrained its resolution to the root.
        let target = descriptor_path(file)?;
        let roots = [self.cwd.clone()];
        let mut context =
            crate::permissions::FileSystemSandboxPolicyContext::new(&self.cwd, &roots)
                .with_temporary_directories(&self.temporary);
        if let Some(home) = &self.home {
            context = context.with_user_home_dir(home);
        }
        let effective_entries = effective_read_entries(policy, &context)?;
        // Keep logical deny checks as well as checks against the exact opened target. System
        // aliases and policy roots are normalized separately, without reopening the resource.
        let denied = [requested_path, target.as_path()].into_iter().any(|path| {
            is_read_denied(policy.entries(), path, &context)
                || is_read_denied(&effective_entries, path, &context)
        });
        if denied || !resolve_access(&effective_entries, &target, &context).can_read() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "skill resource read denied by filesystem permissions",
            ));
        }
        Ok(())
    }
}

/// Normalize existing policy roots, retaining missing descendants beneath resolved ancestors.
pub(super) fn effective_read_entries(
    policy: &FileSystemPermissions,
    context: &crate::permissions::FileSystemSandboxPolicyContext<'_>,
) -> io::Result<Vec<crate::permissions::FileSystemSandboxEntry>> {
    use crate::permissions::{FileSystemPath, FileSystemSandboxEntry, resolved_entries};
    let mut entries: Vec<_> = resolved_entries(policy.entries(), context)
        .into_iter()
        .map(|(root, access)| {
            FileSystemSandboxEntry::new(
                FileSystemPath::Path {
                    path: normalize_effective_path(&root),
                },
                access,
            )
        })
        .collect();
    for entry in policy.entries() {
        if let FileSystemPath::GlobPattern { pattern } = entry.path() {
            entries.push(entry.clone());
            if let Some(pattern) = normalize_glob_prefix(pattern, context)? {
                entries.push(FileSystemSandboxEntry::new(
                    FileSystemPath::GlobPattern { pattern },
                    entry.access(),
                ));
            }
        }
    }
    Ok(entries)
}

pub(super) fn normalize_effective_path(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if std::fs::symlink_metadata(ancestor).is_err() {
            continue;
        }
        let Ok(normalized_ancestor) = canonicalize_preserving_symlinks(ancestor) else {
            continue;
        };
        let Ok(suffix) = path.strip_prefix(ancestor) else {
            continue;
        };
        if let Ok(normalized) =
            crate::permissions::path::from_absolute_path(&normalized_ancestor.join(suffix))
        {
            return normalized;
        }
    }
    path.to_path_buf()
}

fn normalize_glob_prefix(
    pattern: &str,
    context: &crate::permissions::FileSystemSandboxPolicyContext<'_>,
) -> io::Result<Option<String>> {
    let absolute = if let Some(rest) = pattern.strip_prefix("~/") {
        context.user_home_dir().map(|home| home.join(rest))
    } else {
        Some(context.cwd().join(pattern))
    };
    let Some(absolute) = absolute else {
        return Ok(None);
    };
    let Some(text) = absolute.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "deny glob has no UTF-8 path representation",
        ));
    };
    if !text.contains(['*', '?', '[', ']']) {
        return normalize_effective_path(Path::new(text))
            .into_os_string()
            .into_string()
            .map(Some)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "deny glob has no UTF-8 path representation",
                )
            });
    }
    let prefix_end = text
        .find(['*', '?', '[', ']'])
        .map_or(text.len(), |wildcard| {
            text[..wildcard].rfind('/').map_or(0, |slash| slash + 1)
        });
    if prefix_end == 0 {
        return Ok(None);
    }
    let prefix = normalize_effective_path(Path::new(&text[..prefix_end]));
    let Some(prefix) = prefix.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "deny glob has no UTF-8 path representation",
        ));
    };
    Ok(Some(format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        &text[prefix_end..]
    )))
}

#[cfg(target_os = "macos")]
fn descriptor_path(file: &File) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    rustix::fs::getpath(file)
        .map(|path| std::ffi::OsString::from_vec(path.into_bytes()).into())
        .map_err(io::Error::from)
}

#[cfg(target_os = "linux")]
fn descriptor_path(file: &File) -> io::Result<PathBuf> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if !path.is_absolute() || file.metadata()?.nlink() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "opened skill resource has no filesystem path",
        ));
    }
    Ok(path)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn descriptor_path(_file: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "restricted skill reads require a supported descriptor path backend",
    ))
}
