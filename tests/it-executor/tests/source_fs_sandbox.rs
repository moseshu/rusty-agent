// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_filesystem::models::PermissionProfile;
use ra_filesystem::permissions::FileSystemAccessMode;
use ra_filesystem::permissions::FileSystemPath;
use ra_filesystem::permissions::FileSystemSandboxEntry;
use ra_filesystem::permissions::FileSystemSandboxPolicy;
use ra_filesystem::permissions::FileSystemSpecialPath;
use ra_filesystem::permissions::NetworkSandboxPolicy;
use ra_filesystem::sandbox::SandboxOverride;
use ra_paths::AbsolutePathBuf;
use ra_paths::PathUri;
use std::collections::HashMap;
use std::ffi::OsString;

use ra_exec::filesystem::test_api::fs_sandbox::FileSystemSandboxRunner;
use ra_exec::filesystem::test_api::fs_sandbox::SandboxCwd;
use ra_exec::filesystem::test_api::fs_sandbox::add_helper_runtime_permissions;
use ra_exec::filesystem::test_api::fs_sandbox::bind_windows_cwd_relative_deny_read_globs;
use ra_exec::filesystem::test_api::fs_sandbox::helper_env;
use ra_exec::filesystem::test_api::fs_sandbox::helper_env_from_vars;
use ra_exec::filesystem::test_api::fs_sandbox::helper_env_key_is_allowed;
use ra_exec::filesystem::test_api::fs_sandbox::helper_read_roots;
use ra_exec::filesystem::test_api::fs_sandbox::sandbox_cwd;
use ra_executor::ExecServerRuntimeOptions;
#[test]
fn helper_permissions_enable_minimal_reads_for_restricted_profile() {
    let cwd =
        AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path()).expect("absolute cwd");
    let mut policy = restricted_policy(Vec::new());
    add_helper_runtime_permissions(&mut policy, &[], cwd.as_path());
    assert!(policy.include_platform_defaults());
}
#[test]
fn helper_permissions_enable_minimal_reads_for_restricted_profile_with_writes() {
    let cwd =
        AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path()).expect("absolute cwd");
    let mut policy = restricted_policy(vec![path_entry(
        cwd.join("writable"),
        FileSystemAccessMode::Write,
    )]);
    add_helper_runtime_permissions(&mut policy, &[], cwd.as_path());
    assert!(policy.include_platform_defaults());
}
#[test]
fn helper_permissions_preserve_existing_writes() {
    let codex_self_exe = std::env::current_exe().expect("current exe");
    let runtime_paths = ExecServerRuntimeOptions::new(codex_self_exe, None).expect("runtime paths");
    let cwd =
        AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path()).expect("absolute cwd");
    let writable = cwd.join("writable");
    let mut policy = restricted_policy(vec![path_entry(
        writable.clone(),
        FileSystemAccessMode::Write,
    )]);
    let readable = runtime_paths.codex_self_exe.clone();
    add_helper_runtime_permissions(
        &mut policy,
        &helper_read_roots(&runtime_paths),
        cwd.as_path(),
    );
    assert!(policy.can_read_local_path_with_cwd(readable.as_path(), cwd.as_path()));
    assert!(policy.can_write_local_path_with_cwd(writable.as_path(), cwd.as_path()));
}
#[test]
fn helper_env_carries_only_allowlisted_runtime_vars() {
    let env = helper_env();
    let expected = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.to_string_lossy();
            helper_env_key_is_allowed(&key)
                .then(|| (key.into_owned(), value.to_string_lossy().into_owned()))
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(env, expected);
}
#[test]
fn helper_env_preserves_path_for_system_bwrap_discovery_without_leaking_secrets() {
    let env = helper_env_from_vars(
        [
            ("PATH", "/usr/bin:/bin"),
            ("TMPDIR", "/tmp/codex"),
            ("TMP", "/tmp"),
            ("TEMP", "/tmp"),
            ("HOME", "/home/user"),
            ("OPENAI_API_KEY", "secret"),
            ("HTTPS_PROXY", "http://proxy.example"),
        ]
        .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    assert_eq!(
        env,
        HashMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("TMPDIR".to_string(), "/tmp/codex".to_string()),
            ("TMP".to_string(), "/tmp".to_string()),
            ("TEMP".to_string(), "/tmp".to_string()),
        ])
    );
}
#[cfg(target_os = "macos")]
#[test]
fn helper_env_preserves_corefoundation_text_encoding() {
    let env = helper_env_from_vars(
        [
            ("__CF_USER_TEXT_ENCODING", "0x1F6:0x0:0x0"),
            ("HOME", "/Users/test"),
        ]
        .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    assert_eq!(
        env,
        HashMap::from([(
            "__CF_USER_TEXT_ENCODING".to_string(),
            "0x1F6:0x0:0x0".to_string(),
        )])
    );
}
#[cfg(windows)]
#[test]
fn helper_env_preserves_windows_runtime_variables_without_leaking_secrets() {
    let env = helper_env_from_vars(
        [
            ("Path", r"C:\Windows\System32"),
            ("LocalAppData", r"C:\Users\test\AppData\Local"),
            ("SystemDrive", "C:"),
            ("PATH_INJECTION", "bad"),
            ("OPENAI_API_KEY", "secret"),
        ]
        .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    assert_eq!(
        env,
        HashMap::from([
            ("Path".to_string(), r"C:\Windows\System32".to_string()),
            (
                "LocalAppData".to_string(),
                r"C:\Users\test\AppData\Local".to_string()
            ),
            ("SystemDrive".to_string(), "C:".to_string()),
        ])
    );
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn sandbox_exec_request_carries_helper_env() {
    let Some((path_key, path)) = std::env::vars_os().find(|(key, _)| {
        let key = key.to_string_lossy();
        key == "PATH" || (cfg!(windows) && key.eq_ignore_ascii_case("PATH"))
    }) else {
        return;
    };
    let path_key = path_key.to_string_lossy().into_owned();
    let path = path.to_string_lossy().into_owned();
    let codex_self_exe = std::env::current_exe().expect("current exe");
    let runtime_paths = ExecServerRuntimeOptions::new(codex_self_exe.clone(), Some(codex_self_exe))
        .expect("runtime paths");
    let runner = FileSystemSandboxRunner::new(runtime_paths);
    let native_cwd = AbsolutePathBuf::current_dir().expect("cwd");
    let cwd = PathUri::from_abs_path(&native_cwd);
    let file_system_policy = restricted_policy(vec![
        #[cfg(windows)]
        special_entry(FileSystemSpecialPath::Root, FileSystemAccessMode::Read),
        path_entry(native_cwd.clone(), FileSystemAccessMode::Write),
    ]);
    let network_policy = NetworkSandboxPolicy::Restricted;
    let permission_profile =
        PermissionProfile::from_runtime_permissions(&file_system_policy, network_policy);
    let sandbox_context = sandbox_context_with_cwd(&file_system_policy, cwd.clone());
    let sandbox_cwd = SandboxCwd {
        uri: cwd,
        native: native_cwd,
    };
    #[cfg(windows)]
    let sandbox_context = {
        let error = runner
            .sandbox_exec_request(
                &permission_profile,
                &sandbox_cwd,
                std::slice::from_ref(&sandbox_cwd.native),
                &sandbox_context,
            )
            .expect_err("disabled Windows sandbox must not run the helper unsandboxed");
        assert_eq!(
            error.message,
            "filesystem sandbox cannot be enforced on this executor"
        );
        ra_executor::FileSystemSandboxContext {
            windows_sandbox_selection: ra_filesystem::WindowsSandboxSelection::RestrictedToken,
            ..sandbox_context
        }
    };
    let request = runner
        .sandbox_exec_request(
            &permission_profile,
            &sandbox_cwd,
            std::slice::from_ref(&sandbox_cwd.native),
            &sandbox_context,
        )
        .expect("sandbox exec request");
    assert_eq!(request.env.get(&path_key), Some(&path));
}
#[doc = " Removing the selected directory must not change permission anchoring or prevent launch."]
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn sandbox_exec_request_uses_filesystem_root_and_preserves_policy_cwd() {
    let codex_self_exe = std::env::current_exe().expect("current exe");
    let runtime_paths = ExecServerRuntimeOptions::new(codex_self_exe.clone(), Some(codex_self_exe))
        .expect("runtime paths");
    let runner = FileSystemSandboxRunner::new(runtime_paths);
    let selected = tempfile::tempdir().expect("selected directory");
    let selected_cwd = AbsolutePathBuf::from_absolute_path(selected.path()).expect("cwd");
    let root = selected_cwd.ancestors().last().expect("filesystem root");
    let policy = restricted_policy(vec![special_entry(
        FileSystemSpecialPath::Root,
        FileSystemAccessMode::Read,
    )]);
    let mut sandbox_context =
        sandbox_context_with_cwd(&policy, PathUri::from_abs_path(&selected_cwd));
    sandbox_context.sandbox_override = SandboxOverride::EscalatedSandboxWithRestrictions;
    #[cfg(windows)]
    let sandbox_context = ra_executor::FileSystemSandboxContext {
        windows_sandbox_selection: ra_filesystem::WindowsSandboxSelection::RestrictedToken,
        ..sandbox_context
    };
    selected.close().expect("remove selected directory");
    let request = runner
        .sandbox_command(&sandbox_context)
        .expect("sandbox command");
    assert_eq!(
        (
            request.cwd,
            request.sandbox_policy_cwd,
            request.sandbox_override
        ),
        (
            PathUri::from_abs_path(&root),
            PathUri::from_abs_path(&selected_cwd),
            SandboxOverride::EscalatedSandboxWithRestrictions,
        )
    );
}
#[doc = " Tilde patterns stay home-relative when Windows sandbox commands start at another cwd."]
#[test]
fn windows_cwd_relative_denial_binding_keeps_tilde_patterns_home_relative() {
    let cwd = PathUri::parse("file:///C:/policy/checkout").expect("Windows policy cwd");
    let mut policy = restricted_policy(
        [
            "~",
            "~/",
            r"~\",
            "~/private/*.env",
            r"~\private\*.env",
            "hidden/*.env",
            "~someone/private",
            "D:/absolute/*.env",
        ]
        .into_iter()
        .map(|pattern| {
            FileSystemSandboxEntry::new(
                FileSystemPath::GlobPattern {
                    pattern: pattern.to_string(),
                },
                FileSystemAccessMode::Deny,
            )
        })
        .collect(),
    );
    bind_windows_cwd_relative_deny_read_globs(&mut policy, &cwd).expect("bind Windows policy");
    let patterns = policy
        .entries
        .into_iter()
        .filter_map(|entry| match entry.path {
            FileSystemPath::GlobPattern { pattern } => Some(pattern),
            FileSystemPath::Path { .. } | FileSystemPath::Special { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        patterns,
        [
            "~",
            "~/",
            r"~\",
            "~/private/*.env",
            r"~\private\*.env",
            r"C:\policy\checkout\hidden\*.env",
            r"C:\policy\checkout\~someone\private",
            "D:/absolute/*.env",
        ]
    );
}
#[test]
fn sandbox_cwd_uses_context_cwd() {
    let native_cwd =
        AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path()).expect("absolute cwd");
    let cwd = PathUri::from_abs_path(&native_cwd);
    let policy = restricted_policy(vec![special_entry(
        FileSystemSpecialPath::project_roots(None),
        FileSystemAccessMode::Write,
    )]);
    let sandbox_context = sandbox_context_with_cwd(&policy, cwd.clone());
    assert_eq!(
        sandbox_cwd(&sandbox_context).expect("sandbox cwd"),
        SandboxCwd {
            uri: cwd,
            native: native_cwd
        }
    );
}
#[test]
fn sandbox_cwd_rejects_non_native_context_cwd_without_fallback() {
    let cwd = non_native_cwd();
    let policy = restricted_policy(vec![special_entry(
        FileSystemSpecialPath::project_roots(None),
        FileSystemAccessMode::Write,
    )]);
    let sandbox_context = sandbox_context_with_cwd(&policy, cwd.clone());
    let err = sandbox_cwd(&sandbox_context).expect_err("non-native cwd should be rejected");
    assert_eq!(
        err,
        ra_executor::test_api::rpc::invalid_request(format!(
            "'{cwd}' is invalid on '{}'",
            std::env::consts::OS
        ))
    );
}
#[test]
fn helper_permissions_include_only_the_helper_executable() {
    let codex_self_exe = std::env::current_exe().expect("current exe");
    let runtime_paths = ExecServerRuntimeOptions::new(codex_self_exe, None).expect("runtime paths");
    let cwd =
        AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path()).expect("absolute cwd");
    let mut policy = restricted_policy(Vec::new());
    let parent = runtime_paths
        .codex_self_exe
        .parent()
        .expect("current exe parent");
    let sibling = parent.join("credentials.json");
    add_helper_runtime_permissions(
        &mut policy,
        &helper_read_roots(&runtime_paths),
        cwd.as_path(),
    );
    assert!(
        policy.can_read_local_path_with_cwd(runtime_paths.codex_self_exe.as_path(), cwd.as_path(),)
    );
    assert!(!policy.can_read_local_path_with_cwd(parent.as_path(), cwd.as_path()));
    assert!(!policy.can_read_local_path_with_cwd(sibling.as_path(), cwd.as_path()));
}
#[test]
fn helper_permissions_include_only_linux_sandbox_alias_executable() {
    let root = tempfile::tempdir().expect("temp dir");
    let codex_self_exe = root.path().join("bin").join("codex");
    let codex_linux_sandbox_exe = root.path().join("aliases").join("codex-linux-sandbox");
    let runtime_paths =
        ExecServerRuntimeOptions::new(codex_self_exe, Some(codex_linux_sandbox_exe))
            .expect("runtime paths");
    let cwd =
        AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path()).expect("absolute cwd");
    let mut policy = restricted_policy(Vec::new());
    let codex_parent = runtime_paths.codex_self_exe.parent().expect("codex parent");
    let alias = runtime_paths
        .codex_linux_sandbox_exe
        .as_ref()
        .expect("linux sandbox alias");
    let alias_parent = alias.parent().expect("alias parent");
    add_helper_runtime_permissions(
        &mut policy,
        &helper_read_roots(&runtime_paths),
        cwd.as_path(),
    );
    assert!(
        policy.can_read_local_path_with_cwd(runtime_paths.codex_self_exe.as_path(), cwd.as_path(),)
    );
    assert!(policy.can_read_local_path_with_cwd(alias.as_path(), cwd.as_path()));
    assert!(!policy.can_read_local_path_with_cwd(codex_parent.as_path(), cwd.as_path()));
    assert!(!policy.can_read_local_path_with_cwd(alias_parent.as_path(), cwd.as_path()));
}
fn restricted_policy(entries: Vec<FileSystemSandboxEntry>) -> FileSystemSandboxPolicy {
    FileSystemSandboxPolicy::restricted(entries)
}
fn sandbox_context_with_cwd(
    policy: &FileSystemSandboxPolicy,
    cwd: PathUri,
) -> ra_executor::FileSystemSandboxContext {
    ra_filesystem::FileSystemSandboxContext::from_permission_profile(
        PermissionProfile::from_runtime_permissions(policy, NetworkSandboxPolicy::Restricted),
        cwd,
    )
}
fn non_native_cwd() -> PathUri {
    #[cfg(unix)]
    let uri = "file://server/share/checkout";
    #[cfg(windows)]
    let uri = "file:///usr/local/checkout";
    PathUri::parse(uri).expect("non-native cwd URI")
}
fn path_entry(path: AbsolutePathBuf, access: FileSystemAccessMode) -> FileSystemSandboxEntry {
    FileSystemSandboxEntry {
        path: path.into(),
        access,
        missing_path_behavior: None,
    }
}
fn special_entry(
    value: FileSystemSpecialPath,
    access: FileSystemAccessMode,
) -> FileSystemSandboxEntry {
    FileSystemSandboxEntry {
        path: FileSystemPath::Special { value },
        access,
        missing_path_behavior: None,
    }
}
