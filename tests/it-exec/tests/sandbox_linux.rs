//! Linux kernel enforcement tests. The test host must provide bubblewrap and Python 3.
#![cfg(target_os = "linux")]

use std::{os::unix::net::UnixListener, path::Path};

use ra_exec::{
    command::ExecRequest,
    sandbox::{ExecEnvironment, NetworkAccess, SandboxLevel, SandboxPolicy},
    session::{ExecExecutionResult, ProcessManager},
};

fn manager(root: &Path, level: SandboxLevel, network: NetworkAccess) -> ProcessManager {
    ProcessManager::default().with_environment(
        ExecEnvironment::new().with_sandbox(
            SandboxPolicy::new(level)
                .with_network(network)
                .with_writable_root(root),
        ),
    )
}

#[cfg(target_arch = "x86_64")]
#[tokio::test]
async fn x32_syscall_numbers_cannot_bypass_the_filter() {
    let root = tempfile::tempdir().expect("workspace");
    let probe = "/usr/bin/python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); r=c.syscall(0x40000029,1,1,0); assert r == -1 and ctypes.get_errno() == 1, (r,ctypes.get_errno())'";
    let result = run(
        &manager(
            root.path(),
            SandboxLevel::WorkspaceWrite,
            NetworkAccess::Denied,
        ),
        root.path(),
        probe,
        root.path(),
    )
    .await;
    assert_eq!(result.exit_code(), Some(0), "{}", result.stderr());
}

async fn run(
    manager: &ProcessManager,
    root: &Path,
    command: &str,
    argument: &Path,
) -> ra_exec::output::ExecOutputSummary {
    let request = ExecRequest::new(command)
        .with_login(false)
        .with_cwd(root)
        .with_args([
            "probe".to_owned(),
            argument.to_str().expect("UTF-8 fixture").to_owned(),
        ]);
    match manager
        .execute(request, None)
        .await
        .expect("sandbox must be available")
    {
        ExecExecutionResult::Completed(summary) => summary,
        other => panic!("probe did not complete: {other:?}"),
    }
}

#[tokio::test]
async fn denied_network_cannot_connect_to_a_host_pathname_socket() {
    let root = tempfile::tempdir().expect("workspace");
    let socket = root.path().join("host.sock");
    let _listener = UnixListener::bind(&socket).expect("host listener");
    let probe = "/usr/bin/python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); print(\"connected\")' \"$1\"";
    for level in [SandboxLevel::WorkspaceWrite, SandboxLevel::Isolated] {
        let allowed = run(
            &manager(root.path(), level, NetworkAccess::Allowed),
            root.path(),
            probe,
            &socket,
        )
        .await;
        assert_eq!(
            allowed.exit_code(),
            Some(0),
            "control: {}",
            allowed.stderr()
        );
        assert!(allowed.stdout().contains("connected"));
        let denied = run(
            &manager(root.path(), level, NetworkAccess::Denied),
            root.path(),
            probe,
            &socket,
        )
        .await;
        assert_ne!(denied.exit_code(), Some(0));
        assert!(
            denied.stderr().contains("PermissionError"),
            "must reach the denied syscall: {}",
            denied.stderr()
        );
        assert!(!denied.stdout().contains("connected"));
    }
}

#[tokio::test]
async fn sandbox_preserves_shell_execution_and_confines_files() {
    let root = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir_in("/var/tmp").expect("outside private /tmp");
    let secret = outside.path().join("secret");
    std::fs::write(&secret, "outside-data").expect("secret");
    for level in [SandboxLevel::WorkspaceWrite, SandboxLevel::Isolated] {
        let manager = manager(root.path(), level, NetworkAccess::Denied);
        let inside = run(
            &manager,
            root.path(),
            "echo inside > inside; cat inside",
            &secret,
        )
        .await;
        assert_eq!(inside.exit_code(), Some(0), "{}", inside.stderr());
        assert_eq!(inside.stdout().trim(), "inside");
        let write = run(&manager, root.path(), "echo changed > \"$1\"", &secret).await;
        assert_ne!(write.exit_code(), Some(0));
        assert_eq!(
            std::fs::read_to_string(&secret).expect("secret intact"),
            "outside-data"
        );
        let read = run(&manager, root.path(), "cat \"$1\"", &secret).await;
        assert_eq!(
            read.exit_code() == Some(0),
            level == SandboxLevel::WorkspaceWrite
        );
        assert_eq!(
            read.sandbox().expect("report").network(),
            NetworkAccess::Denied
        );
    }
}
