//! `ra-sandbox::snapshot::defaults`: where a snapshot goes when nobody said where.
//!
//! The answer is the platform's per-user state directory, and the branches are tested by describing
//! a host rather than by running on one — a Windows rule that is only ever exercised on Windows is
//! a rule nobody checks. The other half is what the SDK is allowed to delete from the directory it
//! manages: old archives, and nothing else, because a run paused last month resumes from one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ra_core::sandbox::{ErrorCode, SnapshotSpec};
use ra_sandbox::snapshot::defaults::{
    DEFAULT_LOCAL_SNAPSHOT_TTL, HostPlatform, SnapshotHost, cleanup_stale_default_local_snapshots,
    default_local_snapshot_base_dir, resolve_default_local_snapshot_spec,
};
use rstest::rstest;

/// What every answer ends with, below the platform's state directory.
const SUBDIR: [&str; 3] = ["rusty-agent", "sandbox", "snapshots"];

/// A host with the environment spelled out.
fn host(home: impl Into<PathBuf>, env: &[(&str, &str)], platform: HostPlatform) -> SnapshotHost {
    let env: BTreeMap<String, String> = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    SnapshotHost::new(home, env, platform)
}

/// The managed directory under `base`.
fn managed(base: impl Into<PathBuf>) -> PathBuf {
    SUBDIR
        .iter()
        .fold(base.into(), |path, segment| path.join(segment))
}

#[rstest]
// An absolute `XDG_STATE_HOME` is the user saying where their state lives.
#[case(&[("XDG_STATE_HOME", "/state")], "/state")]
// A relative one is ignored rather than resolved against the working directory, which would put a
// user's snapshots wherever the process happened to start.
#[case(&[("XDG_STATE_HOME", "relative-state")], "/home/.local/state")]
#[case(&[], "/home/.local/state")]
fn a_posix_host_keeps_snapshots_in_its_state_directory(
    #[case] env: &[(&str, &str)],
    #[case] expected_base: &str,
) {
    let host = host("/home", env, HostPlatform::Other);

    assert_eq!(
        default_local_snapshot_base_dir(&host),
        managed(expected_base)
    );
}

#[test]
fn a_mac_keeps_them_in_application_support() {
    // Read from the platform, not from the environment: `XDG_STATE_HOME` set on a Mac does not
    // move them.
    let host = host("/home", &[("XDG_STATE_HOME", "/state")], HostPlatform::MacOs);

    assert_eq!(
        default_local_snapshot_base_dir(&host),
        managed("/home/Library/Application Support")
    );
}

#[rstest]
#[case(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")], r"C:\Users\me\AppData\Local")]
// The roaming directory is the fallback, and only an absolute one counts.
#[case(
    &[("LOCALAPPDATA", "relative-local"), ("APPDATA", r"C:\Users\me\AppData\Roaming")],
    r"C:\Users\me\AppData\Roaming"
)]
// A network share is absolute on Windows.
#[case(&[("LOCALAPPDATA", r"\\server\share\state")], r"\\server\share\state")]
fn a_windows_host_follows_its_application_data_variables(
    #[case] env: &[(&str, &str)],
    #[case] expected_base: &str,
) {
    let host = host(r"C:\Users\me", env, HostPlatform::Windows);

    assert_eq!(
        default_local_snapshot_base_dir(&host),
        managed(expected_base)
    );
}

#[rstest]
#[case(&[])]
#[case(&[("LOCALAPPDATA", "relative-local"), ("APPDATA", "relative-roaming")])]
// Absolute on POSIX is not absolute on Windows: `/tmp/x` has no drive, and taking it would put the
// snapshots on whichever drive the process started from.
#[case(&[("LOCALAPPDATA", "/tmp/localappdata")])]
// Half a network share is not one.
#[case(&[("LOCALAPPDATA", r"\\server")])]
#[case(&[("LOCALAPPDATA", r"\\server\\share")])]
#[case(&[("LOCALAPPDATA", "")])]
fn a_windows_host_ignores_a_variable_that_is_not_an_absolute_path(#[case] env: &[(&str, &str)]) {
    let host = host(r"C:\Users\me", env, HostPlatform::Windows);

    assert_eq!(
        default_local_snapshot_base_dir(&host),
        managed(PathBuf::from(r"C:\Users\me").join("AppData").join("Local"))
    );
}

#[test]
fn cleanup_takes_old_archives_and_leaves_everything_else() {
    let directory = tempfile::tempdir().expect("temp");
    let now = SystemTime::now();
    let stale = directory.path().join("stale.tar");
    let fresh = directory.path().join("fresh.tar");
    let other = directory.path().join("keep.txt");
    for (path, body) in [(&stale, "stale"), (&fresh, "fresh"), (&other, "keep")] {
        std::fs::write(path, body).expect("write");
    }
    set_modified(&stale, now - DEFAULT_LOCAL_SNAPSHOT_TTL - Duration::from_secs(60));
    set_modified(&fresh, now - Duration::from_secs(60));
    // Old, and still not this directory's business: only `.tar` is the SDK's to delete.
    set_modified(&other, now - DEFAULT_LOCAL_SNAPSHOT_TTL - Duration::from_secs(60));

    cleanup_stale_default_local_snapshots(directory.path(), now, DEFAULT_LOCAL_SNAPSHOT_TTL);

    assert!(!stale.exists());
    assert!(fresh.exists());
    assert!(other.exists());
}

#[test]
fn cleanup_says_nothing_about_a_directory_that_is_not_there() {
    let directory = tempfile::tempdir().expect("temp");

    cleanup_stale_default_local_snapshots(
        &directory.path().join("never-created"),
        SystemTime::now(),
        DEFAULT_LOCAL_SNAPSHOT_TTL,
    );
}

#[test]
fn resolving_the_default_makes_the_directory_and_keeps_what_is_in_it() {
    let directory = tempfile::tempdir().expect("temp");
    let home = directory.path().join("home");
    let expected = managed(home.join(".local").join("state"));
    std::fs::create_dir_all(&expected).expect("directory");
    let stale = expected.join("stale.tar");
    std::fs::write(&stale, b"stale").expect("write");
    set_modified(
        &stale,
        SystemTime::now() - DEFAULT_LOCAL_SNAPSHOT_TTL - Duration::from_secs(60),
    );

    let spec = resolve_default_local_snapshot_spec(&host(&home, &[], HostPlatform::Other))
        .expect("resolve");

    assert_eq!(
        spec,
        SnapshotSpec::Local {
            base_path: expected.clone(),
        }
    );
    // Resolving is not cleaning up: the run that paused last month resumes from this file.
    assert!(stale.exists());
    assert_eq!(mode(&expected), 0o700);
}

#[test]
fn the_directory_it_creates_is_readable_by_this_account_alone() {
    // A workspace archive holds whatever the session was working on, including files a manifest
    // wrote from the host's own credentials.
    let directory = tempfile::tempdir().expect("temp");
    let home = directory.path().join("home");

    let spec = resolve_default_local_snapshot_spec(&host(&home, &[], HostPlatform::Other))
        .expect("resolve");

    let SnapshotSpec::Local { base_path } = spec else {
        panic!("the default is local storage");
    };
    assert_eq!(mode(&base_path), 0o700);
}

#[cfg(unix)]
#[test]
fn a_home_directory_this_host_cannot_spell_is_refused_rather_than_mangled() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // The directory ends up inside a snapshot, which is stored as JSON. Rendering it lossily would
    // make two accounts whose names differ only in unreadable bytes share one snapshot directory.
    let home = PathBuf::from(OsStr::from_bytes(b"/tmp/home-\xff"));

    let error = resolve_default_local_snapshot_spec(&host(&home, &[], HostPlatform::Other))
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert!(!managed(home.join(".local").join("state")).exists());
}

/// Backdates a file, so a cutoff has something to be on the far side of.
fn set_modified(path: &Path, modified: SystemTime) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open");
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("times");
}

/// The permission bits a directory carries.
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}
