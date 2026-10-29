//! Canonical permissions enforced by the real command launch path, including retained terminals.
#![cfg(target_os = "macos")]

use ra_exec::{
    command::ExecRequest,
    output::ExecOutputSummary,
    sandbox::{ExecEnvironment, SandboxLevel, SandboxPolicy},
    session::{ExecExecutionResult, ProcessManager},
};
use ra_filesystem::{
    legacy::SandboxPolicy as LegacyPolicy,
    models::PermissionProfile,
    permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
        NetworkSandboxPolicy,
    },
};
use ra_paths::AbsolutePathBuf;
use std::{fs, path::Path};

fn workspace_policy(root: &Path) -> FileSystemSandboxPolicy {
    FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
        &LegacyPolicy::new_workspace_write_policy(),
        root,
    )
}
fn manager(root: &Path, file_system: FileSystemSandboxPolicy) -> ProcessManager {
    let profile =
        PermissionProfile::from_runtime_permissions(&file_system, NetworkSandboxPolicy::Restricted);
    ProcessManager::default().with_environment(ExecEnvironment::new().with_sandbox(
        SandboxPolicy::new(SandboxLevel::WorkspaceWrite).with_permission_profile(profile, root),
    ))
}
async fn run(manager: &ProcessManager, root: &Path, command: &str) -> ExecOutputSummary {
    match manager
        .execute(ExecRequest::new(command).with_cwd(root), None)
        .await
        .unwrap()
    {
        ExecExecutionResult::Completed(summary) => summary,
        other => panic!("command did not finish: {other:?}"),
    }
}
#[tokio::test]
async fn workspace_commands_cannot_write_protected_metadata_or_read_only_subpaths() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for directory in [
        ".git",
        ".rusty-agent/rules",
        ".rusty-agent/hooks",
        "read-only",
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
        fs::write(root.join(directory).join("config"), "original").unwrap();
    }
    let mut policy = workspace_policy(&root);
    policy.entries.push(FileSystemSandboxEntry::new(
        AbsolutePathBuf::from_absolute_path(root.join("read-only"))
            .unwrap()
            .into(),
        FileSystemAccessMode::Read,
    ));
    let manager = manager(&root, policy);
    let allowed = run(&manager, &root, "printf allowed > ordinary.txt").await;
    assert_eq!(allowed.exit_code(), Some(0), "{allowed:?}");
    for path in [
        ".git/config",
        ".rusty-agent/rules/config",
        ".rusty-agent/hooks/config",
        "read-only/config",
    ] {
        let result = run(&manager, &root, &format!("printf changed > '{path}'")).await;
        assert_ne!(result.exit_code(), Some(0), "{path}: {result:?}");
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), "original");
    }
    let renamed = run(&manager, &root, "mv .rusty-agent relocated").await;
    assert_ne!(renamed.exit_code(), Some(0), "{renamed:?}");
    assert!(root.join(".rusty-agent/rules/config").exists());
}
#[tokio::test]
async fn deny_read_paths_and_globs_are_enforced_by_shell_children() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir(root.join("private")).unwrap();
    fs::write(root.join("private/token"), "secret-token").unwrap();
    fs::write(root.join("password.secret"), "secret-password").unwrap();
    fs::write(root.join("public"), "public-data").unwrap();
    let mut policy = workspace_policy(&root);
    policy.entries.extend([
        FileSystemSandboxEntry::new(
            AbsolutePathBuf::from_absolute_path(root.join("private"))
                .unwrap()
                .into(),
            FileSystemAccessMode::Deny,
        ),
        FileSystemSandboxEntry::new(
            FileSystemPath::GlobPattern {
                pattern: format!("{}/**/*.secret", root.display()),
            },
            FileSystemAccessMode::Deny,
        ),
    ]);
    let manager = manager(&root, policy);
    for path in ["private/token", "password.secret"] {
        let result = run(&manager, &root, &format!("/bin/sh -c 'cat {path}'")).await;
        assert_ne!(result.exit_code(), Some(0), "{path}: {result:?}");
        assert!(!result.stdout().contains("secret-"), "{result:?}");
    }
    assert_eq!(
        run(&manager, &root, "cat public").await.stdout(),
        "public-data"
    );
}
#[tokio::test]
async fn protected_metadata_cannot_be_created_after_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let manager = manager(&root, workspace_policy(&root));
    for name in [".git", ".rusty-agent"] {
        let result = run(&manager, &root, &format!("mkdir {name}")).await;
        assert_ne!(result.exit_code(), Some(0), "{result:?}");
        assert!(!root.join(name).exists());
    }
}

#[tokio::test]
async fn background_terminal_retains_the_launch_sandbox_for_stdin_commands() {
    use ra_exec::command::ExecLimits;
    use std::time::Duration;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir(root.join(".rusty-agent")).unwrap();
    fs::write(root.join(".rusty-agent/config"), "original").unwrap();
    let manager = manager(&root, workspace_policy(&root));
    let started = manager
        .execute(
            ExecRequest::new("/bin/sh").with_cwd(&root).with_limits(
                ExecLimits::new().with_initial_yield_timeout(Duration::from_millis(100)),
            ),
            None,
        )
        .await
        .unwrap();
    let ExecExecutionResult::Yielded { session_id, .. } = started else {
        panic!("interactive shell must stay running")
    };
    manager
        .write_stdin(
            &session_id,
            Some("printf changed > .rusty-agent/config; printf ordinary > result; exit\n"),
            false,
            None,
        )
        .await
        .unwrap();
    let summary = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let summary = manager.get_output_summary(&session_id).await.unwrap();
            if summary.exit_code().is_some() {
                break summary;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        summary.stderr().contains("Operation not permitted"),
        "{summary:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join(".rusty-agent/config")).unwrap(),
        "original"
    );
    assert_eq!(fs::read_to_string(root.join("result")).unwrap(), "ordinary");
}

#[tokio::test]
async fn canonical_network_switch_controls_real_connections() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0; 2048];
        assert!(stream.read(&mut buffer).unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
    });
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let request = format!(
        "/usr/bin/curl --noproxy '*' --connect-timeout 1 --max-time 2 -s http://127.0.0.1:{port}"
    );
    let denied = manager(&root, workspace_policy(&root));
    assert_ne!(run(&denied, &root, &request).await.exit_code(), Some(0));
    let profile = PermissionProfile::from_runtime_permissions(
        &workspace_policy(&root),
        NetworkSandboxPolicy::Enabled,
    );
    let allowed = ProcessManager::default().with_environment(ExecEnvironment::new().with_sandbox(
        SandboxPolicy::new(SandboxLevel::WorkspaceWrite).with_permission_profile(profile, &root),
    ));
    let response = run(&allowed, &root, &request).await;
    assert_eq!(response.exit_code(), Some(0), "{response:?}");
    assert_eq!(response.stdout(), "ok");
    server.join().unwrap();
}

#[tokio::test]
async fn canonical_preparation_errors_preserve_the_backend_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let target = root.join("target");
    std::fs::create_dir(&target).unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry::new(
            FileSystemPath::Special {
                value: ra_filesystem::permissions::FileSystemSpecialPath::Root,
            },
            FileSystemAccessMode::Read,
        ),
        FileSystemSandboxEntry::new(
            AbsolutePathBuf::from_absolute_path(alias).unwrap().into(),
            FileSystemAccessMode::Write,
        ),
    ]);
    let error = manager(&root, policy)
        .execute(
            ExecRequest::new("touch target/spawned").with_cwd(&root),
            None,
        )
        .await
        .unwrap_err();
    let ra_exec::session::ExecError::Sandbox { source } = error else {
        panic!("expected sandbox preparation failure: {error:?}");
    };
    let text = source.to_string();
    assert!(
        text.contains("failed to prepare Seatbelt sandbox"),
        "{text}"
    );
    assert!(text.contains("symlinked writable root"), "{text}");
    assert!(!target.join("spawned").exists());
}

#[test]
fn canonical_permissions_do_not_silently_replace_a_selected_custom_backend() {
    use ra_exec::sandbox::{
        ConfinementRequest, SandboxBackend, SandboxCommand, SandboxError, SandboxUnavailable,
        resolve_confinement_with,
    };
    #[derive(Debug)]
    struct CustomBackend;
    impl SandboxBackend for CustomBackend {
        fn name(&self) -> &'static str {
            "custom"
        }
        fn available_level(&self) -> Result<SandboxLevel, SandboxUnavailable> {
            Ok(SandboxLevel::Isolated)
        }
        fn confine(
            &self,
            _: &ConfinementRequest,
            command: SandboxCommand,
        ) -> Result<SandboxCommand, SandboxError> {
            Ok(command)
        }
    }
    static BACKEND: CustomBackend = CustomBackend;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let policy = SandboxPolicy::new(SandboxLevel::WorkspaceWrite)
        .with_permission_profile(PermissionProfile::read_only(), &root);
    let confinement = resolve_confinement_with(Some(&BACKEND), &policy, None).unwrap();
    let error = confinement
        .apply(SandboxCommand::new("true", Vec::<String>::new()))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected backend does not support canonical"),
        "{error}"
    );
}
