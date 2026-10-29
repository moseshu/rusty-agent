// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Privileged daemon sockets stay unreachable from a Seatbelt child, despite full network and
//! `/tmp` write grants.
//!
//! Codex's case also passes the daemon root as an explicit Unix-socket allowance, a managed
//! network input that is not ported; with network enabled every other Unix socket is already
//! reachable, so the case keeps its assertions without it.

#![cfg(target_os = "macos")]

use super::CreateSeatbeltCommandArgsParams;
use super::MACOS_PATH_TO_SEATBELT_EXECUTABLE;
use super::create_seatbelt_command_args;
use ra_filesystem::permissions::FileSystemSandboxPolicy;
use ra_filesystem::permissions::NetworkSandboxPolicy;
use ra_paths::AbsolutePathBuf;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::process::Command;

#[test]
fn daemon_sockets_are_denied_despite_network_and_tmp_write_grants() {
    let root = ra_uds::prepare_shared_daemon_socket_directory().unwrap();
    let private = tempfile::tempdir_in(&root).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let socket = private.path().join("rpc.sock");
    let alias = workspace.path().join("alias.sock");
    let unrelated = workspace.path().join("other.sock");
    let _daemon = UnixListener::bind(&socket).unwrap();
    let _other = UnixListener::bind(&unrelated).unwrap();
    std::os::unix::fs::symlink(&socket, &alias).unwrap();
    UnixStream::connect(&alias).expect("host can reach daemon");
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
    let command = vec![
        "/usr/bin/python3".to_string(),
        "-c".to_string(),
        script.to_string(),
        socket.display().to_string(),
        alias.display().to_string(),
        unrelated.display().to_string(),
        workspace.path().display().to_string(),
    ];
    let policy = FileSystemSandboxPolicy::workspace_write(
        &[AbsolutePathBuf::from_absolute_path("/tmp").unwrap()],
        /*exclude_tmpdir_env_var*/ false,
        /*exclude_slash_tmp*/ false,
    );
    let args = create_seatbelt_command_args(CreateSeatbeltCommandArgsParams {
        command,
        file_system_sandbox_policy: &policy,
        network_sandbox_policy: NetworkSandboxPolicy::Enabled,
        sandbox_policy_cwd: workspace.path(),
        enforce_managed_network: false,
        managed_network: None,
        environment_id: None,
        network: None,
        extra_allow_unix_sockets: &[],
    })
    .unwrap();
    let output = Command::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE)
        .args(args)
        .env("RA_HOME", workspace.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
