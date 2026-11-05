//! Refresh through the default factory and real turn/config requests.
use super::*;
use ra_coding::plugins::{PluginConfigManager, PluginServices};
use ra_coding::skills::HostSkillsService;
use ra_paths::AbsolutePathBuf;

fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).unwrap()
}

pub(super) async fn server_with_plugins(home: &HooksHome) -> (Server, Arc<PluginServices>) {
    server_with_plugins_and_builder(home, home.config_builder()).await
}

pub(super) async fn server_with_plugins_and_builder(
    home: &HooksHome,
    builder: ConfigBuilder,
) -> (Server, Arc<PluginServices>) {
    let root = home.workspace.path().canonicalize().unwrap();
    let product_home = home.home.path().canonicalize().unwrap();
    let config = home.config_builder().build().await.unwrap();
    let services = PluginServices::new(
        PluginConfigManager::new(
            ra_executor::LOCAL_FS.clone(),
            absolute(&product_home),
            absolute(&root),
            ra_config::ConfigLoadOptions::from(LoaderOverrides {
                managed_config_path: Some(product_home.join("managed_config.toml")),
                system_config_path: Some(product_home.join("system_config.toml")),
                system_requirements_path: Some(product_home.join("requirements.toml")),
                ..LoaderOverrides::without_managed_config_for_tests()
            }),
            Arc::new(ra_config::NoopThreadConfigLoader),
            config.http_client_factory().clone(),
        ),
        Arc::new(HostSkillsService::new(absolute(&product_home), false)),
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
        .with_product_config(builder);
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
            "config/batchWrite",
            json!({"reloadUserConfig":true, "edits":[{
                "keyPath":"hooks.UserPromptSubmit", "mergeStrategy":"replace",
                "value":[{"hooks":[{"type":"command","command":logging(&log)}]}]
            }]}),
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

/// Codex reloads loaded threads only for a `config/batchWrite` that asks for it, and does so
/// before it responds.
async fn reload_write(server: &Server, connection: &mut Connection, key: &str, value: Value) {
    server
        .call(
            connection,
            "config/batchWrite",
            json!({
                "edits":[{"keyPath":key, "mergeStrategy":"replace", "value":value}],
                "reloadUserConfig":true
            }),
        )
        .await;
}

fn log_lines(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

/// Codex refreshes a loaded session from the user layer only, so hooks the thread's builder adds
/// through CLI overrides survive a user-layer write alongside the hooks the write adds.
#[tokio::test]
async fn config_write_keeps_the_builders_cli_override_hooks() {
    let home = HooksHome::new();
    let cli_log = home.path("cli.log");
    let user_log = home.path("user.log");
    home.write_hooks("[features]\ncodex_hooks = true");
    let cli = ra_config::TomlValue::try_from(
        json!([{"hooks":[{"type":"command","command":logging(&cli_log)}]}]),
    )
    .unwrap();
    let builder = home
        .config_builder()
        .cli_overrides(vec![("hooks.UserPromptSubmit".to_owned(), cli)]);
    let (server, _) = server_with_plugins_and_builder(&home, builder).await;
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    home.script.answer("before");
    server.run_turn(&mut connection, &thread_id, "before").await;
    assert_eq!((log_lines(&cli_log), log_lines(&user_log)), (1, 0));
    reload_write(
        &server,
        &mut connection,
        "hooks.UserPromptSubmit",
        json!([{"hooks":[{"type":"command","command":logging(&user_log)}]}]),
    )
    .await;
    home.script.answer("after");
    server.run_turn(&mut connection, &thread_id, "after").await;
    assert_eq!((log_lines(&cli_log), log_lines(&user_log)), (2, 1));
}

/// A write to an idle loaded thread's user configuration is what its SessionEnd runs when the
/// thread is unloaded next, with no turn in between.
#[tokio::test]
async fn an_idle_threads_session_end_uses_the_handler_written_after_its_last_turn() {
    let home = HooksHome::new();
    let old_log = home.path("old-end.log");
    let new_log = home.path("new-end.log");
    home.write_hooks(&format!(
        "[features]\ncodex_hooks = true\n{}",
        hook_section("SessionEnd", &logging(&old_log))
    ));
    let (server, _) = server_with_plugins(&home).await;
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    home.script.answer("done");
    server.run_turn(&mut connection, &thread_id, "hello").await;
    reload_write(
        &server,
        &mut connection,
        "hooks.SessionEnd",
        json!([{"hooks":[{"type":"command","command":logging(&new_log)}]}]),
    )
    .await;
    super::super::unload(&server, &mut connection, &thread_id).await;
    assert!(!old_log.exists());
    assert_eq!(events_of(&logged(&new_log)), ["SessionEnd"]);
}

/// Enabling or disabling a plugin through a reloading config write adds or removes its hooks from a
/// loaded thread's next turn.
#[tokio::test]
async fn plugin_enablement_writes_toggle_its_hooks_on_a_loaded_thread() {
    let home = HooksHome::new();
    home.write_hooks(
        "[features]\ncodex_hooks = true\nplugins = true\n[plugins.\"sample@test\"]\nenabled = true",
    );
    let log = home.path("plugin.log");
    install(&home, "1.0.0", &log);
    let (server, _) = server_with_plugins(&home).await;
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    let mut turn = async |enabled: Option<bool>, expected: usize| {
        if let Some(enabled) = enabled {
            reload_write(
                &server,
                &mut connection,
                "plugins.\"sample@test\".enabled",
                json!(enabled),
            )
            .await;
        }
        home.script.answer("done");
        server.run_turn(&mut connection, &thread_id, "turn").await;
        assert_eq!(log_lines(&log), expected, "enabled = {enabled:?}");
    };
    turn(None, 1).await;
    turn(Some(false), 1).await;
    turn(Some(true), 2).await;
}

/// Codex reloads loaded sessions only for a write that asks for it: single-value writes,
/// non-reloading batches and skill writes clear caches and leave loaded hooks unchanged.
#[tokio::test]
async fn ordinary_config_writes_wait_for_an_explicit_reload_before_changing_loaded_hooks() {
    let home = HooksHome::new();
    let old = home.path("old.log");
    let new = home.path("new.log");
    home.write_hooks(&hook_section("UserPromptSubmit", &logging(&old)));
    let (server, _) = server_with_plugins(&home).await;
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let handlers = json!([{"hooks":[{"type":"command","command":logging(&new)}]}]);
    server
        .call(
            &mut connection,
            "config/value/write",
            json!({
                "keyPath":"hooks.UserPromptSubmit", "mergeStrategy":"replace", "value":handlers
            }),
        )
        .await;
    home.script.answer("after single write");
    server
        .run_turn(&mut connection, &thread, "single write")
        .await;
    assert_eq!((log_lines(&old), log_lines(&new)), (1, 0));
    server.call(&mut connection, "config/batchWrite", json!({
        "edits":[{"keyPath":"hooks.UserPromptSubmit", "mergeStrategy":"replace", "value":handlers}],
        "reloadUserConfig":false
    })).await;
    home.script.answer("after batch write");
    server
        .run_turn(&mut connection, &thread, "batch write")
        .await;
    assert_eq!((log_lines(&old), log_lines(&new)), (2, 0));
    // Codex's skill writes clear the skill and plugin caches only.
    server
        .call(
            &mut connection,
            "skills/config/write",
            json!({"name":"sample:test","enabled":false}),
        )
        .await;
    server
        .call(
            &mut connection,
            "skills/extraRoots/set",
            json!({"extraRoots":[]}),
        )
        .await;
    home.script.answer("after skill writes");
    server
        .run_turn(&mut connection, &thread, "skill writes")
        .await;
    assert_eq!((log_lines(&old), log_lines(&new)), (3, 0));
    reload_write(&server, &mut connection, "hooks.UserPromptSubmit", handlers).await;
    home.script.answer("after reload");
    server.run_turn(&mut connection, &thread, "reload").await;
    assert_eq!((log_lines(&old), log_lines(&new)), (3, 1));
}
