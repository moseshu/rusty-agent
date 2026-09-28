//! The half of the workspace path decision that needs a real filesystem.
//!
//! `ra_core::sandbox::WorkspacePathPolicy` decides where a path points by reading the text of it.
//! That is the right answer for a remote sandbox, where the SDK has no filesystem to consult, and
//! it is exactly half the answer for a workspace that is a directory on this host: a symlink inside
//! the workspace can point outside it, and no amount of reading the text will say so.
//!
//! This module is the other half. It resolves a path against the filesystem first and then asks the
//! same containment question, which is what the reference does when it is handed
//! `resolve_symlinks=True`. Both halves are kept, not merged — the lexical one still runs first for
//! a relative path, so a path that climbs out of the workspace is refused for climbing out rather
//! than for wherever it happened to land.
//!
//! # Resolution is not "make absolute"
//!
//! [`resolve_without_strictness`] follows symlinks component by component and leaves the components
//! that do not exist alone, which is what `Path.resolve(strict=False)` does in the reference and
//! what a write to a not-yet-created file needs. `std::fs::canonicalize` cannot do it: it fails
//! outright when the path is not already there.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use ra_core::sandbox::{
    ErrorCode, OpName, PathGrantError, PosixPath, SandboxError, SandboxPathGrant, SessionPath,
    WorkspacePathPolicy, windows_absolute_path,
};

/// One step of a path, as resolution consumes it.
enum Segment {
    /// The filesystem root, which restarts the resolved path.
    Root,
    /// A climb to the parent of whatever has been resolved so far.
    Parent,
    /// A named component.
    Normal(OsString),
    /// Records the resolved target before processing the remaining caller path.
    FinishLink(PathBuf),
}

/// Splits a path into the steps resolution takes.
fn segments(path: &Path) -> Vec<Segment> {
    path.components()
        .filter_map(|component| match component {
            Component::RootDir => Some(Segment::Root),
            Component::ParentDir => Some(Segment::Parent),
            Component::Normal(name) => Some(Segment::Normal(name.to_owned())),
            // A bare `.` contributes nothing; a Windows prefix cannot occur on the hosts this
            // backend runs on, and treating one as a name would invent a directory.
            Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// Resolves symlinks as far as the filesystem goes, and leaves the rest of the path alone.
///
/// The port of `Path.resolve(strict=False)`. Components are consumed left to right: a symlink is
/// replaced by its target and the target's own components are resolved in turn, `..` climbs the
/// path resolved *so far* rather than the text that was written, and a component that is not there
/// is simply appended. A relative path is measured from the process's working directory, as the
/// reference measures it. Completed link targets are cached; a link encountered while its own
/// target is still resolving is a cycle, not a reason to return an unresolved authorized path.
///
/// # Errors
///
/// Returns [`ErrorCode::SandboxConfigInvalid`] for a symlink cycle or an IO error that prevents
/// resolution. Missing components remain valid so callers can create new files.
pub fn resolve_without_strictness(path: &Path) -> Result<PathBuf, SandboxError> {
    let mut resolved = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        std::env::current_dir().map_err(|error| resolution_failure(path, &error.to_string()))?
    };
    let mut queue: VecDeque<Segment> = segments(path).into();
    let mut links: BTreeMap<PathBuf, Option<PathBuf>> = BTreeMap::new();

    while let Some(segment) = queue.pop_front() {
        match segment {
            Segment::Root => resolved = PathBuf::from("/"),
            Segment::Parent => {
                resolved.pop();
            }
            Segment::FinishLink(link) => {
                links.insert(link, Some(resolved.clone()));
            }
            Segment::Normal(name) => {
                let candidate = resolved.join(&name);
                if let Some(target) = links.get(&candidate) {
                    resolved = target
                        .clone()
                        .ok_or_else(|| resolution_failure(&candidate, "symlink loop"))?;
                    continue;
                }
                match std::fs::read_link(&candidate) {
                    Ok(target) => {
                        links.insert(candidate.clone(), None);
                        queue.push_front(Segment::FinishLink(candidate));
                        // A relative target is measured from the directory holding the link, which
                        // is what `resolved` already is; an absolute one starts over.
                        if target.is_absolute() {
                            resolved = PathBuf::from("/");
                        }
                        for step in segments(&target).into_iter().rev() {
                            queue.push_front(step);
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::NotADirectory
                                | std::io::ErrorKind::InvalidInput
                        ) =>
                    {
                        resolved = candidate;
                    }
                    Err(error) => return Err(resolution_failure(&candidate, &error.to_string())),
                }
            }
        }
    }
    Ok(resolved)
}

/// Reports a path that cannot be fully resolved for an authorization decision.
fn resolution_failure(path: &Path, reason: &str) -> SandboxError {
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        OpName::Materialize,
        format!("failed to resolve host path {}: {reason}", path.display()),
    )
    .with_context("path", path.to_string_lossy().as_ref())
    .with_context("reason", reason)
}

/// Expands a leading `~` against this process's home directory.
///
/// Only a leading one, and only when it is the whole first component: `~/bin` is a home-relative
/// path, `~user/bin` names somebody else's home and is left alone, as the reference leaves it alone
/// when the user is unknown.
#[must_use]
pub fn expand_user(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        return PathBuf::from(path);
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    }
}

/// Whether a native path is the root of its filesystem.
fn is_filesystem_root(path: &Path) -> bool {
    path.is_absolute() && path.parent().is_none()
}

/// Whether `path` is `root` or something under it.
fn is_under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// Resolves the host directory a grant draws from, and re-checks that it is not everything.
///
/// The check runs twice on purpose. A grant is validated when it is written, but the path it names
/// can be a symlink, and a symlink repointed at `/` between validation and use would hand out the
/// whole filesystem under a name that passed.
///
/// # Errors
///
/// Returns [`ErrorCode::SandboxConfigInvalid`] when the source is relative, resolves to the
/// filesystem root, or cannot be resolved.
pub fn sandbox_path_grant_host_path(grant: &SandboxPathGrant) -> Result<PathBuf, SandboxError> {
    let raw = grant.host_path().unwrap_or_else(|| grant.path());
    let native = PathBuf::from(raw);
    if grant.host_path().is_some() && !native.is_absolute() {
        return Err(invalid_grant(PathGrantError::HostPathNotAbsolute));
    }
    if is_filesystem_root(&native) {
        return Err(invalid_grant(PathGrantError::FilesystemRoot));
    }
    let resolved = resolve_without_strictness(&native)?;
    if is_filesystem_root(&resolved) {
        return Err(invalid_grant(PathGrantError::ResolvedFilesystemRoot));
    }
    Ok(resolved)
}

/// Decides which host paths a session may reach, after following symlinks.
///
/// Wraps the lexical policy rather than replacing it: a relative path is still anchored under the
/// workspace root and still refused for climbing out of it, and only then is the result resolved
/// and asked the containment question again.
#[derive(Debug, Clone)]
pub struct HostWorkspacePaths {
    policy: WorkspacePathPolicy,
    root: PathBuf,
    resolved_root: PathBuf,
}

impl HostWorkspacePaths {
    /// Opens a policy over one workspace root on this host.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when the root is not absolute or cannot be resolved.
    pub fn new(root: &str, extra_path_grants: Vec<SandboxPathGrant>) -> Result<Self, SandboxError> {
        let policy = WorkspacePathPolicy::new(root, extra_path_grants)
            .map_err(|error| resolution_failure(Path::new(root), &error.to_string()))?;
        let root = PathBuf::from(root);
        let resolved_root = resolve_without_strictness(&root)?;
        Ok(Self {
            policy,
            root,
            resolved_root,
        })
    }

    /// The workspace root as it was configured, without symlinks followed.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The workspace root with symlinks followed, which is what containment is measured against.
    #[must_use]
    pub fn resolved_root(&self) -> &Path {
        &self.resolved_root
    }

    /// The lexical policy underneath, for the decisions that do not need a filesystem.
    #[must_use]
    pub const fn policy(&self) -> &WorkspacePathPolicy {
        &self.policy
    }

    /// Validates a path against the workspace and its grants, following symlinks.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidManifestPath`] for a path that resolves outside the workspace and
    /// outside every grant, and [`ErrorCode::WorkspaceArchiveWriteError`] when `for_write` is set
    /// and the grant that allowed it only allows reading.
    pub fn normalize_path(&self, path: &str, for_write: bool) -> Result<PathBuf, SandboxError> {
        let (resolved, grant) = self.resolved_path_and_grant(path)?;
        if for_write
            && let Some(grant) = grant
            && grant.is_read_only()
        {
            return Err(
                SandboxError::workspace_archive_write(&resolved.to_string_lossy())
                    .with_context("reason", "read_only_extra_path_grant")
                    .with_context("grant_path", grant.path()),
            );
        }
        Ok(resolved)
    }

    /// Resolves a path and names the grant that allowed it, if it needed one.
    fn resolved_path_and_grant(
        &self,
        path: &str,
    ) -> Result<(PathBuf, Option<&SandboxPathGrant>), SandboxError> {
        if let Some(windows_path) = windows_absolute_path(path) {
            // The hosts this backend runs on do not read a drive path as absolute, so accepting it
            // would anchor `C:/secret` under the workspace as though it were a relative path.
            return Err(invalid_manifest_path(&windows_path, true));
        }

        let original = PathBuf::from(path);
        let resolved = if original.is_absolute() {
            resolve_without_strictness(&original)?
        } else {
            // Lexical first: a relative path has to land inside the workspace as written, before
            // any symlink gets a say. Skipping this would let `../../etc/passwd` be judged on where
            // it resolved to rather than on the fact that it climbed out.
            //
            // Read as written, as the reference reads the path object it built: a backslash is
            // part of a name here, as it is to the host filesystem this resolves against.
            let absolute = self
                .policy
                .absolute_workspace_path(SessionPath::Posix(&PosixPath::new(path)))?;
            resolve_without_strictness(Path::new(absolute.as_str()))?
        };

        if is_under(&resolved, &self.resolved_root) {
            return Ok((resolved, None));
        }
        match self.matching_grant(&resolved)? {
            Some(grant) => Ok((resolved, Some(grant))),
            None => Err(invalid_manifest_path(path, original.is_absolute())),
        }
    }

    /// The most specific grant whose resolved source contains this path.
    fn matching_grant(&self, path: &Path) -> Result<Option<&SandboxPathGrant>, SandboxError> {
        let mut best: Option<(&SandboxPathGrant, usize)> = None;
        for grant in self.policy.extra_path_grants() {
            let root = sandbox_path_grant_host_path(grant)?;
            if !is_under(path, &root) {
                continue;
            }
            let depth = root.components().count();
            // Nested grants are allowed and the innermost decides: a read-only grant inside a
            // writable one is a narrowing, so the longest match wins rather than the first.
            if best.is_none_or(|(_, best_depth)| depth > best_depth) {
                best = Some((grant, depth));
            }
        }
        Ok(best.map(|(grant, _)| grant))
    }
}

/// Refuses a path that is outside the workspace, or that had to climb out to get there.
///
/// The code, context keys and wording match the lexical policy's refusal: a caller that handles one
/// handles both, and a path refused for the same reason should not read differently depending on
/// which half of the check caught it.
fn invalid_manifest_path(path: &str, absolute: bool) -> SandboxError {
    let rendered = PosixPath::new(path);
    let reason = if absolute { "absolute" } else { "escape_root" };
    let message = if absolute {
        format!("manifest path must be relative: {rendered}")
    } else {
        format!("manifest path must not escape root: {rendered}")
    };
    SandboxError::new(ErrorCode::InvalidManifestPath, OpName::Materialize, message)
        .with_context("rel", rendered.as_str())
        .with_context("reason", reason)
}

/// Reports a grant whose host source stopped being usable.
fn invalid_grant(error: PathGrantError) -> SandboxError {
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        OpName::Materialize,
        error.to_string(),
    )
}
