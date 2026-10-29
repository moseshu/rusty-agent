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
        .unwrap();
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
