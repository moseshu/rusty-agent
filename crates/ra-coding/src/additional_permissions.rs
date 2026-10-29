//! What a command or patch runs with beyond its thread's sandbox: the permissions it asks for
//! itself and those `request_permissions` granted the turn and the session. Codex's helpers in
//! `core/src/tools/handlers/mod.rs` (`apply_granted_turn_permissions`,
//! `implicit_granted_permissions`, `normalize_and_validate_additional_permissions`,
//! `preapproved_permission_profile`), the preapproval `handlers/apply_patch.rs` gives a patch the
//! grants cover, and `effective_permission_profile` from `sandboxing/src/policy_transforms.rs`
//! lowered onto the executor's sandbox.
//!
//! # Differences from Codex
//!
//! - macOS applies additional permissions to the canonical profile through Seatbelt. Legacy
//!   backends still refuse grants containing denied reads or read-only subpaths they cannot express.
//! - Codex lets inline permissions through without its `exec_permission_approvals` feature when
//!   its `request_permissions_tool` feature is on and the grants preapprove them. Grants exist only
//!   once that tool was installed and answered, so here preapproval alone lets them through.
//! - A patch is preapproved only when every path it touches is itself writable: Codex accepts a
//!   writable parent directory and relies on its patch sandbox, which the in-process writer lacks.
//! - Writability is decided lexically over the executor's roots and the grants; on macOS paths are
//!   first resolved as Seatbelt resolves its trusted top-level aliases. Deny glob prefixes use the
//!   same namespace; a glob with a nonliteral top-level component cannot be safely translated on
//!   macOS, so it prevents preapproval. Unresolvable restrictions are never silently discarded.

use std::path::{Path, PathBuf};

use ra_core::{state::RunId, tool::ToolContext};
use ra_exec::sandbox::{NetworkAccess as ExecNetworkAccess, SandboxPolicy as ExecSandboxPolicy};
use ra_protocol::server::protocol::AskForApproval;

use crate::{
    exec_policy::SandboxPermissions,
    permissions::{
        AdditionalPermissionProfile, FileSystemAccessMode, FileSystemPath, FileSystemPermissions,
        FileSystemSandboxEntry, FileSystemSandboxPolicyContext, FileSystemSpecialPath,
        NetworkPermissions, materialize_additional_permissions_with_context,
        merge_permission_profiles, normalize_additional_permissions_with_context, resolve_access,
        resolved_entries,
    },
    thread_settings::CodingThreadSettings,
};

/// What a call runs with once the grants are added to what it asked for: Codex's
/// `EffectiveAdditionalPermissions`.
pub(crate) struct EffectiveAdditionalPermissions {
    pub(crate) sandbox_permissions: SandboxPermissions,
    pub(crate) additional_permissions: Option<AdditionalPermissionProfile>,
    pub(crate) permissions_preapproved: bool,
}

/// The turn a call belongs to, however deeply it is nested: the one its grants are kept for.
fn owning_turn(context: &ToolContext<'_>) -> RunId {
    ra_runtime::agent::tool::current_root_run_id().unwrap_or_else(|| context.run().run_id().clone())
}

/// What the session and the call's turn were granted, together.
fn granted_permissions(
    context: &ToolContext<'_>,
    settings: &CodingThreadSettings,
) -> Option<AdditionalPermissionProfile> {
    let permissions = settings.thread_permissions();
    merge_permission_profiles(
        permissions.session().as_ref(),
        permissions.turn(&owning_turn(context)).as_ref(),
    )
}

/// The session's and the turn's grants added to the call's own request: Codex's
/// `apply_granted_turn_permissions`. A call leaving the sandbox takes none. The call is preapproved
/// when it asks for nothing the grants do not already give.
pub(crate) fn apply_granted_turn_permissions(
    context: &ToolContext<'_>,
    settings: &CodingThreadSettings,
    policy_context: &FileSystemSandboxPolicyContext<'_>,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
) -> EffectiveAdditionalPermissions {
    if sandbox_permissions.requires_escalated_permissions() {
        return EffectiveAdditionalPermissions {
            sandbox_permissions,
            additional_permissions,
            permissions_preapproved: false,
        };
    }
    let granted_permissions = granted_permissions(context, settings);
    let effective_permissions = merge_permission_profiles(
        additional_permissions.as_ref(),
        granted_permissions.as_ref(),
    );
    let preapproved_permissions = granted_permissions.as_ref().and_then(|granted| {
        if additional_permissions.is_none() {
            Some(granted.clone())
        } else {
            effective_permissions.as_ref().and_then(|effective| {
                preapproved_permission_profile(effective, granted, policy_context)
            })
        }
    });
    let permissions_preapproved = preapproved_permissions.is_some();
    // A preapproved command must execute with the stored authority, never an unchecked merge that
    // could reopen one of the grant's denied paths.
    let effective_permissions = preapproved_permissions.or(effective_permissions);
    let sandbox_permissions =
        if effective_permissions.is_some() && !sandbox_permissions.uses_additional_permissions() {
            SandboxPermissions::WithAdditionalPermissions
        } else {
            sandbox_permissions
        };
    EffectiveAdditionalPermissions {
        sandbox_permissions,
        additional_permissions: effective_permissions,
        permissions_preapproved,
    }
}

/// The grant `granted` when `effective`, once resolved, gives nothing beyond it: Codex's
/// `preapproved_permission_profile`. Entries that cannot be resolved where the call runs are never
/// preapproved.
pub(crate) fn preapproved_permission_profile(
    effective_permissions: &AdditionalPermissionProfile,
    granted_permissions: &AdditionalPermissionProfile,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Option<AdditionalPermissionProfile> {
    if effective_permissions
        .file_system()
        .is_some_and(|permissions| {
            permissions
                .entries()
                .iter()
                .any(|entry| match entry.path() {
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Tmpdir,
                    } => context
                        .temporary_directories()
                        .is_none_or(<[PathBuf]>::is_empty),
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::ProjectRoots { .. },
                    } => context.workspace_roots().is_empty(),
                    _ => false,
                })
        })
    {
        return None;
    }
    let (Ok(effective), Ok(granted)) = (
        materialize_additional_permissions_with_context(effective_permissions.clone(), context),
        materialize_additional_permissions_with_context(granted_permissions.clone(), context),
    ) else {
        return None;
    };
    if effective.network() != granted.network() {
        return None;
    }
    let unchanged = match (effective.file_system(), granted.file_system()) {
        (Some(effective), Some(granted)) => {
            effective.glob_scan_max_depth() == granted.glob_scan_max_depth()
                && effective.entries().len() == granted.entries().len()
                && effective
                    .entries()
                    .iter()
                    .all(|entry| granted.entries().contains(entry))
        }
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    };
    unchanged.then(|| granted_permissions.clone())
}

/// The grants a call runs with when it asked for nothing itself: Codex's
/// `implicit_granted_permissions`. Such a call skips the checks of what a call asks for inline.
pub(crate) fn implicit_granted_permissions(
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<&AdditionalPermissionProfile>,
    effective_additional_permissions: &EffectiveAdditionalPermissions,
) -> Option<AdditionalPermissionProfile> {
    if !sandbox_permissions.uses_additional_permissions()
        && !sandbox_permissions.requires_escalated_permissions()
        && additional_permissions.is_none()
    {
        effective_additional_permissions
            .additional_permissions
            .clone()
    } else {
        None
    }
}

/// Checks what a call asks for with `with_additional_permissions` against what may be asked and
/// returns it normalized: Codex's `normalize_and_validate_additional_permissions`.
///
/// # Errors
///
/// Returns Codex's refusal the model reads.
pub(crate) fn normalize_and_validate_additional_permissions(
    additional_permissions_allowed: bool,
    approval_policy: AskForApproval,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
    permissions_preapproved: bool,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Result<Option<AdditionalPermissionProfile>, String> {
    let uses_additional_permissions = sandbox_permissions.uses_additional_permissions();

    if !permissions_preapproved
        && !additional_permissions_allowed
        && (uses_additional_permissions || additional_permissions.is_some())
    {
        return Err(
            "additional permissions are disabled; enable `features.exec_permission_approvals` before using `with_additional_permissions`"
                .to_owned(),
        );
    }

    if uses_additional_permissions {
        // Codex formats the policy with `Debug`, which differs from Codex's only for `Granular`.
        if !permissions_preapproved && !matches!(approval_policy, AskForApproval::OnRequest) {
            return Err(format!(
                "approval policy is {approval_policy:?}; reject command — you cannot request additional permissions unless the approval policy is OnRequest"
            ));
        }
        let Some(additional_permissions) = additional_permissions else {
            return Err(
                "missing `additional_permissions`; provide at least one of `network` or `file_system` when using `with_additional_permissions`"
                    .to_owned(),
            );
        };
        let normalized =
            normalize_additional_permissions_with_context(additional_permissions, context)?;
        if normalized.is_empty() {
            return Err(
                "`additional_permissions` must include at least one requested permission in `network` or `file_system`"
                    .to_owned(),
            );
        }
        return Ok(Some(normalized));
    }

    if additional_permissions.is_some() {
        Err(
            "`additional_permissions` requires `sandbox_permissions` set to `with_additional_permissions`"
                .to_owned(),
        )
    } else {
        Ok(None)
    }
}

/// Whether a patch touching `paths` runs without asking. Codex's patch handler passes the paths it
/// cannot write to `apply_granted_turn_permissions`, which preapproves the patch when there are
/// none and the turn or the session holds a grant. Codex skips a target whose directory is writable
/// even when the target is not, and leaves the target's own restriction to the sandbox its patch
/// runs in; this writer runs in no sandbox, so every target must itself be writable.
pub(crate) fn patch_preapproved(
    context: &ToolContext<'_>,
    settings: &CodingThreadSettings,
    sandbox: Option<&ExecSandboxPolicy>,
    paths: &[PathBuf],
    policy_context: &FileSystemSandboxPolicyContext<'_>,
) -> bool {
    let Some(granted) = granted_permissions(context, settings) else {
        return false;
    };
    Writability::new(sandbox, &granted, policy_context)
        .is_ok_and(|writable| paths.iter().all(|path| writable.can_write(path)))
}

/// What a sandbox with grants added lets a patch write: Codex's `PatchPolicyMatcher::can_write_path`.
enum Writability<'a> {
    /// Nothing confines writes.
    Everywhere,
    /// The sandbox's writable roots and the grants' entries, by Codex's precedence.
    Entries {
        entries: Vec<FileSystemSandboxEntry>,
        context: &'a FileSystemSandboxPolicyContext<'a>,
    },
}

impl<'a> Writability<'a> {
    fn new(
        sandbox: Option<&ExecSandboxPolicy>,
        granted: &AdditionalPermissionProfile,
        context: &'a FileSystemSandboxPolicyContext<'a>,
    ) -> Result<Self, String> {
        let Some(sandbox) = sandbox.filter(|sandbox| sandbox.level().needs_backend()) else {
            return Ok(Self::Everywhere);
        };
        let roots = sandbox.writable_roots().iter().map(|root| {
            FileSystemSandboxEntry::new(
                FileSystemPath::Path { path: root.clone() },
                FileSystemAccessMode::Write,
            )
        });
        let materialized =
            materialize_additional_permissions_with_context(granted.clone(), context)?;
        let granted = materialized
            .file_system()
            .map(FileSystemPermissions::entries)
            .unwrap_or_default()
            .iter()
            .flat_map(|entry| match entry.path() {
                // A deny glob names no path to resolve; it is matched as written.
                FileSystemPath::GlobPattern { .. } => vec![entry.clone()],
                _ => resolved_entries(std::slice::from_ref(entry), context)
                    .into_iter()
                    .filter(|(path, _)| !entry.skips_missing_path() || path.exists())
                    .map(|(path, access)| {
                        FileSystemSandboxEntry::new(FileSystemPath::Path { path }, access)
                    })
                    .collect(),
            });
        let entries = roots
            .chain(granted)
            .map(sandbox_namespace_entry)
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self::Entries { entries, context })
    }

    fn can_write(&self, path: &Path) -> bool {
        match self {
            Self::Everywhere => true,
            Self::Entries { entries, context } => sandbox_namespace_path(path).is_ok_and(|path| {
                resolve_access(entries, &path, context) == FileSystemAccessMode::Write
                    && !denied_by_glob(entries, &path, context)
            }),
        }
    }
}

/// Literal roots and deny patterns must match targets in the same sandbox namespace.
fn sandbox_namespace_entry(
    entry: FileSystemSandboxEntry,
) -> Result<FileSystemSandboxEntry, String> {
    let path = match entry.path() {
        FileSystemPath::Path { path } => FileSystemPath::Path {
            path: sandbox_namespace_path(path)?,
        },
        FileSystemPath::GlobPattern { pattern } => {
            #[cfg(target_os = "macos")]
            if std::path::Path::new(pattern)
                .components()
                .nth(1)
                .and_then(|component| component.as_os_str().to_str())
                .is_some_and(|component| component.contains(['*', '?', '[', ']', '{', '}', '\\']))
            {
                return Err(
                    "cannot safely normalize a deny glob with a nonliteral top-level component"
                        .to_owned(),
                );
            }
            let pattern = sandbox_namespace_path(Path::new(pattern))?;
            let pattern = pattern
                .to_str()
                .ok_or("deny glob cannot be represented losslessly")?;
            FileSystemPath::glob(pattern)
        }
        _ => return Ok(entry),
    };
    Ok(entry.with_path(path))
}

/// Whether a deny glob among `entries` matches `path`.
fn denied_by_glob(
    entries: &[FileSystemSandboxEntry],
    path: &Path,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> bool {
    let globs: Vec<_> = entries
        .iter()
        .filter(|entry| matches!(entry.path(), FileSystemPath::GlobPattern { .. }))
        .cloned()
        .collect();
    !globs.is_empty() && crate::permissions::is_read_denied(&globs, path, context)
}

/// `path` as the command sandbox matches it. Seatbelt resolves trusted top-level aliases such as
/// `/var` before matching, so paths are compared in that namespace on macOS, keeping deeper
/// components as written rather than following symbolic links.
// Only the macOS normalization can fail; elsewhere the path is returned as written.
#[cfg_attr(not(target_os = "macos"), allow(clippy::unnecessary_wraps))]
fn sandbox_namespace_path(path: &Path) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        ra_exec::sandbox::seatbelt::normalize_root(path).map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(path.to_path_buf())
    }
}

/// Why a command whose grants deny a path is refused.
const UNENFORCEABLE_DENY: &str = "the permissions granted to this turn or session deny paths the command sandbox cannot exclude, so the command was not run";

/// Why a command whose effective permissions carve a read-only path out of a writable root is refused.
const UNENFORCEABLE_READ_ONLY: &str = "the granted permissions require read-only paths within writable roots that the command sandbox cannot enforce, so the command was not run";

/// `sandbox` with `permissions` added: Codex's `effective_permission_profile` for the executor's
/// sandbox. Each path an entry writes becomes a writable root and each it reads a readable root,
/// resolved in `context`, and a network grant opens the network. An unconfined sandbox is left
/// alone, as Codex leaves an unrestricted or external one.
///
/// The executor's sandboxes can neither exclude a path from a root they open nor deny a read, so
/// permissions that deny any path or require a read-only carveout are refused. As Codex's
/// `get_writable_roots_with_cwd_impl` does, the carveout check resolves precedence across the
/// baseline and the complete grant before deciding whether a read entry restricts a write.
pub(crate) fn granted_sandbox(
    sandbox: ExecSandboxPolicy,
    permissions: &AdditionalPermissionProfile,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> std::result::Result<ExecSandboxPolicy, String> {
    if !sandbox.level().needs_backend() {
        return Ok(sandbox);
    }
    if let Some((profile, cwd)) = sandbox.permission_profile() {
        let permissions =
            materialize_additional_permissions_with_context(permissions.clone(), context)?;
        let permissions = canonical_additional_permissions(&permissions)?;
        let effective = ra_exec::sandbox::policy_transforms::effective_permission_profile(
            profile,
            Some(&permissions),
        );
        let cwd = cwd.to_path_buf();
        return Ok(sandbox.with_permission_profile(effective, cwd));
    }
    let mut sandbox = sandbox;
    if permissions
        .network()
        .is_some_and(NetworkPermissions::grants)
    {
        sandbox = sandbox.with_network(ExecNetworkAccess::Allowed);
    }
    let Some(file_system) = permissions.file_system() else {
        return Ok(sandbox);
    };
    if file_system
        .entries()
        .iter()
        .any(|entry| entry.access() == FileSystemAccessMode::Deny)
    {
        return Err(UNENFORCEABLE_DENY.to_owned());
    }
    let resolved: Vec<_> = file_system
        .entries()
        .iter()
        .flat_map(|entry| {
            resolved_entries(std::slice::from_ref(entry), context)
                .into_iter()
                .filter(|(path, _)| !entry.skips_missing_path() || path.exists())
        })
        .collect();
    if resolved
        .iter()
        .any(|(_, access)| *access == FileSystemAccessMode::Read)
    {
        // Seatbelt resolves trusted top-level aliases before matching. Compare paths in the
        // same namespace, preserving mutable components rather than following deeper symlinks.
        let effective_entries = sandbox
            .writable_roots()
            .iter()
            .map(|path| (path, FileSystemAccessMode::Write))
            .chain(resolved.iter().map(|(path, access)| (path, *access)))
            .map(|(path, access)| {
                Ok(FileSystemSandboxEntry::new(
                    FileSystemPath::Path {
                        path: sandbox_namespace_path(path)?,
                    },
                    access,
                ))
            })
            .collect::<std::result::Result<Vec<_>, String>>()?;
        let effective_paths = resolved_entries(&effective_entries, context);
        for (path, access) in &effective_paths {
            if *access == FileSystemAccessMode::Read
                && effective_paths.iter().any(|(root, access)| {
                    *access == FileSystemAccessMode::Write && path.starts_with(root)
                })
                && resolve_access(&effective_entries, path, context) != FileSystemAccessMode::Write
            {
                return Err(UNENFORCEABLE_READ_ONLY.to_owned());
            }
        }
    }
    for (path, access) in resolved {
        match access {
            FileSystemAccessMode::Write if !sandbox.writable_roots().contains(&path) => {
                sandbox = sandbox.with_writable_root(path);
            }
            FileSystemAccessMode::Read if !sandbox.readable_roots().contains(&path) => {
                sandbox = sandbox.with_readable_root(path);
            }
            _ => {}
        }
    }
    Ok(sandbox)
}

/// Cross the product/native boundary explicitly: product paths are native paths, while the
/// executor-owned profile stores path URIs. No permission entry may disappear in conversion.
fn canonical_additional_permissions(
    permissions: &AdditionalPermissionProfile,
) -> Result<ra_filesystem::models::AdditionalPermissionProfile, String> {
    use ra_filesystem::{models as native, permissions as policy};
    let file_system = permissions
        .file_system()
        .map(
            |file_system| -> Result<ra_filesystem::models::FileSystemPermissions, String> {
                let entries = file_system
                    .entries()
                    .iter()
                    .map(|entry| {
                        let path = match entry.path() {
                            FileSystemPath::Path { path } => policy::FileSystemPath::Path {
                                path: ra_paths::PathUri::from_host_native_path(path)
                                    .map_err(|error| error.to_string())?,
                            },
                            FileSystemPath::GlobPattern { pattern } => {
                                policy::FileSystemPath::GlobPattern {
                                    pattern: pattern.clone(),
                                }
                            }
                            FileSystemPath::Special { value } => policy::FileSystemPath::Special {
                                value: serde_json::from_value(
                                    serde_json::to_value(value)
                                        .map_err(|error| error.to_string())?,
                                )
                                .map_err(|error| error.to_string())?,
                            },
                        };
                        let access = match entry.access() {
                            FileSystemAccessMode::Read => policy::FileSystemAccessMode::Read,
                            FileSystemAccessMode::Write => policy::FileSystemAccessMode::Write,
                            FileSystemAccessMode::Deny => policy::FileSystemAccessMode::Deny,
                        };
                        let mut native = policy::FileSystemSandboxEntry::new(path, access);
                        if entry.skips_missing_path() {
                            native.missing_path_behavior =
                                Some(policy::FileSystemSandboxEntryMissingPathBehavior::Skip);
                        }
                        Ok(native)
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(native::FileSystemPermissions {
                    entries,
                    glob_scan_max_depth: file_system.glob_scan_max_depth(),
                })
            },
        )
        .transpose()?;
    Ok(native::AdditionalPermissionProfile {
        network: permissions
            .network()
            .map(|network| native::NetworkPermissions {
                enabled: network.enabled(),
            }),
        file_system,
    })
}
