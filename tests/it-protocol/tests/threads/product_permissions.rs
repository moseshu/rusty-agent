// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Named permission profiles through the product configuration, as Codex's app-server resolves
//! them: the `thread_resume`, `thread_fork`, `thread_settings_update` and `turn_start` cases about
//! persisted, overridden and refused profile selections.
//!
//! Each server loads its threads' configuration from a product home the case writes; a "cold"
//! resume is a new server on the same thread store, after the case has rewritten that home.
//! Codex's `config` request map and runtime workspace roots are not ported, so the cases that set
//! them keep only their other assertions.

use super::*;
use ra_coding::config::ConfigBuilder;
use ra_config::LoaderOverrides;

struct ProductHome {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    store: Arc<dyn ThreadStore>,
    script: Arc<Script>,
}

impl ProductHome {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            workspace: tempfile::tempdir().unwrap(),
            store: Arc::new(InMemoryThreadStore::new()),
            script: Arc::new(Script::default()),
        }
    }

    fn write_config(&self, contents: &str) {
        std::fs::write(self.home.path().join("config.toml"), contents).unwrap();
    }

    /// Codex's `MockResponsesConfig`: approval `never` and sandbox `read-only` unless replaced,
    /// then the case's root and extra configuration. Its mock model provider is not needed here.
    fn write_mock_config(&self, approval_policy: &str, root_config: &str, extra_config: &str) {
        self.write_config(&format!(
            "approval_policy = \"{approval_policy}\"\nsandbox_mode = \"read-only\"\n{root_config}\n\n{extra_config}\n"
        ));
    }

    fn write_requirements(&self, contents: &str) {
        std::fs::write(self.home.path().join("requirements.toml"), contents).unwrap();
    }

    fn write_managed_config(&self, contents: &str) {
        std::fs::write(self.home.path().join("managed_config.toml"), contents).unwrap();
    }

    fn config_builder(&self) -> ConfigBuilder {
        let home = self.home.path().canonicalize().unwrap();
        let loader_overrides = LoaderOverrides {
            managed_config_path: Some(home.join("managed_config.toml")),
            system_config_path: Some(home.join("system_config.toml")),
            system_requirements_path: Some(home.join("requirements.toml")),
            ..LoaderOverrides::without_managed_config_for_tests()
        };
        ConfigBuilder::default()
            .codex_home(home)
            .loader_overrides(loader_overrides)
    }

    /// A server whose threads load their configuration from this home, on the shared store.
    fn server(&self) -> Server {
        let root = self.workspace.path().canonicalize().unwrap();
        let host = CodingHost::open(&root).unwrap();
        let settings = CodingThreadSettingsAdapter::new(
            &root,
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        )
        .unwrap()
        .with_rules_home(self.home.path());
        let builder = self.config_builder();
        let factory =
            CodingThreadFactory::new(host, Arc::new(Resolver(Arc::clone(&self.script))), settings)
                .with_product_config(builder);
        build(
            Options {
                store: Some(Arc::clone(&self.store)),
                factory: Some(Arc::new(factory)),
                unload_delay: None,
            },
            None,
        )
        .0
    }
}

/// Starts a thread with `params` and runs one turn on it, so its settings are recorded.
async fn materialize_thread(home: &ProductHome, params: Value) -> String {
    let server = home.server();
    let mut connection = server.connect(1).await;
    let started = server.call(&mut connection, "thread/start", params).await;
    let thread_id = started["thread"]["id"].as_str().unwrap().to_owned();
    home.script.answer("Done");
    server
        .run_turn(&mut connection, &thread_id, "persist permission profile")
        .await;
    thread_id
}

fn write_dev_permission_config(home: &ProductHome, dev_extends: &str) {
    home.write_mock_config(
        "never",
        "default_permissions = \":danger-full-access\"",
        &format!("[permissions.dev]\nextends = \"{dev_extends}\""),
    );
}

#[tokio::test]
async fn cold_resume_reresolves_persisted_active_permission_profile() {
    let home = ProductHome::new();
    write_dev_permission_config(&home, ":workspace");
    let thread_id = materialize_thread(&home, json!({"permissions": "dev"})).await;

    write_dev_permission_config(&home, ":read-only");
    let server = home.server();
    let mut connection = server.connect(1).await;
    let resumed = server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread_id}),
        )
        .await;

    assert_eq!(resumed["sandbox"]["type"], "readOnly");
    assert_eq!(
        resumed["activePermissionProfile"],
        json!({"id": "dev", "extends": ":read-only"})
    );
}

#[tokio::test]
async fn cold_resume_with_removed_permission_profile_uses_configured_default() {
    let home = ProductHome::new();
    write_dev_permission_config(&home, ":workspace");
    let thread_id = materialize_thread(&home, json!({"permissions": "dev"})).await;

    home.write_mock_config("never", "default_permissions = \":danger-full-access\"", "");
    let server = home.server();
    let mut connection = server.connect(1).await;
    let resumed = server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread_id}),
        )
        .await;

    assert_eq!(resumed["sandbox"]["type"], "dangerFullAccess");
    assert_eq!(
        resumed["activePermissionProfile"],
        json!({"id": ":danger-full-access", "extends": null})
    );
}

#[tokio::test]
async fn cold_resume_permission_overrides_win_over_persisted_profile() {
    let home = ProductHome::new();
    write_dev_permission_config(&home, ":workspace");
    let thread_id = materialize_thread(&home, json!({"permissions": "dev"})).await;

    for (params, expected_active) in [
        (
            json!({"threadId": thread_id, "sandbox": "read-only"}),
            Value::Null,
        ),
        (
            json!({"threadId": thread_id, "permissions": ":read-only"}),
            json!({"id": ":read-only", "extends": null}),
        ),
    ] {
        let server = home.server();
        let mut connection = server.connect(1).await;
        let resumed = server.call(&mut connection, "thread/resume", params).await;
        assert_eq!(resumed["sandbox"]["type"], "readOnly");
        assert_eq!(resumed["activePermissionProfile"], expected_active);
    }
}

#[tokio::test]
async fn cold_resume_without_active_permission_profile_restores_base_permissions() {
    for (name, saved_sandbox, configured_default, requirements, expected) in [
        (
            "managed deny-read",
            "danger-full-access",
            ":workspace",
            "[permissions.filesystem]\ndeny_read = [\"./private\"]",
            json!({"type": "workspaceWrite", "writableRoots": [], "networkAccess": false,
                   "excludeTmpdirEnvVar": false, "excludeSlashTmp": false}),
        ),
        (
            "restore saved full access",
            "danger-full-access",
            ":workspace",
            "",
            json!({"type": "dangerFullAccess"}),
        ),
        (
            "managed modes",
            "danger-full-access",
            ":workspace",
            "allowed_sandbox_modes = [\"read-only\"]",
            json!({"type": "readOnly", "networkAccess": false}),
        ),
        (
            "managed catalog",
            "danger-full-access",
            ":workspace",
            "default_permissions = \":read-only\"\n[allowed_permission_profiles]\n\":read-only\" = true",
            json!({"type": "readOnly", "networkAccess": false}),
        ),
        (
            "saved read-only wins before incompatible default validation",
            "read-only",
            ":danger-full-access",
            "allowed_sandbox_modes = [\"read-only\"]",
            json!({"type": "readOnly", "networkAccess": false}),
        ),
    ] {
        let home = ProductHome::new();
        home.write_mock_config("never", "", "");
        let thread_id = materialize_thread(
            &home,
            json!({"sandbox": saved_sandbox, "approvalPolicy": "never"}),
        )
        .await;

        home.write_mock_config(
            "on-request",
            &format!("default_permissions = \"{configured_default}\""),
            "",
        );
        home.write_requirements(requirements);
        let server = home.server();
        let mut connection = server.connect(1).await;
        let resumed = server
            .call(
                &mut connection,
                "thread/resume",
                json!({"threadId": thread_id}),
            )
            .await;
        assert_eq!(
            (&resumed["sandbox"], &resumed["approvalPolicy"]),
            (&expected, &json!("never")),
            "{name}: requirements: {requirements}"
        );
    }
}

#[tokio::test]
async fn cold_resume_restores_profile_selected_by_settings_update() {
    let home = ProductHome::new();
    home.write_mock_config("never", "", "");
    let thread_id = {
        let server = home.server();
        let mut connection = server.connect(1).await;
        let started = server
            .call(&mut connection, "thread/start", json!({}))
            .await;
        let thread_id = started["thread"]["id"].as_str().unwrap().to_owned();
        home.script.answer("Done");
        server
            .run_turn(&mut connection, &thread_id, "persist permission profile")
            .await;
        server
            .call(
                &mut connection,
                "thread/settings/update",
                json!({"threadId": thread_id, "permissions": ":workspace"}),
            )
            .await;
        connection.notification("thread/settings/updated").await;
        thread_id
    };

    let server = home.server();
    let mut connection = server.connect(1).await;
    let resumed = server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread_id}),
        )
        .await;

    assert_eq!(resumed["sandbox"]["type"], "workspaceWrite");
    assert_eq!(
        resumed["activePermissionProfile"],
        json!({"id": ":workspace", "extends": null})
    );
}

#[tokio::test]
async fn thread_fork_preserves_persisted_permission_profile_and_honors_overrides() {
    let home = ProductHome::new();
    home.write_mock_config(
        "never",
        "default_permissions = \":danger-full-access\"",
        "[permissions.dev]\nextends = \":read-only\"",
    );
    let source = materialize_thread(
        &home,
        json!({"approvalPolicy": "on-request", "permissions": "dev"}),
    )
    .await;

    let server = home.server();
    let mut connection = server.connect(1).await;
    let forked = server
        .call(&mut connection, "thread/fork", json!({"threadId": source}))
        .await;
    assert_eq!(forked["sandbox"]["type"], "readOnly");
    assert_eq!(forked["approvalPolicy"], "on-request");
    assert_eq!(
        forked["activePermissionProfile"],
        json!({"id": "dev", "extends": ":read-only"})
    );

    let forked = server
        .call(
            &mut connection,
            "thread/fork",
            json!({"threadId": source, "approvalPolicy": "never", "sandbox": "danger-full-access"}),
        )
        .await;
    assert_eq!(forked["sandbox"]["type"], "dangerFullAccess");
    assert_eq!(forked["approvalPolicy"], "never");
    assert_eq!(forked["activePermissionProfile"], Value::Null);

    let forked = server
        .call(
            &mut connection,
            "thread/fork",
            json!({"threadId": source, "permissions": ":workspace"}),
        )
        .await;
    assert_eq!(forked["sandbox"]["type"], "workspaceWrite");
    assert_eq!(
        forked["activePermissionProfile"],
        json!({"id": ":workspace", "extends": null})
    );
}

#[tokio::test]
async fn thread_settings_update_rejects_sandbox_policy_with_permissions() {
    let home = ProductHome::new();
    home.write_config("");
    let server = home.server();
    let mut connection = server.connect(1).await;
    let started = server
        .call(&mut connection, "thread/start", json!({}))
        .await;
    let thread_id = started["thread"]["id"].as_str().unwrap();

    let error = server
        .call_error(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": thread_id, "sandboxPolicy": {"type": "dangerFullAccess"},
                   "permissions": ":workspace"}),
        )
        .await;

    assert_eq!(
        error["message"],
        "`permissions` cannot be combined with `sandboxPolicy`"
    );
}

/// Codex's case also selects the profile through its `config` request map, which is not ported.
#[tokio::test]
async fn thread_settings_update_preserves_session_profiles() {
    let home = ProductHome::new();
    home.write_config(
        "default_permissions = ':read-only'\n[permissions.audit]\nextends = ':read-only'\n",
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let started = server
        .call(
            &mut connection,
            "thread/start",
            json!({"permissions": "audit"}),
        )
        .await;
    let thread_id = started["thread"]["id"].as_str().unwrap().to_owned();
    for profile_id in [":workspace", "audit"] {
        server
            .call(
                &mut connection,
                "thread/settings/update",
                json!({"threadId": thread_id, "permissions": profile_id}),
            )
            .await;
        let applied = connection.notification("thread/settings/updated").await;
        assert_eq!(
            applied["threadSettings"]["activePermissionProfile"]["id"],
            profile_id
        );
    }
}

#[tokio::test]
async fn turn_start_rejects_invalid_permission_selection_before_starting_turn() {
    let home = ProductHome::new();
    home.write_mock_config("never", "", "");
    home.write_managed_config("sandbox_mode = \"read-only\"\n");
    let server = home.server();
    let mut connection = server.connect(1).await;
    let started = server
        .call(&mut connection, "thread/start", json!({}))
        .await;
    let thread_id = started["thread"]["id"].as_str().unwrap();

    let error = server
        .call_error(
            &mut connection,
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type": "text", "text": "Hello"}],
                   "permissions": ":danger-full-access"}),
        )
        .await;

    assert_eq!(error["code"], -32600);
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("`approval_policy = \"never\"` cannot be used"),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("requirements do not allow `sandbox_mode = \"danger-full-access\"`"),
        "unexpected error message: {message}"
    );
}

/// Not a Codex case: an explicit selection the requirements disallow is refused, as Codex's
/// `thread_settings_update` refuses it, and the thread keeps the profile it had.
#[tokio::test]
async fn thread_settings_update_refuses_profile_requirements_disallow() {
    let home = ProductHome::new();
    home.write_mock_config("on-request", "", "");
    home.write_requirements(
        "default_permissions = \":read-only\"\n[allowed_permission_profiles]\n\":read-only\" = true\n\":workspace\" = true",
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let started = server
        .call(&mut connection, "thread/start", json!({}))
        .await;
    let thread_id = started["thread"]["id"].as_str().unwrap().to_owned();
    assert_eq!(started["activePermissionProfile"]["id"], ":read-only");

    let error = server
        .call_error(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": thread_id, "permissions": ":danger-full-access"}),
        )
        .await;
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("is disallowed by requirements"),
        "unexpected error message: {message}"
    );

    server
        .call(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": thread_id, "permissions": ":workspace"}),
        )
        .await;
    let applied = connection.notification("thread/settings/updated").await;
    assert_eq!(
        applied["threadSettings"]["activePermissionProfile"]["id"],
        ":workspace"
    );
    assert_eq!(
        applied["threadSettings"]["sandboxPolicy"]["type"],
        "workspaceWrite"
    );
}

/// Codex's session projection preserves existing deny entries and glob scan limits when a
/// requested permission profile replaces its allow rules. Legacy updates preserve them too.
#[tokio::test]
async fn permission_updates_preserve_existing_deny_read_entries() {
    use ra_coding::config::{ConfigOverrides, PermissionProfileSnapshot};
    use ra_coding::thread_settings::{CodingThreadSettings, ProductPermissionSource};
    use ra_filesystem::models::PermissionProfile;
    use ra_filesystem::permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry,
    };
    use ra_protocol::server::{ThreadSettingsAdapter, protocol::ThreadPolicyOverrides};

    let home = ProductHome::new();
    home.write_mock_config("on-request", "default_permissions = \":workspace\"", "");
    let root = home.workspace.path().canonicalize().unwrap();
    let builder = home.config_builder().harness_overrides(ConfigOverrides {
        cwd: Some(root.clone()),
        ..ConfigOverrides::default()
    });
    let mut config = builder.clone().build().await.unwrap();
    let original = config.permissions.permission_profile();
    let (mut file_system, network) = original.to_runtime_permissions();
    let deny = FileSystemSandboxEntry::new(
        FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_owned(),
        },
        FileSystemAccessMode::Deny,
    );
    file_system.entries.push(deny.clone());
    file_system.glob_scan_max_depth = Some(2);
    let saved = PermissionProfile::from_runtime_permissions_with_enforcement(
        original.enforcement(),
        &file_system,
        network,
    );
    config
        .permissions
        .set_permission_profile_from_session_snapshot(PermissionProfileSnapshot::legacy(saved))
        .unwrap();
    let adapter = CodingThreadSettingsAdapter::new(
        &root,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap()
    .with_product_config(&config, ProductPermissionSource::new(builder));
    let initial = adapter.initialize(RunConfig::default());

    for update in [
        ThreadPolicyOverrides::new().with_permissions(":workspace"),
        ThreadPolicyOverrides::new().with_sandbox_policy(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        }),
    ] {
        let updated = adapter.prepare_async(&initial, &update).await.unwrap();
        let settings = updated
            .app_context()
            .unwrap()
            .downcast_ref::<CodingThreadSettings>()
            .unwrap();
        let effective = settings
            .permissions_state()
            .permission_profile()
            .file_system_sandbox_policy();
        assert!(effective.entries.contains(&deny));
        assert_eq!(effective.glob_scan_max_depth, Some(2));
    }
}

/// Request selections must be loaded before validating an incompatible configured default.
#[tokio::test]
async fn thread_start_permission_overrides_precede_default_validation() {
    for selection in [
        json!({"sandbox": "read-only"}),
        json!({"permissions": ":read-only"}),
        json!({"approvalPolicy": "on-request"}),
    ] {
        let home = ProductHome::new();
        home.write_mock_config("never", "default_permissions = \":danger-full-access\"", "");
        home.write_requirements("allowed_sandbox_modes = [\"read-only\"]");
        let server = home.server();
        let mut connection = server.connect(1).await;
        let started = server
            .call(&mut connection, "thread/start", selection.clone())
            .await;
        assert_eq!(started["sandbox"]["type"], "readOnly", "{selection}");
    }
}

/// Both saved profiles and explicit request overrides must be available to the first Config
/// load on resume and fork; otherwise unused full-access defaults fail validation too early.
#[tokio::test]
async fn resumed_and_forked_permissions_precede_default_validation() {
    for (saved, explicit) in [
        (json!({"sandbox": "read-only"}), json!({})),
        (json!({"permissions": "dev"}), json!({})),
        (
            json!({"sandbox": "danger-full-access"}),
            json!({"sandbox": "read-only"}),
        ),
        (
            json!({"sandbox": "danger-full-access"}),
            json!({"permissions": ":read-only"}),
        ),
    ] {
        let home = ProductHome::new();
        home.write_mock_config("never", "", "[permissions.dev]\nextends = \":read-only\"");
        let thread_id = materialize_thread(&home, saved.clone()).await;
        home.write_mock_config(
            "never",
            "default_permissions = \":danger-full-access\"",
            "[permissions.dev]\nextends = \":read-only\"",
        );
        home.write_requirements("allowed_sandbox_modes = [\"read-only\"]");
        for method in ["thread/resume", "thread/fork"] {
            let server = home.server();
            let mut connection = server.connect(1).await;
            let mut params = explicit.clone();
            params["threadId"] = json!(thread_id);
            let loaded = server.call(&mut connection, method, params).await;
            assert_eq!(
                loaded["sandbox"]["type"], "readOnly",
                "{method}: {saved} / {explicit}"
            );
            assert_eq!(loaded["approvalPolicy"], "never");
        }
    }
}

/// A thread-loading sandbox mode derives all workspace-write options from Config, whereas
/// a live sandboxPolicy update supplies a complete replacement policy.
/// Windows without its sandbox enabled intentionally downgrades this mode to read-only.
#[cfg(unix)]
#[tokio::test]
async fn thread_loading_sandbox_mode_preserves_workspace_write_config() {
    let home = ProductHome::new();
    let writable = tempfile::tempdir().unwrap();
    let writable = writable.path().canonicalize().unwrap();
    home.write_mock_config("on-request", "", &format!(
        "[sandbox_workspace_write]\nwritable_roots = [{}]\nnetwork_access = true\nexclude_tmpdir_env_var = true\nexclude_slash_tmp = true",
        serde_json::to_string(&writable).unwrap(),
    ));
    let source = materialize_thread(&home, json!({"sandbox": "read-only"})).await;
    let expected = json!({"type": "workspaceWrite", "writableRoots": [writable],
        "networkAccess": true, "excludeTmpdirEnvVar": true, "excludeSlashTmp": true});
    for method in ["thread/start", "thread/resume", "thread/fork"] {
        let server = home.server();
        let mut connection = server.connect(1).await;
        let mut params = json!({"sandbox": "workspace-write"});
        if method != "thread/start" {
            params["threadId"] = json!(source);
        }
        let loaded = server.call(&mut connection, method, params).await;
        assert_eq!(loaded["sandbox"], expected, "{method}");
        let replacement = json!({"type": "workspaceWrite", "writableRoots": [],
            "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false});
        server
            .call(
                &mut connection,
                "thread/settings/update",
                json!({
                    "threadId": loaded["thread"]["id"], "sandboxPolicy": replacement,
                }),
            )
            .await;
        let applied = connection.notification("thread/settings/updated").await;
        assert_eq!(applied["threadSettings"]["sandboxPolicy"], replacement);
    }
}

/// Permission selection reloads use configured approval defaults, not stale thread-start
/// overrides. The live thread's updated approval is checked separately by session constraints.
#[tokio::test]
async fn permission_reload_does_not_reuse_start_approval_override() {
    for (configured, initial, expected_error) in [
        (
            "on-request",
            "never",
            "Configured value for `permission_profile` is disallowed",
        ),
        (
            "never",
            "on-request",
            "`approval_policy = \"never\"` cannot be used",
        ),
    ] {
        let home = ProductHome::new();
        home.write_mock_config(configured, "", "");
        home.write_requirements("allowed_sandbox_modes = [\"read-only\"]");
        let server = home.server();
        let mut connection = server.connect(1).await;
        let started = server
            .call(
                &mut connection,
                "thread/start",
                json!({
                    "sandbox": "read-only", "approvalPolicy": initial,
                }),
            )
            .await;
        let thread_id = &started["thread"]["id"];
        server
            .call(
                &mut connection,
                "thread/settings/update",
                json!({
                    "threadId": thread_id, "approvalPolicy": configured,
                }),
            )
            .await;
        connection.notification("thread/settings/updated").await;
        let error = server
            .call_error(
                &mut connection,
                "thread/settings/update",
                json!({
                    "threadId": thread_id, "permissions": ":danger-full-access",
                }),
            )
            .await;
        assert!(
            error["message"].as_str().unwrap().contains(expected_error),
            "{configured} / {initial}: {error}"
        );
    }
}

/// Not a Codex case: as Codex's thread start resolves it, a named profile the requirements
/// disallow falls back to the required profile instead of failing the load. Only settings and turn
/// updates refuse such a selection (see `thread_settings_update_refuses_profile_requirements_disallow`).
#[tokio::test]
async fn thread_start_falls_back_from_disallowed_named_profile() {
    let home = ProductHome::new();
    home.write_mock_config("on-request", "", "");
    home.write_requirements(
        "default_permissions = \":read-only\"\n[allowed_permission_profiles]\n\":read-only\" = true",
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let started = server
        .call(
            &mut connection,
            "thread/start",
            json!({"permissions": ":danger-full-access"}),
        )
        .await;
    assert_eq!(
        started["activePermissionProfile"],
        json!({"id": ":read-only", "extends": null})
    );
    assert_eq!(started["sandbox"]["type"], "readOnly");
    let thread_id = started["thread"]["id"].as_str().unwrap();
    let loaded = server
        .call(&mut connection, "thread/loaded/list", json!({}))
        .await;
    assert_eq!(loaded["data"], json!([thread_id]));
}

/// Codex's `turn_start_enforces_managed_network_sandbox`, under Seatbelt: Codex runs the case on
/// Linux through its bubblewrap helper, which is not ported, so this exercises the same managed
/// profile through the macOS sandbox. Codex binds the origin to `127.0.0.2` to keep it apart
/// from a sandbox network namespace; macOS has no such namespace and does not configure that
/// loopback alias, so the origin is on `127.0.0.1` and the allowlist names it.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn turn_start_enforces_managed_network_sandbox() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let home = ProductHome::new();
    home.write_mock_config("never", "", "[features]\nnetwork_proxy = true\n");
    home.write_requirements(
        r#"
default_permissions = "managed-network"

[allowed_permission_profiles]
managed-network = true
":read-only" = true

[permissions.managed-network]
extends = ":read-only"

[permissions.managed-network.network]
enabled = true
allow_local_binding = false
allow_upstream_proxy = false

[permissions.managed-network.network.domains]
"127.0.0.1" = "allow"
"#,
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let started = server
        .call(&mut connection, "thread/start", json!({}))
        .await;
    let thread_id = started["thread"]["id"].as_str().unwrap().to_owned();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    // Keep the real origin listening throughout both the proxied and direct requests.
    let _origin = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let body = "sandbox proxy response";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }));
    let test_executable = std::env::current_exe().unwrap();
    let test_executable = test_executable.to_str().unwrap();
    // Libtest selectors omit the integration crate name.
    let (_, test_module) = module_path!().split_once("::").unwrap();
    let command = format!(
        "env 'CODEX_TEST_MANAGED_NETWORK_TARGET={target}' '{test_executable}' --exact \
         {test_module}::managed_network_sandbox_probe --ignored --nocapture --test-threads=1"
    );
    let call_id = "managed-network-probe";
    home.script
        .push(Step::Respond(ModelResponse::new(vec![RunItem::new(
            ItemId::new(call_id),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(call_id),
                "exec_command",
                json!({"cmd": command, "justification": null, "prefix_rule": null,
                   "sandbox_permissions": null, "workdir": null, "shell": "/bin/sh",
                   "tty": false, "login": false, "yield_time_ms": 30000, "timeout_ms": null}),
            )),
        )])));
    home.script.answer("Network probe complete");

    server
        .call(
            &mut connection,
            "turn/start",
            json!({"threadId": thread_id,
                   "input": [{"type": "text", "text": "Verify managed sandbox networking"}],
                   "permissions": "managed-network"}),
        )
        .await;
    connection.notification("turn/completed").await;

    // The marker also proves the child ran: libtest succeeds if no selector matches.
    let output = serde_json::to_string(home.script.last_request().input()).unwrap();
    assert!(
        output.contains("proxied request succeeded; direct access blocked"),
        "unexpected tool output: {output}"
    );
}

/// Runs as the model-controlled command inside the managed network sandbox.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "invoked through the managed network sandbox"]
fn managed_network_sandbox_probe() {
    use std::io::{ErrorKind, Read, Write};
    use std::net::{SocketAddr, TcpStream};

    let target: SocketAddr = std::env::var("CODEX_TEST_MANAGED_NETWORK_TARGET")
        .unwrap()
        .parse()
        .unwrap();
    let proxy = std::env::var("HTTP_PROXY").expect("the managed proxy is advertised");
    let proxy = proxy
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .parse::<SocketAddr>()
        .expect("the managed proxy listens on a loopback socket address");
    let proxy_timeout = Duration::from_secs(5);
    let mut connection = TcpStream::connect_timeout(&proxy, proxy_timeout).unwrap();
    connection.set_read_timeout(Some(proxy_timeout)).unwrap();
    connection.set_write_timeout(Some(proxy_timeout)).unwrap();
    // HTTP/1.0 keeps this small fixture response close-delimited, without chunked encoding.
    write!(
        connection,
        "GET http://{target}/sandbox HTTP/1.0\r\nHost: {target}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    connection.take(4096).read_to_string(&mut response).unwrap();
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .expect("complete proxy HTTP response");
    let status = headers.split_whitespace().nth(1).expect("HTTP status");
    assert_eq!((status, body), ("200", "sandbox proxy response"));

    let direct_error = TcpStream::connect_timeout(&target, Duration::from_secs(2))
        .expect_err("sandbox allowed direct host access");
    assert!(
        matches!(
            direct_error.kind(),
            ErrorKind::ConnectionRefused
                | ErrorKind::NetworkUnreachable
                | ErrorKind::PermissionDenied
        ),
        "unexpected direct access error: {direct_error:?}"
    );
    println!("proxied request succeeded; direct access blocked");
}

/// A loopback origin answering every request with `sandbox proxy response`, kept alive for as long
/// as the returned handle lives.
#[cfg(target_os = "macos")]
async fn loopback_origin() -> (
    std::net::SocketAddr,
    tokio_util::task::AbortOnDropHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let origin = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let body = "sandbox proxy response";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }));
    (target, origin)
}

/// Codex's network approval in flight, under Seatbelt: a command reaches a host the managed
/// allowlist does not name, the proxy asks the client through `item/commandExecution/requestApproval`
/// with the network context, and the client's answer decides the request. Not a Codex test file
/// case: Codex covers the same flow in `core/tests/suite/network_approval.rs` through its own
/// harness. Local binding is allowed so the loopback origin is an allowlist miss rather than a
/// private-address denial.
#[cfg(target_os = "macos")]
async fn network_access_is_reviewed_in_flight(decision: &str) -> (Value, String) {
    let (params, output, _) = network_access_review(json!(decision), false).await;
    (params, output)
}

/// [`network_access_is_reviewed_in_flight`] with the client's whole answer, and the `warning`
/// notification that follows it when `expect_warning` says one does.
#[cfg(target_os = "macos")]
async fn network_access_review(
    decision: Value,
    expect_warning: bool,
) -> (Value, String, Option<Value>) {
    let (params, output, warning) = network_review(NetworkReview {
        decision: Some(decision),
        expect_warning,
        ..NetworkReview::default()
    })
    .await;
    (params.expect("the client is asked"), output, warning)
}

/// How [`network_review`] runs the probe.
#[cfg(target_os = "macos")]
#[derive(Default)]
struct NetworkReview {
    /// Top-level configuration added to the thread's.
    root_config: &'static str,
    /// Requirements added to the managed network's.
    requirements: &'static str,
    /// The command asks to run escalated, and its approval is accepted.
    escalated: bool,
    /// The turn is moved to the user reviewer after it starts and before its command runs.
    switch_reviewer_to_user: bool,
    /// The answer to the network request, which must come when given and must not otherwise.
    decision: Option<Value>,
    /// A `warning` notification follows the answer.
    expect_warning: bool,
}

/// The network review of [`network_access_review`], run as `review` says.
#[cfg(target_os = "macos")]
async fn network_review(review: NetworkReview) -> (Option<Value>, String, Option<Value>) {
    let NetworkReview {
        root_config,
        requirements,
        escalated,
        switch_reviewer_to_user,
        decision,
        expect_warning,
    } = review;
    let home = ProductHome::new();
    home.write_mock_config(
        "on-request",
        root_config,
        "[features]\nnetwork_proxy = true\n",
    );
    home.write_requirements(&format!(
        r#"
[experimental_network]
enabled = true
allow_local_binding = true

[experimental_network.domains]
"example.org" = "allow"

{requirements}
"#
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let started = server
        .call(&mut connection, "thread/start", json!({}))
        .await;
    let thread_id = started["thread"]["id"].as_str().unwrap().to_owned();
    let (target, _origin) = loopback_origin().await;
    let test_executable = std::env::current_exe().unwrap();
    let test_executable = test_executable.to_str().unwrap();
    let (_, test_module) = module_path!().split_once("::").unwrap();
    let command = format!(
        "env 'CODEX_TEST_MANAGED_NETWORK_TARGET={target}' '{test_executable}' --exact \
         {test_module}::managed_network_approval_probe --ignored --nocapture --test-threads=1"
    );
    let call_id = "managed-network-approval-probe";
    let probe = ModelResponse::new(vec![RunItem::new(
        ItemId::new(call_id),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            "exec_command",
            json!({"cmd": command, "justification": escalated.then_some("reach the host"),
                   "prefix_rule": null,
                   "sandbox_permissions": escalated.then_some("require_escalated"),
                   "workdir": null, "shell": "/bin/sh",
                   "tty": false, "login": false, "yield_time_ms": 30000, "timeout_ms": null}),
        )),
    )]);
    let gate = Arc::new(Notify::new());
    home.script.push(if switch_reviewer_to_user {
        Step::After(Arc::clone(&gate), probe)
    } else {
        Step::Respond(probe)
    });
    home.script.answer("Network probe complete");

    let started = server
        .call(
            &mut connection,
            "turn/start",
            json!({"threadId": thread_id,
                   "input": [{"type": "text", "text": "Reach a host outside the allowlist"}]}),
        )
        .await;
    if switch_reviewer_to_user {
        server
            .call(
                &mut connection,
                "turn/settings/update",
                json!({"threadId": thread_id, "turnId": started["turn"]["id"],
                       "approvalsReviewer": "user"}),
            )
            .await;
        gate.notify_one();
    }
    if escalated {
        let request = connection
            .until(|message| {
                message["method"] == "item/commandExecution/requestApproval"
                    && message["params"].get("networkApprovalContext").is_none()
            })
            .await;
        server
            .send(
                &connection,
                ra_protocol::frame::JsonRpcResponse::new(
                    serde_json::from_value::<ra_protocol::frame::RequestId>(request["id"].clone())
                        .unwrap(),
                    json!({"decision": "accept"}),
                ),
            )
            .await;
    }
    let request = if let Some(decision) = decision {
        let request = connection
            .until(|message| {
                message["method"] == "item/commandExecution/requestApproval"
                    && message["params"].get("networkApprovalContext").is_some()
            })
            .await;
        server
            .send(
                &connection,
                ra_protocol::frame::JsonRpcResponse::new(
                    serde_json::from_value::<ra_protocol::frame::RequestId>(request["id"].clone())
                        .unwrap(),
                    json!({"decision": decision}),
                ),
            )
            .await;
        Some(request["params"].clone())
    } else {
        None
    };
    let warning = if expect_warning {
        Some(connection.notification("warning").await)
    } else {
        None
    };
    connection.notification("turn/completed").await;
    assert!(
        request.is_some()
            || !connection.passed.iter().any(|message| {
                message["method"] == "item/commandExecution/requestApproval"
                    && message["params"].get("networkApprovalContext").is_some()
            }),
        "the client is not asked"
    );
    let output = serde_json::to_string(home.script.last_request().input()).unwrap();
    (request, output, warning)
}

/// Codex routes a network review under an automatic reviewer to Guardian, which is not ported:
/// the access is refused without asking the client.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_automatic_reviewer_refuses_network_access() {
    let (_, output, _) = network_review(NetworkReview {
        root_config: "approvals_reviewer = \"auto_review\"",
        ..NetworkReview::default()
    })
    .await;

    assert!(
        output.contains("automatic approval review is not available"),
        "unexpected tool output: {output}"
    );
    assert!(!output.contains("proxied status 200"), "{output}");
}

/// Codex reviews a network access under the settings the running turn's next step has, so a
/// mid-turn switch to the user reviewer asks the client.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_mid_turn_reviewer_switch_applies_to_network_review() {
    let (params, output, _) = network_review(NetworkReview {
        root_config: "approvals_reviewer = \"auto_review\"",
        switch_reviewer_to_user: true,
        decision: Some(json!("accept")),
        ..NetworkReview::default()
    })
    .await;

    assert_eq!(
        params.unwrap()["networkApprovalContext"]["host"],
        "127.0.0.1"
    );
    assert!(
        output.contains("proxied status 200"),
        "unexpected tool output: {output}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn approved_network_access_reaches_the_host() {
    let (params, output) = network_access_is_reviewed_in_flight("accept").await;

    assert_eq!(params["networkApprovalContext"]["host"], "127.0.0.1");
    assert_eq!(params["networkApprovalContext"]["protocol"], "http");
    assert_eq!(params["reason"], "127.0.0.1 is not in the allowed_domains");
    assert!(params.get("command").is_none(), "{params}");
    assert_eq!(
        params["proposedNetworkPolicyAmendments"],
        json!([{"host": "127.0.0.1", "action": "allow"},
               {"host": "127.0.0.1", "action": "deny"}])
    );
    assert!(
        output.contains("proxied status 200"),
        "unexpected tool output: {output}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn declined_network_access_rejects_the_command() {
    let (_, output) = network_access_is_reviewed_in_flight("decline").await;

    assert!(
        output.contains("rejected by user"),
        "unexpected tool output: {output}"
    );
    assert!(
        !output.contains("proxied status 200"),
        "unexpected tool output: {output}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_network_rule_that_cannot_be_applied_warns_the_client() {
    // The amendment names a host other than the one approved, so it is refused when applied.
    let (_, output, warning) = network_access_review(
        json!({"applyNetworkPolicyAmendment": {"network_policy_amendment": {
            "host": "other.test", "action": "allow"}}}),
        true,
    )
    .await;

    let warning = warning.expect("the client is warned");
    let message = warning["message"].as_str().unwrap();
    assert!(
        message.starts_with("Failed to apply network policy amendment: ")
            && message.contains("does not match approved host"),
        "{warning}"
    );
    assert!(
        !output.contains("proxied status 200"),
        "the access stays blocked: {output}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_saved_network_rule_is_told_to_the_model_without_hooks() {
    let (_, output, _) = network_access_review(
        json!({"applyNetworkPolicyAmendment": {"network_policy_amendment": {
            "host": "127.0.0.1", "action": "allow"}}}),
        false,
    )
    .await;

    assert!(
        output.contains("Allowed network rule saved in execpolicy (allowlist): 127.0.0.1"),
        "unexpected model input: {output}"
    );
    assert!(
        output.contains("proxied status 200"),
        "the saved rule lets the access through: {output}"
    );
}

/// Runs as the model-controlled command; prints the status the proxy answered with.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "invoked through the managed network sandbox"]
fn managed_network_approval_probe() {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};

    let target: SocketAddr = std::env::var("CODEX_TEST_MANAGED_NETWORK_TARGET")
        .unwrap()
        .parse()
        .unwrap();
    let proxy = std::env::var("HTTP_PROXY").expect("the managed proxy is advertised");
    let proxy = proxy
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .parse::<SocketAddr>()
        .expect("the managed proxy listens on a loopback socket address");
    let timeout = Duration::from_secs(20);
    let mut connection = TcpStream::connect_timeout(&proxy, timeout).unwrap();
    connection.set_read_timeout(Some(timeout)).unwrap();
    write!(
        connection,
        "GET http://{target}/sandbox HTTP/1.0\r\nHost: {target}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    let _ = connection.take(4096).read_to_string(&mut response);
    let status = response.split_whitespace().nth(1).unwrap_or("none");
    println!("proxied status {status}");
}

/// Codex's `sandbox_permissions_preserving_denied_reads`: an escalated command cannot drop the
/// sandbox that enforces denied reads, so it still runs under the managed proxy and its review.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_escalation_kept_sandboxed_by_denied_reads_stays_under_network_review() {
    let (params, output, _) = network_review(NetworkReview {
        requirements: "[permissions.filesystem]\ndeny_read = ['/rusty-agent-denied-read-probe']",
        escalated: true,
        decision: Some(json!("accept")),
        ..NetworkReview::default()
    })
    .await;

    assert_eq!(
        params.unwrap()["networkApprovalContext"]["host"],
        "127.0.0.1"
    );
    assert!(
        output.contains("proxied status 200"),
        "unexpected tool output: {output}"
    );
}

/// A required managed permission hook answers the real proxy request before its client is asked.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_default_permission_hook_denies_network_access_without_asking_the_client() {
    let (request, output, _) = network_review(NetworkReview {
        requirements: r#"
[hooks]
managed_dir = "/tmp"
[[hooks.PermissionRequest]]
matcher = "Bash"
[[hooks.PermissionRequest.hooks]]
type = "command"
command = '''printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"network denied by default hook"}}}' '''
"#,
        ..NetworkReview::default()
    }).await;
    assert!(request.is_none());
    assert!(
        output.contains("network denied by default hook"),
        "{output}"
    );
    assert!(!output.contains("proxied status 200"), "{output}");
}
