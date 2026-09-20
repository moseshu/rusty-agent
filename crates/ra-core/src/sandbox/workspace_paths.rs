//! Where a sandbox path is allowed to point, and how it is rendered.
//!
//! Three things live here, and they answer three different questions:
//!
//! - [`PosixPath`] is the path algebra everything else is written in. Sandbox paths are POSIX
//!   whatever host the process runs on, so they cannot be [`std::path::Path`]: on Windows that type
//!   would read `C:/x` as absolute and `a\b` as two components, and a workspace check written
//!   against it would mean something different depending on where the SDK happened to run.
//! - [`SandboxWorkspaceScope`] anchors relative paths under a run's working directory. It changes
//!   only where a relative path is measured from — never what may be reached.
//! - [`SandboxPathGrant`] and [`WorkspacePathPolicy`] are the access decision: the workspace root is
//!   reachable, a grant widens that to one more subtree, and everything else is refused.
//!
//! # A Windows drive path is refused, not reinterpreted
//!
//! `C:/secret.txt` is not a POSIX absolute path, so coercing it would leave a relative-looking
//! `C:/secret.txt` that a workspace check would happily anchor under the root. Every entry point
//! therefore tests for drive-absolute syntax first and refuses it as absolute, which is the same
//! answer the reference gives and the reason it carries a Windows path type alongside the POSIX one.
//!
//! # What is here and what is not
//!
//! The decisions are all lexical, which is what lets them live in a crate that performs no IO. The
//! reference's `WorkspacePathPolicy` has a second half that resolves symlinks against a real host
//! directory, used when the workspace is a local directory rather than a remote filesystem; that
//! half, and the grant-source resolution `sandbox_path_grant_host_path` performs, belong with the
//! backend that owns a host filesystem. A lexical check is not a substitute for it: a symlink inside
//! the workspace can point out of it, and only a resolving check catches that.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::error::{ErrorCode, OpName, SandboxError};

/// A path in POSIX form, as the sandbox filesystem sees it.
///
/// Holds text rather than a native path. Construction tidies the spelling — repeated separators
/// collapse, a bare `.` drops out, a trailing separator goes — because that is what the reference's
/// path type does when it is handed a string, and a refusal quotes the path *after* that tidying.
///
/// A `..` survives it. Resolving one is [`Self::normalized`], and it is a separate step on purpose:
/// the reference decides where a path lands by normalizing at the point of decision, and reports the
/// path as it was written. Folding the two together would make a refusal quote a path the caller
/// never asked about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PosixPath(String);

impl PosixPath {
    /// Reads text that is already POSIX-flavoured.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self(tidy(&path.into()))
    }

    /// Reads text that may have been written with backslashes.
    ///
    /// Separator translation is what the reference's `coerce_posix_path` does for strings, and it is
    /// deliberately not applied everywhere: [`SandboxWorkspaceScope::anchor`] passes a model-supplied
    /// path through untouched, so a literal backslash in a filename survives as part of the name
    /// rather than becoming a directory boundary that was never there.
    #[must_use]
    pub fn coerce(path: &str) -> Self {
        Self::new(path.replace('\\', "/"))
    }

    /// The path as POSIX text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the path starts at the filesystem root.
    #[must_use]
    pub fn is_absolute(&self) -> bool {
        self.0.starts_with('/')
    }

    /// The leading slashes, if any.
    ///
    /// Exactly two leading slashes are their own anchor. POSIX leaves `//foo` to the implementation,
    /// so it is not the same path as `/foo` and must not be collapsed into it. Three or more are an
    /// ordinary root, which is why the question is how many rather than whether.
    fn anchor(&self) -> &str {
        if self.0.starts_with("//") {
            "//"
        } else if self.0.starts_with('/') {
            "/"
        } else {
            ""
        }
    }

    /// The anchor followed by each named component.
    ///
    /// A bare `.` contributes nothing, so `a/./b` and `a/b` have the same components. A `..` is kept:
    /// it names something, and dropping it here would make an unnormalized path look safe.
    #[must_use]
    pub fn parts(&self) -> Vec<&str> {
        let anchor = self.anchor();
        let mut parts = Vec::new();
        if !anchor.is_empty() {
            parts.push(anchor);
        }
        parts.extend(
            self.0
                .split('/')
                .filter(|part| !part.is_empty() && *part != "."),
        );
        parts
    }

    /// Resolves `.` and `..` textually, without consulting a filesystem.
    ///
    /// A `..` that would climb past the root of an absolute path is dropped rather than kept, which
    /// is what makes `/workspace/../../etc` normalize to `/etc` and then fail the workspace check
    /// instead of quietly resolving somewhere else.
    #[must_use]
    pub fn normalized(&self) -> Self {
        let prefix = self.anchor();
        let mut components: Vec<&str> = Vec::new();
        for component in self.0.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                if !components.is_empty() && components.last() != Some(&"..") {
                    components.pop();
                } else if prefix.is_empty() {
                    // Relative paths keep a leading `..`; there is nothing above them to drop it
                    // against.
                    components.push("..");
                }
                continue;
            }
            components.push(component);
        }

        let normalized = format!("{prefix}{}", components.join("/"));
        Self(if normalized.is_empty() {
            ".".to_owned()
        } else {
            normalized
        })
    }

    /// Appends a path, or replaces this one when the appended path is absolute.
    #[must_use]
    pub fn join(&self, path: &str) -> Self {
        if path.starts_with('/') || self.parts().is_empty() {
            return Self::new(path);
        }
        if path.is_empty() {
            return self.clone();
        }
        if self.0.ends_with('/') {
            Self::new(format!("{}{path}", self.0))
        } else {
            Self::new(format!("{}/{path}", self.0))
        }
    }

    /// Re-measures this path from `base`, or `None` when it does not start there.
    ///
    /// Purely textual, like the reference's: a path equal to the base becomes `.`, and a path that
    /// merely shares a name prefix — `/workspace-alias` against `/workspace` — does not match,
    /// because the comparison is by component rather than by character.
    #[must_use]
    pub fn relative_to(&self, base: &Self) -> Option<Self> {
        let parts = self.parts();
        let base_parts = base.parts();
        if parts.len() < base_parts.len() || parts[..base_parts.len()] != base_parts[..] {
            return None;
        }
        let rest = &parts[base_parts.len()..];
        Some(if rest.is_empty() {
            Self(".".to_owned())
        } else {
            Self(rest.join("/"))
        })
    }

    /// Whether this path is `root` or something inside it.
    #[must_use]
    pub fn is_under(&self, root: &Self) -> bool {
        self.relative_to(root).is_some()
    }

    /// Whether this path is the filesystem root itself.
    ///
    /// Both `/` and `//` are: each is its own parent, and neither names anything a grant could
    /// sensibly hand out.
    #[must_use]
    pub fn is_filesystem_root(&self) -> bool {
        self.is_absolute() && self.parts().len() == 1
    }
}

impl std::fmt::Display for PosixPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<PosixPath> for String {
    fn from(path: PosixPath) -> Self {
        path.0
    }
}

/// Tidies a path's spelling the way the reference's path type does when it is handed a string.
///
/// Repeated separators collapse, a bare `.` drops out, and a trailing separator goes. Exactly two
/// leading separators survive, because POSIX leaves `//foo` to the implementation; three or more are
/// an ordinary root. A `..` is left alone — resolving one decides where a path lands, which is
/// [`PosixPath::normalized`] rather than a matter of spelling.
fn tidy(path: &str) -> String {
    let leading = path.len() - path.trim_start_matches('/').len();
    let prefix = match leading {
        0 => "",
        2 => "//",
        _ => "/",
    };
    let components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect();
    if components.is_empty() {
        // Nothing but separators, or nothing at all. The reference spells the empty path `.`.
        return if prefix.is_empty() {
            ".".to_owned()
        } else {
            prefix.to_owned()
        };
    }
    format!("{prefix}{}", components.join("/"))
}

/// Reads a path written in Windows drive-absolute syntax, rendered POSIX-style.
///
/// Only the drive form (`C:/x`, `C:\x`) is detected. A path that merely starts with a separator is
/// not absolute on Windows, and a UNC path is absolute in both flavours and so needs no special
/// handling here — the same two exclusions the reference's `PureWindowsPath` comparison produces.
#[must_use]
pub fn windows_absolute_path(path: &str) -> Option<String> {
    let mut characters = path.chars();
    if !characters.next()?.is_ascii_alphabetic() {
        return None;
    }
    if characters.next()? != ':' {
        return None;
    }
    if !matches!(characters.next()?, '/' | '\\') {
        return None;
    }
    Some(path.replace('\\', "/"))
}

/// Renders a Windows path the way the host would write it.
///
/// Separators become backslashes and repeated ones collapse, matching what the reference gets from
/// `str(PureWindowsPath(...))`. Only reached for paths already known to be drive-absolute, so there
/// is no UNC prefix to preserve.
fn windows_display(path: &str) -> String {
    let translated = path.replace('/', "\\");
    let (drive, rest) = translated.split_at(2);
    let components: Vec<&str> = rest.split('\\').filter(|part| !part.is_empty()).collect();
    if components.is_empty() {
        format!("{drive}\\")
    } else {
        format!("{drive}\\{}", components.join("\\"))
    }
}

/// Whether a native host path is the root of its filesystem.
fn native_is_filesystem_root(path: &Path) -> bool {
    path.is_absolute() && path.parent().is_none()
}

/// Whether a rendered Windows path is a bare drive, and so the whole of it.
///
/// Lexical, and deliberately so: the reference asks a `PureWindowsPath` this question whatever host
/// it is running on, which is what stops a configuration that means one thing on Windows from being
/// accepted with a different meaning elsewhere.
fn windows_is_drive_root(rendered: &str) -> bool {
    rendered.len() == 3 && rendered.ends_with('\\')
}

/// Why a run working directory was rejected.
///
/// The wording is the reference's: these reach whoever wrote the run configuration.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CwdError {
    /// The value was written with backslashes.
    #[error("sandbox.cwd must use POSIX path separators")]
    Separators,
    /// The value was empty or blank.
    #[error("sandbox.cwd must be non-empty")]
    Empty,
    /// The value was absolute, so it did not name a place inside the workspace.
    #[error("sandbox.cwd must be workspace-relative")]
    NotRelative,
    /// The value climbed above the workspace root.
    #[error("sandbox.cwd must not contain parent segments")]
    ParentSegments,
}

/// Validates a run working directory and resolves it against the workspace root.
///
/// # Errors
///
/// Returns [`CwdError`] for a blank value, one written with backslashes, an absolute one — POSIX or
/// Windows drive — or one containing a parent segment. A `..` is refused rather than normalized
/// away: `tasks/../a` and `a` would resolve to the same directory, but accepting the first would
/// mean accepting `../a` up to the point where normalization happens to save it.
pub fn normalize_sandbox_cwd(cwd: &str) -> Result<PosixPath, CwdError> {
    if cwd.contains('\\') {
        return Err(CwdError::Separators);
    }
    if cwd.trim().is_empty() {
        return Err(CwdError::Empty);
    }
    if windows_absolute_path(cwd).is_some() {
        return Err(CwdError::NotRelative);
    }

    let path = PosixPath::new(cwd);
    if path.is_absolute() {
        return Err(CwdError::NotRelative);
    }
    if path.parts().contains(&"..") {
        return Err(CwdError::ParentSegments);
    }
    Ok(path.normalized())
}

/// Why a path could not be rendered for the model.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScopePathError {
    /// A workspace-relative path handed to the display renderer was absolute.
    #[error("workspace-relative display paths must not be absolute")]
    DisplayPathAbsolute,
    /// A session resource path was written with backslashes.
    #[error("session resource paths must use POSIX path separators")]
    ResourceSeparators,
    /// A session resource path was absolute or climbed above the root.
    #[error("session resource paths must be workspace-relative")]
    ResourceNotRelative,
    /// A session resource path named nothing.
    #[error("session resource paths must be non-empty")]
    ResourceEmpty,
    /// The workspace root a resource was measured against was not POSIX absolute.
    #[error("sandbox workspace root must be POSIX absolute")]
    RootNotPosixAbsolute,
}

/// The directory a run's relative paths are measured from.
///
/// This changes where a relative path is anchored and nothing else. The session that owns the
/// workspace still decides what may be reached: a scope cannot widen access, and pointing it at a
/// directory does not confine a run to that directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxWorkspaceScope {
    cwd: Option<PosixPath>,
}

impl SandboxWorkspaceScope {
    /// Measures relative paths from the workspace root.
    #[must_use]
    pub const fn root() -> Self {
        Self { cwd: None }
    }

    /// Measures relative paths from a directory inside the workspace.
    ///
    /// # Errors
    ///
    /// Returns [`CwdError`] when the directory is not a usable workspace-relative path.
    pub fn from_cwd(cwd: Option<&str>) -> Result<Self, CwdError> {
        Ok(Self {
            cwd: cwd.map(normalize_sandbox_cwd).transpose()?,
        })
    }

    /// The working directory relative paths are measured from, if there is one.
    #[must_use]
    pub const fn cwd(&self) -> Option<&PosixPath> {
        self.cwd.as_ref()
    }

    /// Anchors one path beneath this scope's working directory.
    ///
    /// Absolute paths, in either flavour, are returned untouched: the scope moves the origin for
    /// relative paths, and a path that already states where it starts is not measured from anywhere.
    /// The text is otherwise passed through verbatim, backslashes included, because at this point it
    /// is still whatever a model wrote.
    #[must_use]
    pub fn anchor(&self, path: &str) -> String {
        let Some(cwd) = &self.cwd else {
            return path.to_owned();
        };
        if path.starts_with('/') || windows_absolute_path(path).is_some() {
            return path.to_owned();
        }
        cwd.join(path).into()
    }

    /// Renders a workspace-root-relative path the way the model should see it.
    ///
    /// # Errors
    ///
    /// Returns [`ScopePathError::DisplayPathAbsolute`] when handed an absolute path, which is
    /// measured from the filesystem rather than from the workspace and so has nothing to re-measure.
    pub fn model_path(&self, workspace_relative_path: &str) -> Result<PosixPath, ScopePathError> {
        let relative = PosixPath::coerce(workspace_relative_path);
        if relative.is_absolute() {
            return Err(ScopePathError::DisplayPathAbsolute);
        }
        let normalized = relative.normalized();
        Ok(match &self.cwd {
            None => normalized,
            Some(cwd) => relative_posix_path(&normalized, cwd),
        })
    }

    /// Renders a workspace resource the model has to keep addressing.
    ///
    /// Without a run working directory this is the workspace-root-relative path, unchanged. With one
    /// it becomes absolute — not relative to the cwd — because a resource named in instructions
    /// outlives the directory the model happened to be in when it read them: a shell command that
    /// selects a nested workdir, or a `cd`, would otherwise leave the path pointing at nothing.
    ///
    /// # Errors
    ///
    /// Returns [`ScopePathError`] for a path that is empty, absolute, written with backslashes or
    /// containing a parent segment, and for a workspace root that is not POSIX absolute.
    pub fn model_resource_path(
        &self,
        workspace_root: &str,
        workspace_relative_path: &str,
    ) -> Result<PosixPath, ScopePathError> {
        if workspace_relative_path.contains('\\') {
            return Err(ScopePathError::ResourceSeparators);
        }
        if windows_absolute_path(workspace_relative_path).is_some() {
            return Err(ScopePathError::ResourceNotRelative);
        }

        let relative = PosixPath::coerce(workspace_relative_path);
        if relative.is_absolute() || relative.parts().contains(&"..") {
            return Err(ScopePathError::ResourceNotRelative);
        }
        if relative.parts().is_empty() {
            return Err(ScopePathError::ResourceEmpty);
        }

        let normalized = relative.normalized();
        let Some(_cwd) = &self.cwd else {
            return Ok(normalized);
        };

        let root = if let Some(windows_root) = windows_absolute_path(workspace_root) {
            PosixPath::new(windows_root)
        } else if workspace_root.contains('\\') {
            return Err(ScopePathError::RootNotPosixAbsolute);
        } else {
            let root = PosixPath::coerce(workspace_root);
            if !root.is_absolute() {
                return Err(ScopePathError::RootNotPosixAbsolute);
            }
            root
        };
        Ok(root.join(normalized.as_str()).normalized())
    }

    /// Renders a tool result path.
    ///
    /// A model that asked using an absolute path is answered with one, whatever the scope is; a
    /// relative question gets a relative answer, measured from the same place the question was.
    ///
    /// # Errors
    ///
    /// As [`Self::model_path`].
    pub fn display_path(
        &self,
        original_path: &str,
        workspace_relative_path: &str,
    ) -> Result<PosixPath, ScopePathError> {
        let relative = PosixPath::coerce(workspace_relative_path);
        let answered_absolutely =
            original_path.starts_with('/') || windows_absolute_path(original_path).is_some();
        if self.cwd.is_none() || answered_absolutely {
            return Ok(relative.normalized());
        }
        self.model_path(relative.as_str())
    }
}

/// Re-measures `path` from `start`, climbing with `..` where it has to.
///
/// Textual, like the reference's `posixpath.relpath`: both paths are already normalized, and no
/// filesystem is consulted to find out whether a climbed-over component is a symlink.
fn relative_posix_path(path: &PosixPath, start: &PosixPath) -> PosixPath {
    let parts = path.parts();
    let start_parts = start.parts();
    let shared = parts
        .iter()
        .zip(start_parts.iter())
        .take_while(|(left, right)| left == right)
        .count();

    let mut components: Vec<&str> = vec![".."; start_parts.len() - shared];
    components.extend(&parts[shared..]);
    if components.is_empty() {
        PosixPath::new(".")
    } else {
        PosixPath::new(components.join("/"))
    }
}

/// Why a path grant was rejected.
///
/// The wording is the reference's: a grant is written by hand in a configuration, and these are what
/// tell its author which rule they hit.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PathGrantError {
    /// The in-sandbox path was relative, or absolute only in Windows drive syntax on a host where
    /// that is not a native absolute path.
    #[error("sandbox path grant path must be POSIX absolute")]
    PathNotPosixAbsolute,
    /// The grant named the filesystem root, which is every path at once.
    #[error("sandbox path grant path must not be filesystem root")]
    FilesystemRoot,
    /// The grant resolved to the filesystem root through a symlink.
    #[error("sandbox path grant path must not resolve to filesystem root")]
    ResolvedFilesystemRoot,
    /// The host source was relative.
    #[error("sandbox path grant host_path must be an absolute host path")]
    HostPathNotAbsolute,
    /// The host source was a UNC or device path.
    #[error("sandbox path grant host_path does not support UNC or device paths")]
    HostPathUnc,
    /// The host source climbed through a parent segment.
    #[error("sandbox path grant host_path must not contain parent segments")]
    HostPathParentSegments,
    /// A split grant named its in-sandbox path in Windows syntax.
    ///
    /// The two halves mean different things — one is a path inside the sandbox, the other a source on
    /// the host — and a drive letter on the sandbox side is a sign the two have been confused.
    #[error("sandbox path grant path must be POSIX absolute when host_path is configured")]
    SplitPathNotPosixAbsolute,
    /// The payload was not an object, so there was no grant in it to read.
    #[error("sandbox path grant must be an object")]
    NotAGrant,
    /// The payload named no in-sandbox path, which is the one field a grant cannot do without.
    #[error("sandbox path grant requires a `path`")]
    PathMissing,
    /// A field was present and not the shape a grant expects.
    #[error("sandbox path grant `{field}` must be a {expected}")]
    FieldType {
        /// The field that was rejected.
        field: &'static str,
        /// What it should have been.
        expected: &'static str,
    },
}

/// Access to one absolute path outside the workspace.
///
/// `path` is what the sandbox sees. `host_path` is an optional source on the host, for a backend
/// that materializes the subtree or bind-mounts it; when it is absent the two are the same place.
///
/// **This is authority.** A grant that survives into a persisted session state is a claim about what
/// a later process may reach, and a resumed session must take its grants from a manifest it trusts
/// rather than from whatever the payload carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SandboxPathGrant {
    /// The path as the sandbox sees it.
    path: String,
    /// Whether the sandbox may only read through this grant.
    read_only: bool,
    /// What the grant is for, for whoever reads the configuration later.
    description: Option<String>,
    /// The host source, when it differs from the in-sandbox path.
    #[serde(skip_serializing_if = "Option::is_none")]
    host_path: Option<String>,
}

impl<'de> Deserialize<'de> for SandboxPathGrant {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::from_json(&value).map_err(serde::de::Error::custom)
    }
}

impl SandboxPathGrant {
    /// Grants read and write access to one absolute path.
    ///
    /// # Errors
    ///
    /// Returns [`PathGrantError`] when the path is not absolute or names the filesystem root.
    pub fn new(path: &str) -> Result<Self, PathGrantError> {
        Ok(Self {
            path: validate_grant_path(path)?,
            read_only: false,
            description: None,
            host_path: None,
        })
    }

    /// Reads a grant from a payload, applying the rules construction applies.
    ///
    /// Validation is not skipped on the way in. A grant read back from a configuration file is
    /// exactly the case where an unchecked `/` or a `..` would matter, and a grant is the one thing
    /// in a manifest that widens what a session may reach.
    ///
    /// # Errors
    ///
    /// Returns [`PathGrantError`] when the payload is not an object, names no path, carries a field
    /// of the wrong type, or breaks one of the rules [`Self::new`] and [`Self::with_host_path`]
    /// enforce.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, PathGrantError> {
        let serde_json::Value::Object(fields) = value else {
            return Err(PathGrantError::NotAGrant);
        };
        let field = |key: &str| fields.get(key).filter(|value| !value.is_null());

        let path = field("path").ok_or(PathGrantError::PathMissing)?;
        let path = path.as_str().ok_or(PathGrantError::FieldType {
            field: "path",
            expected: "string",
        })?;
        let mut grant = Self::new(path)?;

        if let Some(read_only) = field("read_only") {
            grant.read_only = read_only.as_bool().ok_or(PathGrantError::FieldType {
                field: "read_only",
                expected: "boolean",
            })?;
        }
        if let Some(description) = field("description") {
            grant.description = Some(
                description
                    .as_str()
                    .ok_or(PathGrantError::FieldType {
                        field: "description",
                        expected: "string",
                    })?
                    .to_owned(),
            );
        }
        if let Some(host_path) = field("host_path") {
            let host_path = host_path.as_str().ok_or(PathGrantError::FieldType {
                field: "host_path",
                expected: "string",
            })?;
            grant = grant.with_host_path(host_path)?;
        }
        Ok(grant)
    }

    /// Narrows the grant to reads, or widens it back.
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Records what the grant is for.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Names a host source that differs from the in-sandbox path.
    ///
    /// # Errors
    ///
    /// Returns [`PathGrantError`] when the source is relative, a UNC or device path, contains a
    /// parent segment, or is the filesystem root, and when the in-sandbox path is not POSIX absolute.
    pub fn with_host_path(mut self, host_path: &str) -> Result<Self, PathGrantError> {
        if windows_absolute_path(&self.path).is_some() {
            return Err(PathGrantError::SplitPathNotPosixAbsolute);
        }
        self.host_path = Some(validate_grant_host_path(host_path)?);
        Ok(self)
    }

    /// The path as the sandbox sees it.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the sandbox may only read through this grant.
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// What the grant is for.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The host source, when it differs from the in-sandbox path.
    ///
    /// Turning this into a usable source directory means resolving it against a real filesystem and
    /// re-checking that it did not resolve to the root — a symlink swapped between validation and use
    /// is the case that check exists for. That resolution belongs to the backend that owns the host
    /// filesystem, and is not performed here.
    #[must_use]
    pub fn host_path(&self) -> Option<&str> {
        self.host_path.as_deref()
    }
}

/// Validates the in-sandbox half of a grant.
fn validate_grant_path(path: &str) -> Result<String, PathGrantError> {
    if let Some(windows_path) = windows_absolute_path(path) {
        // A drive path is usable only where the host itself reads it as absolute. Everywhere else it
        // is a relative path wearing a colon, and accepting it would grant a subtree of the
        // workspace under a name that looks like it is outside.
        let rendered = windows_display(&windows_path);
        if !Path::new(&rendered).is_absolute() {
            return Err(PathGrantError::PathNotPosixAbsolute);
        }
        if native_is_filesystem_root(Path::new(&rendered)) {
            return Err(PathGrantError::FilesystemRoot);
        }
        return Ok(rendered);
    }

    let normalized = PosixPath::coerce(path).normalized();
    if !normalized.is_absolute() {
        return Err(PathGrantError::PathNotPosixAbsolute);
    }
    if normalized.is_filesystem_root() {
        return Err(PathGrantError::FilesystemRoot);
    }
    Ok(normalized.into())
}

/// Validates the host half of a grant.
fn validate_grant_host_path(host_path: &str) -> Result<String, PathGrantError> {
    if host_path.starts_with("\\\\") || host_path.starts_with("//") {
        return Err(PathGrantError::HostPathUnc);
    }
    if host_path
        .split(['\\', '/'])
        .any(|component| component == "..")
    {
        return Err(PathGrantError::HostPathParentSegments);
    }

    if windows_absolute_path(host_path).is_some() {
        let rendered = windows_display(host_path);
        if windows_is_drive_root(&rendered) {
            return Err(PathGrantError::FilesystemRoot);
        }
        return Ok(rendered);
    }

    let normalized = PosixPath::new(host_path).normalized();
    if !normalized.is_absolute() {
        return Err(PathGrantError::HostPathNotAbsolute);
    }
    if normalized.is_filesystem_root() {
        return Err(PathGrantError::FilesystemRoot);
    }
    Ok(normalized.into())
}

/// A workspace root that does not say where it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("sandbox workspace root must be absolute")]
pub struct InvalidWorkspaceRoot;

/// Decides which paths a session may reach, and what they resolve to.
///
/// The workspace root is reachable, each grant adds one more subtree, and everything else is
/// refused. Relative paths are measured from the root; absolute paths are taken at face value and
/// must land somewhere allowed.
///
/// Every decision here is textual. A workspace that is a real host directory needs the resolving
/// variant the local backend owns — a symlink inside the workspace pointing out of it passes every
/// check in this type.
#[derive(Debug, Clone)]
pub struct WorkspacePathPolicy {
    root: PosixPath,
    extra_path_grants: Vec<SandboxPathGrant>,
}

impl WorkspacePathPolicy {
    /// Opens a policy over one workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidWorkspaceRoot`] when the root is not absolute. A relative root would make
    /// every check relative to wherever the process happened to be.
    pub fn new(
        root: &str,
        extra_path_grants: Vec<SandboxPathGrant>,
    ) -> Result<Self, InvalidWorkspaceRoot> {
        let root = PosixPath::coerce(root);
        if !root.is_absolute() {
            return Err(InvalidWorkspaceRoot);
        }
        Ok(Self {
            root: root.normalized(),
            extra_path_grants,
        })
    }

    /// The workspace root, normalized.
    #[must_use]
    pub const fn sandbox_root(&self) -> &PosixPath {
        &self.root
    }

    /// The grants this policy honours.
    #[must_use]
    pub fn extra_path_grants(&self) -> &[SandboxPathGrant] {
        &self.extra_path_grants
    }

    /// Resolves a path inside the workspace to an absolute one.
    ///
    /// Grants are not consulted: this answers where something inside the workspace lives, and a path
    /// that leaves the workspace has no answer here even when a grant would allow reaching it.
    ///
    /// # Errors
    ///
    /// Returns a [`SandboxError`] with [`ErrorCode::InvalidManifestPath`] for a path that is absolute
    /// outside the root, or that climbs out of it.
    pub fn absolute_workspace_path(&self, path: &str) -> Result<PosixPath, SandboxError> {
        if let Some(windows_path) = windows_absolute_path(path) {
            return Err(invalid_manifest_path(&PosixPath::new(windows_path)));
        }
        self.absolute_workspace_posix_path(&PosixPath::coerce(path))
    }

    /// Re-measures a path from the workspace root.
    ///
    /// The root itself becomes `.`. The absolute form is never handed back, so a tool result cannot
    /// leak where a provider put the workspace.
    ///
    /// # Errors
    ///
    /// As [`Self::absolute_workspace_path`].
    pub fn relative_path(&self, path: &str) -> Result<PosixPath, SandboxError> {
        if let Some(windows_path) = windows_absolute_path(path) {
            return Err(invalid_manifest_path(&PosixPath::new(windows_path)));
        }
        let absolute = self.absolute_workspace_posix_path(&PosixPath::coerce(path))?;
        Ok(absolute
            .relative_to(&self.root)
            .unwrap_or_else(|| PosixPath::new(".")))
    }

    /// Validates a path against the workspace and its grants.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidManifestPath`] for a path that is neither inside the workspace nor
    /// inside a grant, and [`ErrorCode::WorkspaceArchiveWriteError`] when `for_write` is set and the
    /// matching grant is read-only.
    pub fn normalize_sandbox_path(
        &self,
        path: &str,
        for_write: bool,
    ) -> Result<PosixPath, SandboxError> {
        if let Some(windows_path) = windows_absolute_path(path) {
            return Err(invalid_manifest_path(&PosixPath::new(windows_path)));
        }
        let original = PosixPath::coerce(path);
        let (resolved, grant) = self.sandbox_path_and_grant(&original)?;
        if for_write
            && let Some(grant) = grant
            && grant.is_read_only()
        {
            return Err(read_only_grant(&resolved, grant));
        }
        Ok(resolved)
    }

    /// The normalized grant roots and whether each is read-only.
    ///
    /// A backend that can resolve paths inside the sandbox uses these to re-check a resolved path
    /// against the same rules this policy applies textually.
    ///
    /// # Errors
    ///
    /// Returns [`PathGrantError`] when a grant names a Windows drive path or the filesystem root.
    /// Constructing a grant already rejects both; a policy can still be handed grants that were
    /// deserialized elsewhere, and the rules are cheap enough to state twice.
    pub fn extra_path_grant_rules(&self) -> Result<Vec<(PosixPath, bool)>, PathGrantError> {
        let mut rules = Vec::with_capacity(self.extra_path_grants.len());
        for grant in &self.extra_path_grants {
            if windows_absolute_path(grant.path()).is_some() {
                return Err(PathGrantError::PathNotPosixAbsolute);
            }
            let root = PosixPath::coerce(grant.path());
            if root.is_filesystem_root() {
                return Err(PathGrantError::FilesystemRoot);
            }
            rules.push((root, grant.is_read_only()));
        }
        Ok(rules)
    }

    /// Resolves a path and says which grant, if any, allowed it.
    fn sandbox_path_and_grant(
        &self,
        original: &PosixPath,
    ) -> Result<(PosixPath, Option<&SandboxPathGrant>), SandboxError> {
        let normalized = if original.is_absolute() {
            self.absolute_posix_path(original)
        } else {
            self.absolute_workspace_posix_path(original)?
        };
        if normalized.is_under(&self.root) {
            return Ok((normalized, None));
        }
        // A grant is reachable only by naming it outright. A relative path that normalized its way
        // out of the workspace and landed in a granted subtree is still a path that escaped the
        // root, and treating it as granted would make `../../opt/toolchain` mean something.
        match self.matching_grant(&normalized) {
            Some(grant) if original.is_absolute() => Ok((normalized, Some(grant))),
            _ => Err(invalid_manifest_path(original)),
        }
    }

    /// The most specific grant containing this path, if any.
    fn matching_grant(&self, path: &PosixPath) -> Option<&SandboxPathGrant> {
        self.extra_path_grants
            .iter()
            .filter_map(|grant| {
                let root = PosixPath::coerce(grant.path());
                path.is_under(&root).then(|| (grant, root.parts().len()))
            })
            // Nested grants are allowed, and the innermost one decides: a read-only grant inside a
            // writable one is a narrowing, so the longest match has to win rather than the first.
            .max_by_key(|(_, depth)| *depth)
            .map(|(grant, _)| grant)
    }

    /// Resolves a path that must end up inside the workspace.
    fn absolute_workspace_posix_path(&self, path: &PosixPath) -> Result<PosixPath, SandboxError> {
        let normalized = self.absolute_posix_path(path);
        if normalized.is_under(&self.root) {
            Ok(normalized)
        } else {
            Err(invalid_manifest_path(path))
        }
    }

    /// Anchors a relative path under the root and normalizes the result.
    fn absolute_posix_path(&self, path: &PosixPath) -> PosixPath {
        if path.is_absolute() {
            path.normalized()
        } else {
            self.root.join(path.as_str()).normalized()
        }
    }
}

/// Refuses a path that is outside the workspace, or that had to climb out to get there.
fn invalid_manifest_path(path: &PosixPath) -> SandboxError {
    // Absolute in either flavour. A drive path reaches here already rendered POSIX-style, and
    // calling it an escaped relative path would describe a mistake the caller did not make.
    let absolute = path.is_absolute() || windows_absolute_path(path.as_str()).is_some();
    let reason = if absolute { "absolute" } else { "escape_root" };
    let message = if absolute {
        format!("manifest path must be relative: {path}")
    } else {
        format!("manifest path must not escape root: {path}")
    };
    SandboxError::new(ErrorCode::InvalidManifestPath, OpName::Materialize, message)
        .with_context("rel", path.as_str())
        .with_context("reason", reason)
}

/// Refuses a write through a grant that only allows reading.
fn read_only_grant(path: &PosixPath, grant: &SandboxPathGrant) -> SandboxError {
    SandboxError::workspace_archive_write(path.as_str())
        .with_context("reason", "read_only_extra_path_grant")
        .with_context("grant_path", grant.path())
}
