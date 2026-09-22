//! The fence macOS puts around one local command.
//!
//! The reference wraps every command this backend runs in `sandbox-exec` when it is running on
//! macOS, with a profile that denies the places a user's data lives and then allows the workspace,
//! the interpreter's own search path, and whatever the manifest granted. This is that profile.
//!
//! **It fences a command, not the backend.** The process still runs as the user running the SDK,
//! on the same network, against the same filesystem; what the profile removes is the accidental
//! reach — a command that wanders into `~/Documents` because nothing stopped it. Treating it as
//! isolation would be reading a guard rail as a wall.
//!
//! On every other platform there is no counterpart and the command runs as written, which is what
//! the reference does too.

#[cfg(target_os = "macos")]
pub(crate) use macos::confined_exec_command;
#[cfg(not(target_os = "macos"))]
pub(crate) use other::confined_exec_command;

#[cfg(not(target_os = "macos"))]
mod other {
    use std::collections::BTreeMap;
    use std::path::Path;

    use ra_core::sandbox::{SandboxError, SandboxPathGrant};

    /// Runs the command as written.
    pub(crate) fn confined_exec_command(
        command_parts: Vec<String>,
        _workspace_root: &Path,
        _env: &BTreeMap<String, String>,
        _extra_path_grants: &[SandboxPathGrant],
    ) -> Result<Vec<String>, SandboxError> {
        Ok(command_parts)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use ra_core::sandbox::{ErrorCode, OpName, PathGrantError, SandboxError, SandboxPathGrant};

    use crate::host_paths::{expand_user, resolve_without_strictness};

    /// The directories the profile denies outright, before anything is allowed back.
    ///
    /// Later rules win in this profile language, so the allows below re-open the workspace and the
    /// toolchain paths that live inside these trees. The list is the reference's, in its order.
    const DENIED: [&str; 12] = [
        "/Users",
        "/Volumes",
        "/Applications",
        "/Library",
        "/opt",
        "/etc",
        "/private/etc",
        "/tmp",
        "/private/tmp",
        "/private",
        "/var",
        "/usr",
    ];

    /// Prefixes whose whole tree becomes readable when anything inside it is.
    ///
    /// A toolchain installed under one of these reaches sideways into its siblings — a Homebrew
    /// binary loads Homebrew libraries — so granting only the directory holding the executable
    /// produces a command that starts and then cannot find its own runtime.
    const WIDENED_PREFIXES: [&str; 3] = ["/opt/homebrew", "/usr/local", "/Library/Frameworks"];

    /// Wraps a command in the host sandbox, or explains why it cannot be.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ExecTransportError`] when `sandbox-exec` is not on the host, because a
    /// command that cannot be fenced is not run unfenced — a macOS host that has lost the tool is a
    /// broken host, not a request to widen access.
    pub(crate) fn confined_exec_command(
        command_parts: Vec<String>,
        workspace_root: &Path,
        env: &BTreeMap<String, String>,
        extra_path_grants: &[SandboxPathGrant],
    ) -> Result<Vec<String>, SandboxError> {
        let Some(sandbox_exec) = which("sandbox-exec", None) else {
            return Err(SandboxError::exec_transport(command_parts, None)
                .with_context("reason", "unix_local_confinement_unavailable")
                .with_context("platform", std::env::consts::OS)
                .with_context("workspace_root", workspace_root.to_string_lossy().as_ref()));
        };

        let profile = exec_profile(
            workspace_root,
            &additional_read_paths(&command_parts, env)?,
            &extra_path_grant_roots(extra_path_grants)?,
        );
        let mut confined = vec![
            sandbox_exec.to_string_lossy().into_owned(),
            "-p".to_owned(),
            profile,
        ];
        confined.extend(command_parts);
        Ok(confined)
    }

    /// Finds an executable on a search path, the way a shell would.
    fn which(program: &str, path: Option<&str>) -> Option<PathBuf> {
        if program.contains('/') {
            let candidate = PathBuf::from(program);
            return is_executable_file(&candidate).then_some(candidate);
        }
        let search = match path {
            Some(path) => path.to_owned(),
            None => std::env::var("PATH").ok()?,
        };
        search
            .split(':')
            .filter(|directory| !directory.is_empty())
            .map(|directory| Path::new(directory).join(program))
            .find(|candidate| is_executable_file(candidate))
    }

    /// Whether a path is a file this process could execute.
    fn is_executable_file(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;

        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }

    /// This host's home directory, resolved.
    fn host_home() -> Result<PathBuf, SandboxError> {
        std::env::var_os("HOME").map_or_else(
            || Ok(PathBuf::from("/")),
            |home| resolve_without_strictness(Path::new(&home)),
        )
    }

    /// The directories a command needs to read in order to be the command it is.
    ///
    /// Three sources, and the third is deliberately separate: only a path the *host* put on `PATH`
    /// may widen a `bin` grant to the environment root above it. A manifest that sets `PATH` can
    /// point a command somewhere else, and letting it also widen the grant would turn a
    /// configuration field into an access decision.
    fn additional_read_paths(
        command_parts: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Vec<PathBuf>, SandboxError> {
        let home = host_home()?;
        let mut allowed: Vec<PathBuf> = Vec::new();
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();

        let mut append =
            |candidate: &str, allow_environment_root: bool| -> Result<(), SandboxError> {
                let candidate = expand_user(candidate);
                if !candidate.is_absolute() {
                    return Ok(());
                }
                for root in allowable_read_roots(&candidate, &home, allow_environment_root)? {
                    if seen.insert(root.clone()) {
                        allowed.push(root);
                    }
                }
                Ok(())
            };

        for entry in env.get("PATH").map_or("", String::as_str).split(':') {
            if !entry.is_empty() {
                append(entry, false)?;
            }
        }
        if let Some(program) = command_parts.first()
            && let Some(executable) = which(program, env.get("PATH").map(String::as_str))
        {
            append(&executable.to_string_lossy(), false)?;
        }
        let host_path = std::env::var("PATH").unwrap_or_default();
        for entry in host_path.split(':') {
            if !entry.is_empty() {
                append(entry, true)?;
            }
        }
        Ok(allowed)
    }

    /// The read roots one path on the search path implies.
    fn allowable_read_roots(
        path: &Path,
        home: &Path,
        allow_environment_root: bool,
    ) -> Result<Vec<PathBuf>, SandboxError> {
        let mut candidates: BTreeSet<PathBuf> = BTreeSet::new();
        let resolved = resolve_without_strictness(path)?;

        // Both spellings are granted: the profile matches the path a command actually opens, and a
        // symlinked toolchain is opened under one name and read under the other.
        for candidate in [path, resolved.as_path()] {
            if candidate.is_dir() {
                candidates.insert(candidate.to_owned());
            } else if let Some(parent) = candidate.parent() {
                candidates.insert(parent.to_owned());
            }
        }

        if allow_environment_root {
            for candidate in [path, resolved.as_path()] {
                if candidate.file_name() == Some(OsStr::new("bin"))
                    && let Some(root) = candidate.parent()
                    && root.join("pyvenv.cfg").is_file()
                {
                    candidates.insert(root.to_owned());
                }
            }
        }

        let resolved_text = resolved.to_string_lossy();
        for prefix in WIDENED_PREFIXES {
            if resolved_text == prefix || resolved_text.starts_with(&format!("{prefix}/")) {
                candidates.insert(PathBuf::from(prefix));
            }
        }

        if let Ok(relative) = resolved.strip_prefix(home) {
            let parts: Vec<&OsStr> = relative.iter().collect();
            match parts.first().map(|part| part.to_string_lossy()) {
                // A dotted directory in the home is where a per-user toolchain installs itself, and
                // it is granted as a whole rather than one nested directory at a time.
                Some(first) if first.starts_with('.') => {
                    candidates.insert(home.join(first.as_ref()));
                }
                Some(_) if parts.len() >= 2 && parts[0] == "Library" && parts[1] == "Python" => {
                    candidates.insert(home.join("Library").join("Python"));
                }
                _ => {}
            }
        }

        // Shortest first, so the profile reads outermost grant to innermost.
        let mut roots: Vec<PathBuf> = candidates.into_iter().collect();
        roots.sort_by_key(|root| {
            (
                root.components().count(),
                root.to_string_lossy().into_owned(),
            )
        });
        Ok(roots)
    }

    /// The host directories a manifest's grants open, with whether each is read-only.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when a grant is, or resolves to, the filesystem
    /// root — which would hand the whole host to a command in the name of one subtree.
    fn extra_path_grant_roots(
        grants: &[SandboxPathGrant],
    ) -> Result<Vec<(PathBuf, bool)>, SandboxError> {
        let mut roots: Vec<(PathBuf, bool)> = Vec::new();
        let mut seen: BTreeSet<(PathBuf, bool)> = BTreeSet::new();
        let mut append = |path: PathBuf, read_only: bool| -> Result<(), SandboxError> {
            if path.is_absolute() && path.parent().is_none() {
                return Err(invalid_grant(PathGrantError::ResolvedFilesystemRoot));
            }
            if seen.insert((path.clone(), read_only)) {
                roots.push((path, read_only));
            }
            Ok(())
        };

        for grant in grants {
            let grant_path = expand_user(grant.path());
            let resolved = resolve_without_strictness(&grant_path)?;
            append(grant_path.clone(), grant.is_read_only())?;
            if resolved != grant_path {
                append(resolved, grant.is_read_only())?;
            }
        }
        Ok(roots)
    }

    /// Reports a grant that cannot be turned into a profile rule.
    fn invalid_grant(error: PathGrantError) -> SandboxError {
        SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            OpName::Exec,
            error.to_string(),
        )
    }

    /// Renders a path as a profile string literal.
    fn literal(path: &Path) -> String {
        let escaped = path
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        format!("\"{escaped}\"")
    }

    /// Builds the profile for one command.
    ///
    /// Permissive by default and then narrowed, which is the reference's shape: the denies remove
    /// the user's data and the system's configuration, and the allows put back the workspace, the
    /// toolchain, and the grants. Order is the whole mechanism — a later rule wins — so the
    /// workspace allow has to follow the `/private` deny that would otherwise cover a temporary
    /// workspace root.
    fn exec_profile(
        workspace_root: &Path,
        extra_read_paths: &[PathBuf],
        extra_path_grants: &[(PathBuf, bool)],
    ) -> String {
        let mut lines = vec!["(version 1)".to_owned(), "(allow default)".to_owned()];
        for path in DENIED {
            let path = literal(Path::new(path));
            lines.push(format!("(deny file-read-data (subpath {path}))"));
            lines.push(format!("(deny file-write* (subpath {path}))"));
        }

        let root = literal(workspace_root);
        lines.push(format!(
            "(allow file-read-data file-read-metadata (subpath {root}))"
        ));
        lines.push(format!("(allow file-write* (subpath {root}))"));
        for path in extra_read_paths {
            lines.push(format!(
                "(allow file-read-data file-read-metadata (subpath {}))",
                literal(path)
            ));
        }
        for (path, _read_only) in extra_path_grants {
            lines.push(format!(
                "(allow file-read-data file-read-metadata (subpath {}))",
                literal(path)
            ));
        }
        for (path, read_only) in extra_path_grants {
            if !read_only {
                lines.push(format!("(allow file-write* (subpath {}))", literal(path)));
            }
        }
        for (path, read_only) in extra_path_grants {
            if *read_only {
                lines.push(format!("(deny file-write* (subpath {}))", literal(path)));
            }
        }
        lines.push("(allow file-read-data file-read-metadata (subpath \"/usr/bin\"))".to_owned());
        lines.push("(allow file-read-data file-read-metadata (subpath \"/usr/lib\"))".to_owned());
        lines.push("(allow file-read-data file-read-metadata (subpath \"/bin\"))".to_owned());
        lines.push("(allow file-read-data file-read-metadata (subpath \"/System\"))".to_owned());
        lines.push(
            "(allow file-read-data file-read-metadata (literal \"/private/var/select/sh\"))"
                .to_owned(),
        );
        lines.push("(allow file-write* (literal \"/dev/null\"))".to_owned());
        lines.join("\n")
    }
}
