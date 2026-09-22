//! Where snapshots go when nobody chose a place for them.
//!
//! A caller that asks for pause and resume without naming storage still needs the archive to land
//! somewhere the next process can find it. That is a per-user state directory, whose location is
//! the operating system's convention rather than ours, and whose contents the SDK owns: nothing
//! else writes there, so it is also the one directory a stale snapshot may be deleted from.
//!
//! # Only the directory it manages
//!
//! [`cleanup_stale_default_local_snapshots`] deletes from this directory alone, and only files old
//! enough that no pause can still be waiting on them. Snapshots are deliberately **not** deleted
//! when a session ends: a paused run resumes from one long after the session that wrote it is
//! gone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ra_core::sandbox::{ErrorCode, OpName, SandboxError, SandboxResult, SnapshotSpec};

/// How long a snapshot in the managed directory is kept before it counts as stale.
pub const DEFAULT_LOCAL_SNAPSHOT_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);

/// The path the managed directory has below the platform's state directory.
///
/// Named after this product rather than the reference's Python distribution: it is a directory in
/// the user's own account, and what belongs in it is whatever this SDK wrote.
const DEFAULT_LOCAL_SNAPSHOT_SUBDIR: [&str; 3] = ["rusty-agent", "sandbox", "snapshots"];

/// The suffix the managed directory's snapshots carry, and the only one cleanup considers.
const SNAPSHOT_FILE_SUFFIX: &str = ".tar";

/// Which set of conventions this host follows for per-user state.
///
/// Three cases rather than the reference's two strings: it reads `sys.platform` for macOS and
/// `os.name` for Windows, and the combinations that do not describe a real host — a macOS that is
/// also Windows — are not worth being able to write down.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPlatform {
    /// `~/Library/Application Support`.
    MacOs,
    /// `%LOCALAPPDATA%`, then `%APPDATA%`, then `~/AppData/Local`.
    Windows,
    /// `$XDG_STATE_HOME`, then `~/.local/state`.
    Other,
}

impl HostPlatform {
    /// What this build is running on.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// The facts about a machine that decide where its snapshots go.
///
/// Passed in rather than read at the point of use, which is how the reference makes the same code
/// answer for every platform: the Windows branch is exercised from a Linux test run by describing a
/// Windows host, not by running on one.
#[derive(Debug, Clone)]
pub struct SnapshotHost {
    home: PathBuf,
    env: BTreeMap<String, String>,
    platform: HostPlatform,
}

impl SnapshotHost {
    /// Describes a host explicitly.
    #[must_use]
    pub fn new(
        home: impl Into<PathBuf>,
        env: BTreeMap<String, String>,
        platform: HostPlatform,
    ) -> Self {
        Self {
            home: home.into(),
            env,
            platform,
        }
    }

    /// Describes the machine this process is running on.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SandboxConfigInvalid`] when the account has no home directory to put a
    /// state directory under.
    pub fn current() -> SandboxResult<Self> {
        let home_variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let home = std::env::var_os(home_variable)
            .filter(|home| !home.is_empty())
            .ok_or_else(|| {
                SandboxError::new(
                    ErrorCode::SandboxConfigInvalid,
                    OpName::SnapshotPersist,
                    format!(
                        "cannot place default snapshot storage: `{home_variable}` names no home \
                         directory"
                    ),
                )
            })?;
        // `vars_os`, because a variable this process cannot read as UTF-8 is not a reason to stop:
        // the two that decide this answer are paths the host wrote, and the rest are bystanders.
        let env = std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .collect();
        Ok(Self::new(PathBuf::from(home), env, HostPlatform::current()))
    }
}

/// The directory this host keeps SDK-managed snapshots in.
#[must_use]
pub fn default_local_snapshot_base_dir(host: &SnapshotHost) -> PathBuf {
    let base = match host.platform {
        HostPlatform::MacOs => host.home.join("Library").join("Application Support"),
        HostPlatform::Windows => {
            first_absolute_windows_env_path(&host.env, &["LOCALAPPDATA", "APPDATA"])
                .unwrap_or_else(|| host.home.join("AppData").join("Local"))
        }
        // An `XDG_STATE_HOME` that is not absolute is ignored rather than resolved against the
        // working directory, which would put a user's snapshots wherever the process happened to
        // start.
        HostPlatform::Other => match host.env.get("XDG_STATE_HOME") {
            Some(state_home) if state_home.starts_with('/') => PathBuf::from(state_home),
            _ => host.home.join(".local").join("state"),
        },
    };
    DEFAULT_LOCAL_SNAPSHOT_SUBDIR
        .iter()
        .fold(base, |path, segment| path.join(segment))
}

/// Deletes the managed directory's snapshots that nothing can still be waiting on.
///
/// Every failure is skipped rather than reported: this is housekeeping alongside work the caller
/// actually asked for, and a file that could not be removed is a file that will be considered again
/// next time.
pub fn cleanup_stale_default_local_snapshots(base_path: &Path, now: SystemTime, max_age: Duration) {
    let Some(cutoff) = now.checked_sub(max_age) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(base_path) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(SNAPSHOT_FILE_SUFFIX))
        {
            continue;
        }
        // Metadata rather than the directory entry's own type, so that a symlink is judged by what
        // it points at, as the reference's `is_file()` is.
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        // A file whose age cannot be read is left alone: "old enough to delete" is a claim, and an
        // unreadable timestamp does not support it.
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified >= cutoff {
            continue;
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Settles on the managed directory, creating it if this is the first time.
///
/// Readable by this account alone: a workspace archive holds whatever the session was working on,
/// including files a manifest wrote from the host's own credentials.
///
/// **Existing snapshots are left alone, stale or not.** A caller resuming a run that was paused
/// last month needs the archive that run wrote; cleaning up here would delete it on the way to
/// finding it.
///
/// # Errors
///
/// Returns [`ErrorCode::SandboxConfigInvalid`] when this host's directory is not valid UTF-8 — it
/// is built from the account's home directory, which a snapshot cannot name if it cannot be written
/// down — and [`ErrorCode::SnapshotPersistError`] when the directory cannot be created.
pub fn resolve_default_local_snapshot_spec(host: &SnapshotHost) -> SandboxResult<SnapshotSpec> {
    let base_path = default_local_snapshot_base_dir(host);
    // Checked before the directory is made, and before a spec that cannot be used is handed back:
    // the failure belongs to whoever resolved the default, not to the session that later tries to
    // build a snapshot out of it.
    if base_path.to_str().is_none() {
        return Err(SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            OpName::SnapshotPersist,
            format!(
                "the default snapshot directory must be valid UTF-8: {}",
                base_path.display()
            ),
        )
        .with_context("path", base_path.to_string_lossy().as_ref()));
    }

    create_private_dir(&base_path).map_err(|error| {
        SandboxError::new(
            ErrorCode::SnapshotPersistError,
            OpName::SnapshotPersist,
            format!(
                "failed to create the default snapshot directory: {}",
                base_path.display()
            ),
        )
        .with_context("path", base_path.to_string_lossy().as_ref())
        .with_cause(error)
    })?;
    Ok(SnapshotSpec::Local { base_path })
}

/// Creates the managed directory, restricted to this account where the platform can say so.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        // The mode above is masked by the process umask, and says nothing about a directory that
        // already existed. Narrowing it afterwards is best effort: a directory the user widened on
        // purpose is still theirs, and refusing to run would help nobody.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// The first of `names` whose value is an absolute path in Windows spelling.
///
/// A relative value is ignored rather than resolved, and so is a POSIX-absolute one: `/tmp/x` is
/// not absolute on Windows, and treating it as though it were would put a user's snapshots on
/// whichever drive the process started from.
fn first_absolute_windows_env_path(
    env: &BTreeMap<String, String>,
    names: &[&str],
) -> Option<PathBuf> {
    names
        .iter()
        .filter_map(|name| env.get(*name))
        .find(|value| !value.is_empty() && is_windows_absolute(value))
        .map(PathBuf::from)
}

/// Whether a path is absolute in Windows spelling: a drive, or a network share.
fn is_windows_absolute(value: &str) -> bool {
    if ra_core::sandbox::windows_absolute_path(value).is_some() {
        return true;
    }

    // A UNC path is `\\server\share`, and needs both halves: `\\server` alone is not absolute, and
    // neither is a third leading separator or an empty share.
    let normalized = value.replace('\\', "/");
    let Some(rest) = normalized.strip_prefix("//") else {
        return false;
    };
    let mut segments = rest.split('/');
    let server = segments.next().unwrap_or_default();
    let share = segments.next().unwrap_or_default();
    !server.is_empty() && !share.is_empty()
}
