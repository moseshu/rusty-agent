//! Refresh through the default factory and real turn/config requests.
use super::*;
use ra_coding::plugins::{PluginConfigManager, PluginServices};
use ra_coding::skills::HostSkillsService;
use ra_paths::AbsolutePathBuf;

fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).unwrap()
}

async fn server_with_plugins(home: &HooksHome) -> (Server, Arc<PluginServices>) {
    let root = home.workspace.path().canonicalize().unwrap();
    let config = home.config_builder().build().await.unwrap();
    let services = PluginServices::new(
        PluginConfigManager::new(
            ra_executor::LOCAL_FS.clone(),
            absolute(home.home.path()),
            absolute(&root),
            ra_config::ConfigLoadOptions::from(LoaderOverrides::without_managed_config_for_tests()),
            Arc::new(ra_config::NoopThreadConfigLoader),
            config.http_client_factory().clone(),
        ),
        Arc::new(HostSkillsService::new(absolute(home.home.path()), false)),
    );
    let host = CodingHost::open(&root)
        .unwrap()
        .with_plugin_skills(services.clone());
    let settings = CodingThreadSettingsAdapter::new(
        &root,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap()
    .with_rules_home(home.home.path());
    let factory = CodingThreadFactory::new(host, Arc::new(Resolver(home.script.clone())), settings)
        .with_product_config(home.config_builder());
    let app = AppServer::new(&root)
        .unwrap()
        .with_thread_store(home.store.clone())
        .with_thread_factory(Arc::new(factory))
        .with_thread_unload_delay(Duration::ZERO)
        .with_plugin_management(services.protocol_adapter());
    (Server::run(app, home.store.clone()), services)
}

#[tokio::test]
async fn config_write_refreshes_hooks_on_a_turn_without_policy_overrides() {
    let home = HooksHome::new();
    let log = home.path("refreshed.log");
    let notify = home.path("initial-notify.log");
    let replacement_notify = home.path("replacement-notify.log");
    home.write_hooks(&format!(
        "notify = ['sh', '-c', \"echo initial >> {}\"]\n[features]\ncodex_hooks = true",
        notify.display()
    ));
    let (server, _) = server_with_plugins(&home).await;
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    server
        .call(
            &mut connection,
            "config/value/write",
            json!({
                "keyPath":"hooks.UserPromptSubmit", "mergeStrategy":"replace",
                "value":[{"hooks":[{"type":"command","command":logging(&log)}]}]
            }),
        )
        .await;
    server
        .call(
            &mut connection,
            "config/value/write",
            json!({
                "keyPath":"features.codex_hooks", "mergeStrategy":"replace", "value":true
            }),
        )
        .await;
    home.script.answer("done");
    server
        .turn_start(&mut connection, &thread_id, "hello")
        .await;
    connection.notification("turn/completed").await;
    assert_eq!(events_of(&logged(&log)), ["UserPromptSubmit"]);
    server
        .call(
            &mut connection,
            "config/value/write",
            json!({
                "keyPath":"features.codex_hooks", "mergeStrategy":"replace", "value":false
            }),
        )
        .await;
    server.call(&mut connection, "config/value/write", json!({
        "keyPath":"notify", "mergeStrategy":"replace",
        "value":["sh", "-c", format!("echo replacement >> {}", replacement_notify.display())]
    })).await;
    home.script.answer("done again");
    server.turn_start(&mut connection, &thread_id, "next").await;
    connection.notification("turn/completed").await;
    assert_eq!(logged(&log).len(), 2);
    timeout(PATIENCE, async {
        loop {
            if std::fs::read_to_string(&notify)
                .unwrap_or_default()
                .lines()
                .count()
                == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!replacement_notify.exists());
}

fn install(home: &HooksHome, version: &str, log: &Path) {
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join(".codex-plugin")).unwrap();
    std::fs::create_dir_all(source.path().join("hooks")).unwrap();
    std::fs::write(
        source.path().join(".codex-plugin/plugin.json"),
        format!(r#"{{"name":"sample","version":"{version}"}}"#),
    )
    .unwrap();
    std::fs::write(source.path().join("hooks/hooks.json"), json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":logging(log)}]}]}}).to_string()).unwrap();
    ra_coding::plugins::store::PluginStore::new(home.home.path().to_path_buf())
        .install(
            absolute(source.path()),
            ra_coding::plugins::PluginId::parse("sample@test").unwrap(),
        )
        .unwrap();
}

#[tokio::test]
async fn turn_without_policy_overrides_detects_another_process_plugin_cache_change() {
    let home = HooksHome::new();
    home.write_hooks(
        "[features]\ncodex_hooks = true\nplugins = true\n[plugins.\"sample@test\"]\nenabled = true",
    );
    let first = home.path("first.log");
    let second = home.path("second.log");
    install(&home, "1.0.0", &first);
    let (server, services) = server_with_plugins(&home).await;
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    home.script.answer("first");
    server.turn_start(&mut connection, &thread_id, "one").await;
    connection.notification("turn/completed").await;
    assert_eq!(logged(&first).len(), 1);
    let generation = services.generation();
    install(&home, "2.0.0", &second);
    home.script.answer("second");
    server.turn_start(&mut connection, &thread_id, "two").await;
    connection.notification("turn/completed").await;
    assert_eq!(services.generation(), generation);
    assert_eq!(logged(&first).len(), 1);
    assert_eq!(events_of(&logged(&second)), ["UserPromptSubmit"]);
}

#[tokio::test]
async fn config_write_does_not_enable_hooks_in_a_disabled_thread() {
    let home = HooksHome::new();
    let log = home.path("disabled.log");
    home.write_hooks(&format!(
        "[features]\ncodex_hooks = false\n{}",
        hook_section("UserPromptSubmit", &logging(&log))
    ));
    let (server, _) = server_with_plugins(&home).await;
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    server
        .call(
            &mut connection,
            "config/value/write",
            json!({"keyPath":"features.codex_hooks", "mergeStrategy":"replace", "value":true}),
        )
        .await;
    home.script.answer("done");
    server
        .turn_start(&mut connection, &thread_id, "hello")
        .await;
    connection.notification("turn/completed").await;
    assert!(!log.exists());
}
