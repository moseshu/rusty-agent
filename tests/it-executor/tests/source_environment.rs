// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_executor::test_api::environment::Environment;
use ra_executor::test_api::environment::EnvironmentManager;
use ra_executor::test_api::environment::EnvironmentObservedStatus;
use ra_executor::test_api::environment::LOCAL_ENVIRONMENT_ID;
use ra_executor::test_api::environment::REMOTE_ENVIRONMENT_ID;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ra_executor::ExecServerRuntimeOptions;

use ra_config::ScopedSkillsConfig;
use ra_executor::test_api::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;
use ra_executor::test_api::client_api::ExecServerTransportParams;
use ra_executor::test_api::client_api::StdioExecServerCommand;
use ra_executor::test_api::environment_provider::EnvironmentDefault;
use ra_executor::test_api::environment_provider::EnvironmentProviderEntry;
use ra_executor::test_api::environment_provider::EnvironmentProviderSnapshot;
use ra_http::HttpClientFactory;
use ra_http::OutboundProxyPolicy;
use ra_paths::PathUri;

use tokio::net::TcpListener;
use tokio::time::timeout;
fn legacy_http_client_factory() -> HttpClientFactory {
    HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
}
fn prepared_websocket_environment() -> ExecServerTransportParams {
    ExecServerTransportParams::websocket_url(
        "ws://127.0.0.1:8765".to_string(),
        DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
    )
}
fn test_runtime_paths() -> ExecServerRuntimeOptions {
    ExecServerRuntimeOptions::new(std::env::current_exe().expect("current exe"), None)
        .expect("runtime paths")
}
fn assert_local_environment_unavailable(manager: &EnvironmentManager) {
    assert!(manager.try_local_environment().is_none());
}
#[test]
fn local_environment_info_includes_current_directory() {
    let info = ra_executor::test_api::environment::local_environment_info();
    assert_eq!(
        info.cwd,
        Some(
            PathUri::from_host_native_path(std::env::current_dir().expect("current directory"))
                .expect("cwd URI")
        )
    );
    assert_eq!(
        info.temp_dir,
        PathUri::from_host_native_path(std::env::temp_dir()).ok()
    );
}
#[tokio::test]
async fn create_local_environment_does_not_connect() {
    let environment = Environment::create(None, test_runtime_paths(), legacy_http_client_factory())
        .expect("create environment");
    assert!(!environment.is_remote());
    assert!(environment.info().await.is_ok());
}
#[tokio::test]
async fn environment_manager_normalizes_empty_url() {
    let manager =
        EnvironmentManager::create_for_tests(Some(String::new()), Some(test_runtime_paths())).await;
    let environment = manager.default_environment().expect("default environment");
    assert_eq!(manager.default_environment_id(), Some(LOCAL_ENVIRONMENT_ID));
    assert!(Arc::ptr_eq(
        &environment,
        &manager
            .get_environment(LOCAL_ENVIRONMENT_ID)
            .expect("local environment")
    ));
    assert!(Arc::ptr_eq(
        &environment,
        &manager.try_local_environment().expect("local environment")
    ));
    assert!(manager.try_local_environment().is_some());
    assert!(manager.get_environment(REMOTE_ENVIRONMENT_ID).is_none());
    assert!(!environment.is_remote());
}
#[tokio::test]
async fn disabled_environment_manager_has_no_default_or_local_environment() {
    let manager = EnvironmentManager::without_environments(HttpClientFactory::new(
        OutboundProxyPolicy::RespectSystemProxy,
    ));
    assert!(manager.default_environment().is_none());
    assert_eq!(manager.default_environment_id(), None);
    assert_local_environment_unavailable(&manager);
    assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
    assert!(manager.get_environment(REMOTE_ENVIRONMENT_ID).is_none());
    assert_eq!(
        manager.http_client_factory().outbound_proxy_policy(),
        OutboundProxyPolicy::RespectSystemProxy
    );
}
#[tokio::test]
async fn environment_manager_creates_remote_environment_for_url() {
    let manager = EnvironmentManager::create_for_tests(
        Some("ws://127.0.0.1:8765".to_string()),
        Some(test_runtime_paths()),
    )
    .await;
    let environment = manager.default_environment().expect("default environment");
    assert_eq!(
        manager.default_environment_id(),
        Some(REMOTE_ENVIRONMENT_ID)
    );
    assert!(environment.is_remote());
    assert!(Arc::ptr_eq(
        &environment,
        &manager
            .get_environment(REMOTE_ENVIRONMENT_ID)
            .expect("remote environment")
    ));
    assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
    assert_local_environment_unavailable(&manager);
}
#[tokio::test]
async fn environment_manager_default_environment_caches_environment() {
    let manager = EnvironmentManager::default_for_tests();
    let first = manager.default_environment().expect("default environment");
    let second = manager.default_environment().expect("default environment");
    assert!(Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(
        &first.get_filesystem(),
        &second.get_filesystem()
    ));
}
#[tokio::test]
async fn environment_manager_builds_from_snapshot() {
    let snapshot = EnvironmentProviderSnapshot::for_test(
        vec![EnvironmentProviderEntry {
            id: REMOTE_ENVIRONMENT_ID.to_string(),
            transport: prepared_websocket_environment(),
            skills: Default::default(),
        }],
        EnvironmentDefault::EnvironmentId(REMOTE_ENVIRONMENT_ID.to_string()),
        false,
    );
    let manager = ra_executor::test_api::environment::from_snapshot(
        snapshot,
        Some(test_runtime_paths()),
        legacy_http_client_factory(),
    )
    .expect("environment manager");
    assert_eq!(
        manager.default_environment_id(),
        Some(REMOTE_ENVIRONMENT_ID)
    );
    assert!(
        manager
            .get_environment(REMOTE_ENVIRONMENT_ID)
            .expect("remote environment")
            .is_remote()
    );
    assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
    assert_local_environment_unavailable(&manager);
}
#[tokio::test]
async fn environment_manager_rejects_empty_environment_id() {
    let snapshot = EnvironmentProviderSnapshot::for_test(
        vec![EnvironmentProviderEntry {
            id: "".to_string(),
            transport: prepared_websocket_environment(),
            skills: Default::default(),
        }],
        EnvironmentDefault::Disabled,
        false,
    );
    let err = ra_executor::test_api::environment::from_snapshot(
        snapshot,
        Some(test_runtime_paths()),
        legacy_http_client_factory(),
    )
    .expect_err("empty id should fail");
    assert_eq!(
        err.to_string(),
        "exec-server protocol error: environment id cannot be empty"
    );
}
#[tokio::test]
async fn environment_manager_rejects_provider_supplied_local_environment() {
    let snapshot = EnvironmentProviderSnapshot::for_test(
        vec![EnvironmentProviderEntry {
            id: LOCAL_ENVIRONMENT_ID.to_string(),
            transport: prepared_websocket_environment(),
            skills: Default::default(),
        }],
        EnvironmentDefault::Disabled,
        false,
    );
    let err = ra_executor::test_api::environment::from_snapshot(
        snapshot,
        Some(test_runtime_paths()),
        legacy_http_client_factory(),
    )
    .expect_err("local id should fail");
    assert_eq!(
        err.to_string(),
        "exec-server protocol error: environment id `local` is reserved for EnvironmentManager"
    );
}
#[tokio::test]
async fn environment_manager_uses_explicit_provider_default() {
    let snapshot = EnvironmentProviderSnapshot::for_test(
        vec![EnvironmentProviderEntry {
            id: "devbox".to_string(),
            transport: prepared_websocket_environment(),
            skills: Default::default(),
        }],
        EnvironmentDefault::EnvironmentId("devbox".to_string()),
        true,
    );
    let manager = ra_executor::test_api::environment::from_snapshot(
        snapshot,
        Some(test_runtime_paths()),
        legacy_http_client_factory(),
    )
    .expect("manager");
    assert_eq!(manager.default_environment_id(), Some("devbox"));
    assert_eq!(
        manager.default_environment_ids(),
        vec!["devbox".to_string(), LOCAL_ENVIRONMENT_ID.to_string()]
    );
    assert!(manager.default_environment().expect("default").is_remote());
}
#[tokio::test]
async fn environment_manager_disables_provider_default() {
    let snapshot = EnvironmentProviderSnapshot::for_test(
        vec![EnvironmentProviderEntry {
            id: "devbox".to_string(),
            transport: prepared_websocket_environment(),
            skills: Default::default(),
        }],
        EnvironmentDefault::Disabled,
        true,
    );
    let manager = ra_executor::test_api::environment::from_snapshot(
        snapshot,
        Some(test_runtime_paths()),
        legacy_http_client_factory(),
    )
    .expect("manager");
    assert_eq!(manager.default_environment_id(), None);
    assert!(manager.default_environment().is_none());
    assert!(Arc::ptr_eq(
        &manager
            .get_environment(LOCAL_ENVIRONMENT_ID)
            .expect("local environment"),
        &manager.try_local_environment().expect("local environment")
    ));
}
#[tokio::test]
async fn environment_manager_rejects_unknown_provider_default() {
    let snapshot = EnvironmentProviderSnapshot::for_test(
        vec![EnvironmentProviderEntry {
            id: "devbox".to_string(),
            transport: prepared_websocket_environment(),
            skills: Default::default(),
        }],
        EnvironmentDefault::EnvironmentId("missing".to_string()),
        true,
    );
    let err = ra_executor::test_api::environment::from_snapshot(
        snapshot,
        Some(test_runtime_paths()),
        legacy_http_client_factory(),
    )
    .expect_err("unknown default should fail");
    assert_eq!(
        err.to_string(),
        "exec-server protocol error: default environment `missing` is not configured"
    );
}
#[tokio::test]
async fn environment_manager_includes_local_for_default_provider_without_url() {
    let manager = EnvironmentManager::create_for_tests(None, Some(test_runtime_paths())).await;
    let environment = manager.default_environment().expect("default environment");
    assert_eq!(manager.default_environment_id(), Some(LOCAL_ENVIRONMENT_ID));
    assert!(Arc::ptr_eq(
        &environment,
        &manager
            .get_environment(LOCAL_ENVIRONMENT_ID)
            .expect("local environment")
    ));
    assert!(Arc::ptr_eq(
        &environment,
        &manager.try_local_environment().expect("local environment")
    ));
    assert!(!environment.is_remote());
}
#[tokio::test]
async fn environment_manager_carries_local_runtime_paths() {
    let runtime_paths = test_runtime_paths();
    let manager = EnvironmentManager::create_for_tests(None, Some(runtime_paths.clone())).await;
    let environment = manager.try_local_environment().expect("local environment");
    assert_eq!(environment.local_runtime_paths(), Some(&runtime_paths));
    let manager = EnvironmentManager::create_for_tests(
        None,
        Some(
            environment
                .local_runtime_paths()
                .expect("local runtime paths")
                .clone(),
        ),
    )
    .await;
    let environment = manager.try_local_environment().expect("local environment");
    assert_eq!(environment.local_runtime_paths(), Some(&runtime_paths));
}
#[tokio::test]
async fn environment_manager_omits_default_provider_local_lookup_when_default_disabled() {
    let manager =
        EnvironmentManager::create_for_tests(Some("none".to_string()), Some(test_runtime_paths()))
            .await;
    assert!(manager.default_environment().is_none());
    assert_eq!(manager.default_environment_id(), None);
    assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
    assert!(manager.get_environment(REMOTE_ENVIRONMENT_ID).is_none());
    assert_local_environment_unavailable(&manager);
}
#[tokio::test]
async fn environment_manager_snapshot_without_local_environment_disables_local_default() {
    let mut snapshot = EnvironmentProviderSnapshot::for_test(
        Vec::new(),
        EnvironmentDefault::EnvironmentId(LOCAL_ENVIRONMENT_ID.to_string()),
        true,
    );
    snapshot.include_local = false;
    snapshot.default = EnvironmentDefault::Disabled;
    let manager = ra_executor::test_api::environment::from_snapshot(
        snapshot,
        None,
        legacy_http_client_factory(),
    )
    .expect("environment manager");
    assert!(manager.default_environment().is_none());
    assert_eq!(manager.default_environment_id(), None);
    assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
    assert_local_environment_unavailable(&manager);
}
#[tokio::test]
async fn get_environment_returns_none_for_unknown_id() {
    let manager = EnvironmentManager::default_for_tests();
    assert!(manager.get_environment("does-not-exist").is_none());
}
#[tokio::test]
async fn environment_manager_upserts_named_remote_environment() {
    let manager = EnvironmentManager::without_environments(legacy_http_client_factory());
    manager
        .upsert_environment(
            "executor-a".to_string(),
            "ws://127.0.0.1:8765".to_string(),
            None,
        )
        .expect("remote environment");
    let first = manager
        .get_environment("executor-a")
        .expect("first remote environment");
    assert!(first.is_remote());
    assert_eq!(manager.default_environment_id(), None);
    manager
        .upsert_environment(
            "executor-a".to_string(),
            "ws://127.0.0.1:9876".to_string(),
            None,
        )
        .expect("updated remote environment");
    let second = manager
        .get_environment("executor-a")
        .expect("second remote environment");
    assert!(second.is_remote());
    assert!(!Arc::ptr_eq(&first, &second));
}
#[tokio::test]
async fn environment_manager_starts_remote_environment_when_upserted() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind websocket listener");
    let manager = EnvironmentManager::without_environments(legacy_http_client_factory());
    manager
        .upsert_environment(
            "executor-a".to_string(),
            format!("ws://{}", listener.local_addr().expect("listener address")),
            None,
        )
        .expect("remote environment");
    timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("environment should start connecting when registered")
        .expect("accept connection");
}
#[tokio::test]
async fn environment_status_keeps_stdio_environment_pending() {
    let environment = Environment::remote_with_transport(
        ExecServerTransportParams::StdioCommand {
            command: StdioExecServerCommand {
                program: "codex-missing-exec-server-for-test".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
            },
            initialize_timeout: Duration::from_secs(1),
        },
        None,
        legacy_http_client_factory(),
        ScopedSkillsConfig::default(),
    );
    assert_eq!(
        environment.status().await,
        EnvironmentObservedStatus::Pending
    );
    assert!(!environment.startup_finished());
}
#[tokio::test]
async fn environment_manager_leaves_stdio_environment_lazy() {
    let transport = ExecServerTransportParams::StdioCommand {
        command: StdioExecServerCommand {
            program: "codex-missing-exec-server-for-test".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
        },
        initialize_timeout: Duration::from_secs(1),
    };
    let manager = ra_executor::test_api::environment::from_snapshot(
        EnvironmentProviderSnapshot::for_test(
            vec![EnvironmentProviderEntry {
                id: "stdio".to_string(),
                transport,
                skills: Default::default(),
            }],
            EnvironmentDefault::Disabled,
            false,
        ),
        None,
        legacy_http_client_factory(),
    )
    .expect("environment manager");
    let environment = manager.get_environment("stdio").expect("stdio environment");
    assert!(!environment.startup_finished());
    assert!(environment.wait_until_ready().await.is_err());
    assert!(environment.startup_finished());
}
#[tokio::test]
async fn selected_capability_inspection_keeps_stdio_environment_lazy() {
    use ra_executor::capabilities::CapabilityRootLocation;
    use ra_executor::capabilities::SelectedCapabilityRoot;
    let transport = ExecServerTransportParams::StdioCommand {
        command: StdioExecServerCommand {
            program: "codex-missing-exec-server-for-test".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
        },
        initialize_timeout: Duration::from_secs(1),
    };
    let manager = ra_executor::test_api::environment::from_snapshot(
        EnvironmentProviderSnapshot::for_test(
            vec![EnvironmentProviderEntry {
                id: "stdio".to_string(),
                transport,
                skills: Default::default(),
            }],
            EnvironmentDefault::Disabled,
            false,
        ),
        None,
        legacy_http_client_factory(),
    )
    .expect("environment manager");
    let environment = manager.get_environment("stdio").expect("stdio environment");
    let selected_root = SelectedCapabilityRoot {
        id: "demo@1".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: "stdio".to_string(),
            path: PathUri::parse("file:///plugins/demo").expect("plugin path URI"),
        },
    };
    let status = manager.inspect_selected_capability_roots(std::slice::from_ref(&selected_root));
    assert!(status.ready_roots.is_empty());
    assert_eq!(status.warnings, Vec::<String>::new());
    assert!(!environment.startup_finished());
    let missing_root = SelectedCapabilityRoot {
        id: "missing@1".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: "missing".to_string(),
            path: PathUri::parse("file:///plugins/missing").expect("missing plugin path URI"),
        },
    };
    let status = manager.inspect_selected_capability_roots(&[missing_root]);
    assert!(status.ready_roots.is_empty());
    assert_eq!(
        status.warnings,
        vec![
            "selected capability root `missing@1` references unavailable environment `missing`"
                .to_string()
        ]
    );
    assert!(environment.wait_until_ready().await.is_err());
    let status = manager.inspect_selected_capability_roots(&[selected_root]);
    assert!(status.ready_roots.is_empty());
    assert_eq!(status.warnings.len(), 1);
    assert!(status.warnings[0].contains("environment `stdio` is unavailable"));
}
#[tokio::test]
async fn replacing_environment_stops_its_startup_task() {
    let first_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind first websocket listener");
    let second_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind second websocket listener");
    let manager = EnvironmentManager::without_environments(legacy_http_client_factory());
    manager
        .upsert_environment(
            "executor-a".to_string(),
            format!(
                "ws://{}",
                first_listener.local_addr().expect("first listener address")
            ),
            None,
        )
        .expect("first remote environment");
    let environment = manager
        .get_environment("executor-a")
        .expect("first remote environment");
    let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
    environment.observe_connection_attempts(move |_, outcome| {
        observed_tx
            .send(outcome)
            .expect("record cancelled connection");
    });
    let _connection = timeout(Duration::from_secs(1), first_listener.accept())
        .await
        .expect("connection should start")
        .expect("accept initial connection");
    let startup_abort = environment
        .startup_abort_handle_for_test()
        .expect("startup task");
    assert!(!startup_abort.is_finished());
    drop(environment);
    manager
        .upsert_environment(
            "executor-a".to_string(),
            format!(
                "ws://{}",
                second_listener
                    .local_addr()
                    .expect("second listener address")
            ),
            None,
        )
        .expect("replacement remote environment");
    timeout(Duration::from_secs(1), async {
        while !startup_abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replacing the environment should cancel its startup task");
    assert_eq!(
        observed_rx.try_recv(),
        Ok(ra_executor::test_api::environment::ConnectionAttemptOutcome::Cancelled)
    );
    assert!(observed_rx.try_recv().is_err());
}
#[tokio::test]
async fn environment_manager_rejects_empty_remote_environment_url() {
    let manager = EnvironmentManager::without_environments(legacy_http_client_factory());
    let err = manager
        .upsert_environment("executor-a".to_string(), String::new(), None)
        .expect_err("empty URL should fail");
    assert_eq!(
        err.to_string(),
        "exec-server protocol error: remote environment requires an exec-server url"
    );
}
#[tokio::test]
async fn test_environment_rejects_sandboxed_filesystem_without_runtime_paths() {
    let environment = Environment::default_for_tests();
    let path = ra_paths::AbsolutePathBuf::from_absolute_path(
        std::env::current_exe().expect("current exe").as_path(),
    )
    .expect("absolute current exe");
    let path = ra_paths::PathUri::from_abs_path(&path);
    let sandbox = ra_executor::FileSystemSandboxContext::from_permission_profile(
        ra_filesystem::models::PermissionProfile::from_runtime_permissions(
            &ra_filesystem::permissions::FileSystemSandboxPolicy::restricted(Vec::new()),
            ra_filesystem::permissions::NetworkSandboxPolicy::Restricted,
        ),
        PathUri::from_host_native_path(std::env::current_dir().expect("read current dir"))
            .expect("cwd URI"),
    );
    let err = environment
        .get_filesystem()
        .read_file(&path, Default::default(), Some(&sandbox))
        .await
        .expect_err("sandboxed read should require runtime paths");
    assert_eq!(
        err.to_string(),
        "sandboxed filesystem operations require configured runtime paths"
    );
}
