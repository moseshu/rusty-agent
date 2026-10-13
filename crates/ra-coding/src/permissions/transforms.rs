//! Checking, resolving, intersecting and merging permission profiles: Codex's
//! `sandboxing/src/policy_transforms.rs`, with the parts of `protocol/src/permissions.rs` they rely
//! on — a restricted policy's `resolved_entries` and `resolve_access`, its deny-read matcher, and
//! `file_system_root`.

use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use globset::{Candidate, GlobBuilder, GlobMatcher};

use super::{
    AdditionalPermissionProfile, FileSystemAccessMode, FileSystemPath, FileSystemPermissions,
    FileSystemSandboxEntry, FileSystemSpecialPath, NetworkPermissions, path,
};

/// Where a profile is interpreted: Codex's `FileSystemSandboxPolicyContext`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct FileSystemSandboxPolicyContext<'a> {
    cwd: &'a Path,
    workspace_roots: &'a [PathBuf],
    user_home_dir: Option<&'a Path>,
    temporary_directories: Option<&'a [PathBuf]>,
}

impl<'a> FileSystemSandboxPolicyContext<'a> {
    /// Interpreted in `cwd`, whose project roots are `workspace_roots`; no home or temporary
    /// directory is known.
    #[must_use]
    pub const fn new(cwd: &'a Path, workspace_roots: &'a [PathBuf]) -> Self {
        Self {
            cwd,
            workspace_roots,
            user_home_dir: None,
            temporary_directories: None,
        }
    }

    /// Sets the home directory a `~` names.
    #[must_use]
    pub const fn with_user_home_dir(mut self, user_home_dir: &'a Path) -> Self {
        self.user_home_dir = Some(user_home_dir);
        self
    }

    /// Sets the temporary directories `tmpdir` names.
    #[must_use]
    pub const fn with_temporary_directories(
        mut self,
        temporary_directories: &'a [PathBuf],
    ) -> Self {
        self.temporary_directories = Some(temporary_directories);
        self
    }

    /// The directory relative paths resolve against.
    #[must_use]
    pub const fn cwd(&self) -> &'a Path {
        self.cwd
    }
}

/// Checks a profile without knowing where it applies: Codex's `normalize_additional_permissions`.
/// An empty part is dropped, duplicate entries are kept once, and a glob may only deny.
///
/// # Errors
///
/// Returns Codex's refusal of a glob that reads or writes.
pub fn normalize_additional_permissions(
    additional_permissions: AdditionalPermissionProfile,
) -> Result<AdditionalPermissionProfile, String> {
    let network = additional_permissions
        .network
        .filter(|network| !network.is_empty());
    let file_system = match additional_permissions.file_system {
        Some(file_system) => {
            let mut entries = Vec::with_capacity(file_system.entries.len());
            for entry in file_system.entries {
                if matches!(&entry.path, FileSystemPath::GlobPattern { .. })
                    && entry.access != FileSystemAccessMode::Deny
                {
                    return Err(
                        "glob file system permissions only support deny-read entries".to_owned(),
                    );
                }
                if !entries.contains(&entry) {
                    entries.push(entry);
                }
            }
            let file_system = FileSystemPermissions {
                entries,
                glob_scan_max_depth: file_system.glob_scan_max_depth,
            };
            (!file_system.is_empty()).then_some(file_system)
        }
        None => None,
    };
    Ok(AdditionalPermissionProfile {
        network,
        file_system,
    })
}

/// [`normalize_additional_permissions`], and every path must be one `context` can interpret:
/// Codex's `normalize_additional_permissions_with_context`.
///
/// # Errors
///
/// Returns the refusal of a glob that reads or writes, or Codex's refusal of a path that does not
/// match the context's.
pub fn normalize_additional_permissions_with_context(
    additional_permissions: AdditionalPermissionProfile,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Result<AdditionalPermissionProfile, String> {
    let normalized = normalize_additional_permissions(additional_permissions)?;
    if let Some(file_system) = normalized.file_system.as_ref() {
        for entry in &file_system.entries {
            let FileSystemPath::Path { path } = &entry.path else {
                continue;
            };
            if path::normalize(path).as_deref() != Some(path.as_path()) {
                return Err(format!(
                    "permission path `{}` does not match executor cwd `{}`",
                    path.display(),
                    context.cwd.display()
                ));
            }
        }
    }
    Ok(normalized)
}

/// Resolves the entries that depend on where a profile applies without narrowing what it gives:
/// Codex's `materialize_additional_permissions_with_context`.
///
/// # Errors
///
/// Returns Codex's refusal when a deny entry cannot be resolved, or the refusals of
/// [`normalize_additional_permissions_with_context`].
pub fn materialize_additional_permissions_with_context(
    mut additional_permissions: AdditionalPermissionProfile,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Result<AdditionalPermissionProfile, String> {
    if let Some(file_system) = additional_permissions.file_system.as_mut() {
        file_system.entries = materialize_context_dependent_entries(&file_system.entries, context)
            .ok_or_else(|| {
                format!(
                    "unable to resolve permission path in `{}`",
                    context.cwd.display()
                )
            })?;
    }
    normalize_additional_permissions_with_context(additional_permissions, context)
}

/// `permissions` added to `base`: Codex's `merge_permission_profiles`. Network access is given if
/// either gives it; file system entries are combined, each kept once.
#[must_use]
pub fn merge_permission_profiles(
    base: Option<&AdditionalPermissionProfile>,
    permissions: Option<&AdditionalPermissionProfile>,
) -> Option<AdditionalPermissionProfile> {
    let Some(permissions) = permissions else {
        return base.cloned();
    };
    let Some(base) = base else {
        return Some(permissions.clone()).filter(|permissions| !permissions.is_empty());
    };
    let network = (base
        .network
        .as_ref()
        .is_some_and(NetworkPermissions::grants)
        || permissions
            .network
            .as_ref()
            .is_some_and(NetworkPermissions::grants))
    .then(|| NetworkPermissions::new(Some(true)));
    let file_system = match (base.file_system.as_ref(), permissions.file_system.as_ref()) {
        (Some(base), Some(permissions)) => Some(FileSystemPermissions {
            entries: merge_permission_entries(&base.entries, &permissions.entries),
            glob_scan_max_depth: merge_glob_scan_max_depth(
                &base.entries,
                base.glob_scan_max_depth.map(usize::from),
                &permissions.entries,
                permissions.glob_scan_max_depth.map(usize::from),
            )
            .and_then(NonZeroUsize::new),
        })
        .filter(|file_system| !file_system.is_empty()),
        (Some(base), None) => Some(base.clone()),
        (None, Some(permissions)) => Some(permissions.clone()),
        (None, None) => None,
    };
    Some(AdditionalPermissionProfile {
        network,
        file_system,
    })
    .filter(|permissions| !permissions.is_empty())
}

/// The part of `granted` that `requested` covers: Codex's
/// `intersect_permission_profiles_with_context`.
///
/// A granted entry is kept when the request gives at least its access to its path and no deny the
/// request makes covers it; the deny entries of either side that constrain what is kept are kept
/// with it. Entries are resolved against `context`, so the result applies wherever it is used
/// later. Network access is given only if both ask for and give it.
#[must_use]
pub fn intersect_permission_profiles_with_context(
    requested: AdditionalPermissionProfile,
    granted: AdditionalPermissionProfile,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> AdditionalPermissionProfile {
    let file_system = requested
        .file_system
        .and_then(|requested_file_system| {
            let granted_file_system = granted.file_system.unwrap_or_default();
            let requested_entries =
                materialize_context_dependent_entries(&requested_file_system.entries, context)?;
            let granted_entries =
                materialize_context_dependent_entries(&granted_file_system.entries, context)?;
            let requested_read_deny_matcher =
                ReadDenyMatcher::from_context(&requested_entries, context);
            let mut accepted_entries = Vec::new();
            for entry in granted_entries.iter().filter(|entry| {
                granted_file_system_entry_within_request(
                    &requested_file_system,
                    &requested_entries,
                    requested_read_deny_matcher.as_ref(),
                    entry,
                    context,
                )
            }) {
                if !accepted_entries.contains(entry) {
                    accepted_entries.push(entry.clone());
                }
            }
            let mut entries = accepted_entries.clone();
            let requested_retained_deny_entries = retain_constraining_deny_entries(
                &requested_entries,
                &accepted_entries,
                context,
                &mut entries,
            );
            let granted_retained_deny_entries = retain_constraining_deny_entries(
                &granted_entries,
                &accepted_entries,
                context,
                &mut entries,
            );
            Some(FileSystemPermissions {
                glob_scan_max_depth: merge_glob_scan_max_depth(
                    &requested_retained_deny_entries,
                    requested_file_system.glob_scan_max_depth.map(usize::from),
                    &granted_retained_deny_entries,
                    granted_file_system.glob_scan_max_depth.map(usize::from),
                )
                .and_then(NonZeroUsize::new),
                entries,
            })
        })
        .filter(|file_system| !file_system.is_empty());
    let network = (requested
        .network
        .as_ref()
        .is_some_and(NetworkPermissions::grants)
        && granted
            .network
            .as_ref()
            .is_some_and(NetworkPermissions::grants))
    .then(|| NetworkPermissions::new(Some(true)));
    AdditionalPermissionProfile {
        network,
        file_system,
    }
}

fn merge_glob_scan_max_depth(
    left_entries: &[FileSystemSandboxEntry],
    left_depth: Option<usize>,
    right_entries: &[FileSystemSandboxEntry],
    right_depth: Option<usize>,
) -> Option<usize> {
    let left_depth = effective_glob_scan_depth(left_entries, left_depth);
    let right_depth = effective_glob_scan_depth(right_entries, right_depth);
    match (left_depth, right_depth) {
        (Some(GlobScanDepth::Unbounded), _)
        | (_, Some(GlobScanDepth::Unbounded))
        | (None, None) => None,
        (Some(GlobScanDepth::Bounded(left)), Some(GlobScanDepth::Bounded(right))) => {
            Some(left.max(right))
        }
        (Some(GlobScanDepth::Bounded(depth)), None)
        | (None, Some(GlobScanDepth::Bounded(depth))) => Some(depth),
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

fn granted_file_system_entry_within_request(
    requested: &FileSystemPermissions,
    requested_entries: &[FileSystemSandboxEntry],
    requested_read_deny_matcher: Option<&ReadDenyMatcher>,
    granted_entry: &FileSystemSandboxEntry,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> bool {
    if !granted_entry.access.can_read() {
        return false;
    }
    if let Some(path) = resolve_permission_path(&granted_entry.path, context) {
        if requested_read_deny_matcher.is_some_and(|matcher| matcher.is_read_denied(&path)) {
            return false;
        }
        return access_covers(
            resolve_access(requested_entries, &path, context),
            granted_entry.access,
        );
    }
    requested.entries.iter().any(|requested_entry| {
        access_covers(requested_entry.access, granted_entry.access)
            && requested_entry.path == granted_entry.path
    })
}

fn retain_constraining_deny_entries(
    source_entries: &[FileSystemSandboxEntry],
    accepted_entries: &[FileSystemSandboxEntry],
    context: &FileSystemSandboxPolicyContext<'_>,
    output_entries: &mut Vec<FileSystemSandboxEntry>,
) -> Vec<FileSystemSandboxEntry> {
    let mut retained_entries = Vec::new();
    for entry in source_entries
        .iter()
        .filter(|entry| entry.access == FileSystemAccessMode::Deny)
    {
        if !deny_entry_constrains_accepted_grant(entry, accepted_entries, context) {
            continue;
        }
        if !output_entries.contains(entry) {
            output_entries.push(entry.clone());
        }
        retained_entries.push(entry.clone());
    }
    retained_entries
}

fn deny_entry_constrains_accepted_grant(
    deny_entry: &FileSystemSandboxEntry,
    accepted_entries: &[FileSystemSandboxEntry],
    context: &FileSystemSandboxPolicyContext<'_>,
) -> bool {
    accepted_entries
        .iter()
        .filter(|entry| entry.access.can_read())
        .any(|entry| {
            if is_root_entry(entry) {
                return true;
            }
            let Some(grant_path) = resolve_permission_path(&entry.path, context) else {
                return true;
            };
            match &deny_entry.path {
                FileSystemPath::GlobPattern { pattern } => {
                    glob_static_prefix_path(pattern, context)
                        .is_none_or(|prefix| path::overlaps(&prefix, &grant_path))
                }
                FileSystemPath::Path { .. } | FileSystemPath::Special { .. } => {
                    resolve_permission_path(&deny_entry.path, context)
                        .is_none_or(|deny_path| path::overlaps(&deny_path, &grant_path))
                }
            }
        })
}

/// The directory below which everything `pattern` matches lies: Codex's `glob_static_prefix_path`.
fn glob_static_prefix_path(
    pattern: &str,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Option<PathBuf> {
    let (prefix, wildcard_in_segment) = match pattern.find(['*', '?', '[', ']']) {
        Some(0) => return None,
        Some(index) => {
            let prefix = &pattern[..index];
            (prefix, !prefix.ends_with('/'))
        }
        None => (pattern, false),
    };
    let prefix = path::join(context.cwd, prefix)?;
    if wildcard_in_segment {
        prefix.parent().map(Path::to_path_buf)
    } else {
        Some(prefix)
    }
}

const fn access_covers(requested: FileSystemAccessMode, granted: FileSystemAccessMode) -> bool {
    match granted {
        FileSystemAccessMode::Read => requested.can_read(),
        FileSystemAccessMode::Write => requested.can_write(),
        FileSystemAccessMode::Deny => false,
    }
}

const fn is_root_entry(entry: &FileSystemSandboxEntry) -> bool {
    matches!(
        &entry.path,
        FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        }
    )
}

/// A glob resolved against the context's cwd, or its home for a `~` pattern: Codex's
/// `materialize_cwd_dependent_entry`. Native literal paths are normalized and validated because
/// `PathBuf` does not carry Codex's path invariants; special paths are left as they are.
fn materialize_cwd_dependent_entry(
    entry: &FileSystemSandboxEntry,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Option<FileSystemSandboxEntry> {
    match &entry.path {
        FileSystemPath::GlobPattern { pattern } => {
            let home_relative = pattern
                .strip_prefix("~/")
                .or_else(|| (pattern == "~").then_some(""));
            let (root, pattern) = match home_relative {
                Some(suffix) => (context.user_home_dir?, suffix.trim_start_matches('/')),
                None => (context.cwd, pattern.as_str()),
            };
            let path = path::join(root, pattern)?;
            Some(entry.with_path(FileSystemPath::GlobPattern {
                pattern: path.to_string_lossy().into_owned(),
            }))
        }
        FileSystemPath::Path { path } => {
            path::normalize(path).map(|path| entry.with_path(FileSystemPath::Path { path }))
        }
        FileSystemPath::Special { .. } => Some(entry.clone()),
    }
}

/// Where a path entry points in `context`, if it names one place: Codex's
/// `resolve_permission_path`.
fn resolve_permission_path(
    file_system_path: &FileSystemPath,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Option<PathBuf> {
    match file_system_path {
        FileSystemPath::Path { path } => path::normalize(path),
        FileSystemPath::GlobPattern { .. } => None,
        FileSystemPath::Special { value } => match value {
            FileSystemSpecialPath::Root => path::file_system_root(context.cwd),
            FileSystemSpecialPath::ProjectRoots { subpath } => {
                let root = context.workspace_roots.first()?;
                match subpath {
                    Some(subpath) => path::join(root, subpath),
                    None => path::normalize(root),
                }
            }
            FileSystemSpecialPath::Tmpdir => {
                path::normalize(context.temporary_directories?.first()?)
            }
            FileSystemSpecialPath::SlashTmp => path::join(context.cwd, "/tmp"),
            FileSystemSpecialPath::Minimal | FileSystemSpecialPath::Unknown { .. } => None,
        },
    }
}

/// Entries resolved against `context` so they mean the same wherever they are used later:
/// Codex's `materialize_context_dependent_entries`. `None` if a deny entry cannot be resolved,
/// since dropping it would widen what the entries give.
fn materialize_context_dependent_entries(
    entries: &[FileSystemSandboxEntry],
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Option<Vec<FileSystemSandboxEntry>> {
    let mut materialized = Vec::new();
    for entry in entries {
        match &entry.path {
            FileSystemPath::Special {
                value: FileSystemSpecialPath::ProjectRoots { .. },
            } => {
                let mut resolved = Vec::new();
                for root in context.workspace_roots {
                    let mut root_context = *context;
                    root_context.workspace_roots = std::slice::from_ref(root);
                    let Some(path) = resolve_permission_path(&entry.path, &root_context) else {
                        if entry.access == FileSystemAccessMode::Deny {
                            return None;
                        }
                        continue;
                    };
                    resolved.push(entry.with_path(FileSystemPath::Path { path }));
                }
                if entry.access == FileSystemAccessMode::Deny && resolved.is_empty() {
                    return None;
                }
                materialized.extend(resolved);
            }
            FileSystemPath::Special {
                value: FileSystemSpecialPath::Tmpdir,
            } => {
                let Some(temporary_directories) = context.temporary_directories else {
                    if entry.access == FileSystemAccessMode::Deny {
                        return None;
                    }
                    materialized.push(entry.clone());
                    continue;
                };
                for root in temporary_directories {
                    let Some(path) = path::normalize(root) else {
                        if entry.access == FileSystemAccessMode::Deny {
                            return None;
                        }
                        continue;
                    };
                    materialized.push(entry.with_path(FileSystemPath::Path { path }));
                }
            }
            FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            } => {
                resolve_permission_path(&entry.path, context)?;
                materialized.push(entry.clone());
            }
            _ => {
                let Some(entry) = materialize_cwd_dependent_entry(entry, context) else {
                    if entry.access == FileSystemAccessMode::Deny {
                        return None;
                    }
                    continue;
                };
                materialized.push(entry);
            }
        }
    }
    Some(materialized)
}

fn merge_permission_entries(
    base: &[FileSystemSandboxEntry],
    permissions: &[FileSystemSandboxEntry],
) -> Vec<FileSystemSandboxEntry> {
    let mut merged = Vec::with_capacity(base.len() + permissions.len());
    for entry in base.iter().chain(permissions) {
        if !merged.contains(entry) {
            merged.push(entry.clone());
        }
    }
    merged
}

/// Every place `entries` name in `context`, with its access: a restricted
/// `FileSystemSandboxPolicy`'s `resolved_entries`.
fn resolved_entries(
    entries: &[FileSystemSandboxEntry],
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Vec<(PathBuf, FileSystemAccessMode)> {
    entries
        .iter()
        .flat_map(|entry| {
            let paths: Vec<PathBuf> = match &entry.path {
                FileSystemPath::Path { path } => path::normalize(path).into_iter().collect(),
                FileSystemPath::GlobPattern { .. } => Vec::new(),
                FileSystemPath::Special { value } => match value {
                    FileSystemSpecialPath::Root => {
                        path::file_system_root(context.cwd).into_iter().collect()
                    }
                    FileSystemSpecialPath::ProjectRoots { subpath } => context
                        .workspace_roots
                        .iter()
                        .filter_map(|root| match subpath {
                            Some(subpath) => path::join(root, subpath),
                            None => path::normalize(root),
                        })
                        .collect(),
                    FileSystemSpecialPath::Tmpdir => context
                        .temporary_directories
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|root| path::normalize(root))
                        .collect(),
                    FileSystemSpecialPath::SlashTmp => {
                        path::join(context.cwd, "/tmp").into_iter().collect()
                    }
                    FileSystemSpecialPath::Minimal | FileSystemSpecialPath::Unknown { .. } => {
                        Vec::new()
                    }
                },
            };
            paths.into_iter().map(move |path| (path, entry.access))
        })
        .collect()
}

/// The access a restricted policy of `entries` gives `path`: its `resolve_access`. The most
/// specific entry containing the path decides, a later access breaking a tie; a path no entry
/// contains is denied.
fn resolve_access(
    entries: &[FileSystemSandboxEntry],
    target: &Path,
    context: &FileSystemSandboxPolicyContext<'_>,
) -> FileSystemAccessMode {
    if path::normalize(context.cwd).is_none() {
        return FileSystemAccessMode::Deny;
    }
    let Some(target) = path::normalize(target) else {
        return FileSystemAccessMode::Deny;
    };
    resolved_entries(entries, context)
        .into_iter()
        .filter(|(root, _)| target.starts_with(root))
        .max_by_key(|(root, access)| (path::lexical_depth(root), *access))
        .map_or(FileSystemAccessMode::Deny, |(_, access)| access)
}

/// What a restricted policy's deny entries keep from being read: Codex's `ReadDenyMatcher`, built
/// as its `from_context` builds it, failing closed on a pattern it cannot use.
struct ReadDenyMatcher {
    denied_roots: Vec<PathBuf>,
    deny_read_matchers: Vec<GlobMatcher>,
    invalid_pattern: bool,
}

impl ReadDenyMatcher {
    /// The matcher for `entries`, or `None` if none of them denies.
    fn from_context(
        entries: &[FileSystemSandboxEntry],
        context: &FileSystemSandboxPolicyContext<'_>,
    ) -> Option<Self> {
        if !entries
            .iter()
            .any(|entry| entry.access == FileSystemAccessMode::Deny)
        {
            return None;
        }
        let file_system_root = path::file_system_root(context.cwd);
        let denied_roots = resolved_entries(entries, context)
            .into_iter()
            .filter(|(root, access)| {
                *access == FileSystemAccessMode::Deny && file_system_root.as_ref() != Some(root)
            })
            .map(|(root, _)| root)
            .collect();
        let Ok(patterns) = deny_read_globs(entries, context) else {
            return Some(Self {
                denied_roots,
                deny_read_matchers: Vec::new(),
                invalid_pattern: true,
            });
        };
        let mut deny_read_matchers = Vec::new();
        let mut invalid_pattern = false;
        for pattern in patterns {
            match build_glob_matcher(&pattern) {
                Ok(matcher) => deny_read_matchers.push(matcher),
                Err(_) => invalid_pattern = true,
            }
        }
        Some(Self {
            denied_roots,
            deny_read_matchers,
            invalid_pattern,
        })
    }

    /// Whether `target` may not be read: Codex's `matches_prepared_read_deny`.
    fn is_read_denied(&self, target: &Path) -> bool {
        self.invalid_pattern
            || self
                .denied_roots
                .iter()
                .any(|root| target.starts_with(root))
            || self
                .deny_read_matchers
                .iter()
                .any(|matcher| matcher.is_match_candidate(&Candidate::new(target)))
    }
}

/// The deny globs of `entries`, made absolute against the cwd, or the home directory for a `~/`
/// pattern: Codex's `deny_read_globs`.
fn deny_read_globs(
    entries: &[FileSystemSandboxEntry],
    context: &FileSystemSandboxPolicyContext<'_>,
) -> Result<Vec<String>, String> {
    entries
        .iter()
        .filter(|entry| entry.access == FileSystemAccessMode::Deny)
        .filter_map(|entry| {
            let FileSystemPath::GlobPattern { pattern } = &entry.path else {
                return None;
            };
            let (root, pattern) = match pattern.strip_prefix("~/") {
                Some(suffix) => match context.user_home_dir {
                    Some(home) => (home, suffix.trim_start_matches('/')),
                    None => {
                        return Some(Err(format!(
                            "unable to resolve deny-read glob pattern `{pattern}` without executor home"
                        )));
                    }
                },
                None => (context.cwd, pattern.as_str()),
            };
            Some(
                path::join(root, pattern)
                    .map(|path| path.to_string_lossy().into_owned())
                    .ok_or_else(|| format!("unable to resolve deny-read glob pattern `{pattern}`")),
            )
        })
        .collect()
}

/// Codex's `build_glob_matcher` for POSIX paths: `*` and `?` stay within one component, an
/// unclosed `[` is a literal, and a backslash escapes.
fn build_glob_matcher(pattern: &str) -> Result<GlobMatcher, String> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .allow_unclosed_class(true)
        .backslash_escape(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| error.to_string())
}
