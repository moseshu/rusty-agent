//! Privileged socket isolation, ported from Codex's Seatbelt and Linux daemon socket tests.
#![cfg(unix)]

use std::{
    collections::BTreeSet,
    ffi::OsString,
    os::unix::{ffi::OsStringExt, fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
};

use ra_uds::{prepare_shared_daemon_socket_directory, shared_daemon_socket_directory};

use ra_exec::sandbox::{
    ConfinementRequest, NetworkAccess, SandboxBackend, SandboxCommand, SandboxError, SandboxLevel,
    SandboxPolicy, SandboxUnavailable, bwrap::test_api::protected_args, resolve_confinement_with,
};

#[test]
fn shared_socket_directory_is_fixed_private_and_independent_of_the_state_directory() {
    let path = shared_daemon_socket_directory().unwrap();
    assert_eq!(
        path.parent(),
        Some(std::fs::canonicalize("/tmp").unwrap().as_path())
    );
    assert!(
        path.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("ra-daemon-")
    );
    let name = path.file_name().unwrap().to_str().unwrap();
    let prefix = name.rsplit_once('-').unwrap().0;
    let longest = Path::new("/private/tmp")
        .join(format!("{prefix}-{}", u32::MAX))
        .join("f".repeat(64));
    assert!(
        longest.as_os_str().len() <= 103,
        "physical path must fit Darwin sun_path"
    );
    assert_eq!(prepare_shared_daemon_socket_directory().unwrap(), path);
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[derive(Debug)]
struct Available;

impl SandboxBackend for Available {
    fn name(&self) -> &'static str {
        "policy-fixture"
    }
    fn available_level(&self) -> Result<SandboxLevel, SandboxUnavailable> {
        Ok(SandboxLevel::Isolated)
    }
    fn confine(
        &self,
        _request: &ConfinementRequest,
        command: SandboxCommand,
    ) -> Result<SandboxCommand, SandboxError> {
        Ok(command)
    }
}

fn request(roots: &[&Path]) -> ConfinementRequest {
    request_at(SandboxLevel::WorkspaceWrite, roots)
}

fn request_at(level: SandboxLevel, roots: &[&Path]) -> ConfinementRequest {
    let mut policy = SandboxPolicy::new(level).with_network(NetworkAccess::Allowed);
    for root in roots {
        policy = policy.with_writable_root(root);
    }
    resolve_confinement_with(Some(&Available), &policy, None)
        .unwrap()
        .request()
        .unwrap()
        .clone()
}

#[test]
fn seatbelt_denies_control_socket_access_after_all_grants_and_protects_ancestors() {
    let profile =
        ra_exec::sandbox::seatbelt::build_profile(&request(&[Path::new("/tmp")])).unwrap();
    let policy = profile.policy();
    let grants = policy.rfind("(allow ").unwrap();
    assert!(policy.find("(deny network-outbound").unwrap() > grants);
    assert!(policy.find("(deny file-read* file-write*").unwrap() > grants);
    assert!(policy.contains("(deny file-write-unlink (require-all (vnode-type DIRECTORY)"));
    assert!(
        profile
            .params()
            .iter()
            .any(|(key, path)| key == "CONTROL_SOCKET_DIRECTORY"
                && *path == shared_daemon_socket_directory().unwrap())
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn daemon_socket_is_denied_despite_network_and_tmp_write_grants() {
    let root = prepare_shared_daemon_socket_directory().unwrap();
    let private = tempfile::tempdir_in(&root).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let socket = private.path().join("rpc.sock");
    let alias = workspace.path().join("alias.sock");
    let unrelated = workspace.path().join("other.sock");
    let _daemon = UnixListener::bind(&socket).unwrap();
    let _other = UnixListener::bind(&unrelated).unwrap();
    std::os::unix::fs::symlink(&socket, &alias).unwrap();
    std::os::unix::net::UnixStream::connect(&alias).expect("host can reach daemon");
    let script = r#"
import os, socket, sys
for path in sys.argv[1:3]:
    try:
        socket.socket(socket.AF_UNIX).connect(path)
    except OSError:
        pass
    else:
        raise AssertionError('daemon reachable: ' + path)
try:
    os.link(sys.argv[1], sys.argv[4] + '/hardlink')
except OSError:
    pass
else:
    raise AssertionError('daemon hardlink created')
socket.socket(socket.AF_UNIX).connect(sys.argv[3])
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[4] + '/local.sock')
s.listen(1)
c = socket.socket(socket.AF_UNIX)
c.connect(sys.argv[4] + '/local.sock')
p, _ = s.accept()
c.sendall(b'ok')
assert p.recv(2) == b'ok'
"#;
    let request = request(&[Path::new("/tmp"), workspace.path()]);
    let command = ra_exec::sandbox::SandboxCommand::new(
        "/usr/bin/python3",
        [
            "-c".to_owned(),
            script.to_owned(),
            socket.display().to_string(),
            alias.display().to_string(),
            unrelated.display().to_string(),
            workspace.path().display().to_string(),
        ],
    );
    let command = ra_exec::sandbox::platform_backend()
        .unwrap()
        .confine(&request, command)
        .unwrap();
    let output = command.prepare().unwrap().output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The six arguments Codex's `append_daemon_socket_masks` adds for one directory.
fn mask(directory: &Path) -> Vec<String> {
    let directory = directory.to_str().unwrap().to_owned();
    vec![
        "--perms".to_owned(),
        "000".to_owned(),
        "--tmpfs".to_owned(),
        directory.clone(),
        "--remount-ro".to_owned(),
        directory,
    ]
}

fn masks_after(args: &[String], bind: [&str; 3]) -> Vec<String> {
    let position = args
        .windows(3)
        .position(|window| window == bind)
        .unwrap_or_else(|| panic!("{bind:?} missing from {args:?}"));
    args[position + 3..].iter().take(6).cloned().collect()
}

/// A path whose mask cannot be written as UTF-8; protecting it always fails.
fn non_utf8_directory() -> PathBuf {
    PathBuf::from(OsString::from_vec(b"/tmp/ra-daemon-\xff".to_vec()))
}

/// Adapted from Codex's `read_only_tmp_carveout_keeps_daemon_sockets_masked`: every bind that
/// exposes the directory is followed at once by its mask, the read-only world and a writable root
/// alike, so the later writable bind does not reopen what the first mask hid.
#[test]
fn every_bind_exposing_the_socket_directory_is_followed_by_its_mask() {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let directory = root.join("ra-daemon-1000");
    let args = protected_args(&request(&[&root]), &BTreeSet::from([directory.clone()])).unwrap();
    let root = root.to_str().unwrap();
    assert_eq!(
        masks_after(&args, ["--ro-bind", "/", "/"]),
        mask(&directory)
    );
    assert_eq!(masks_after(&args, ["--bind", root, root]), mask(&directory));
    assert_eq!(
        args.iter().filter(|arg| *arg == "--perms").count(),
        2,
        "{args:?}"
    );
}

#[test]
fn a_platform_root_is_masked_and_a_missing_one_is_skipped() {
    let directory = Path::new("/usr/ra-daemon-1000");
    let args = protected_args(
        &request_at(SandboxLevel::Isolated, &[]),
        &BTreeSet::from([directory.to_path_buf()]),
    )
    .unwrap();
    assert_eq!(
        masks_after(&args, ["--ro-bind-try", "/usr", "/usr"]),
        mask(directory)
    );
    if !Path::new("/nix/store").exists() {
        assert_ne!(
            masks_after(&args, ["--ro-bind-try", "/nix/store", "/nix/store"]).first(),
            Some(&"--perms".to_owned())
        );
    }
}

fn unavailable_reason(error: &SandboxError) -> &str {
    match error {
        SandboxError::Unavailable { source } => source.reason(),
        other => panic!("expected an unavailable backend: {other:?}"),
    }
}

/// Codex's `non_utf8_daemon_socket_mask_path_is_rejected`.
#[test]
fn a_mask_path_that_is_not_utf8_is_refused() {
    let error = protected_args(&request(&[]), &BTreeSet::from([non_utf8_directory()])).unwrap_err();
    assert!(unavailable_reason(&error).contains("UTF-8"), "{error:?}");
}

#[test]
fn a_mount_root_inside_the_socket_directory_is_refused() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = std::fs::canonicalize(temporary.path()).unwrap();
    let inside = directory.join("nested");
    std::fs::create_dir(&inside).unwrap();
    let error = protected_args(&request(&[&inside]), &BTreeSet::from([directory])).unwrap_err();
    assert!(
        unavailable_reason(&error).contains("cannot be a sandbox mount root"),
        "{error:?}"
    );
}

/// Codex adds neither the Seatbelt denies nor the bubblewrap masks when writes are unrestricted,
/// and does not inspect the socket directory for them.
#[test]
fn unrestricted_writes_skip_control_socket_protection_as_in_codex() {
    let request = request(&[Path::new("/")]);
    let args = protected_args(&request, &BTreeSet::from([non_utf8_directory()]))
        .expect("the directories are not consulted");
    assert!(!args.iter().any(|arg| arg == "--perms"), "{args:?}");
    let profile = ra_exec::sandbox::seatbelt::build_profile(&request).unwrap();
    assert!(!profile.policy().contains("CONTROL_SOCKET"));
    assert!(
        profile
            .params()
            .iter()
            .all(|(key, _)| !key.starts_with("CONTROL_SOCKET"))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unrestricted_writes_leave_the_daemon_socket_reachable() {
    let root = prepare_shared_daemon_socket_directory().unwrap();
    let private = tempfile::tempdir_in(&root).unwrap();
    let socket = private.path().join("rpc.sock");
    let _daemon = UnixListener::bind(&socket).unwrap();
    let command = ra_exec::sandbox::SandboxCommand::new(
        "/usr/bin/python3",
        [
            "-c".to_owned(),
            "import socket, sys; socket.socket(socket.AF_UNIX).connect(sys.argv[1])".to_owned(),
            socket.display().to_string(),
        ],
    );
    let command = ra_exec::sandbox::platform_backend()
        .unwrap()
        .confine(&request(&[Path::new("/")]), command)
        .unwrap();
    let output = command.prepare().unwrap().output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
