//! Thread-owned refresh, including idle publication and source-owner races.
#![cfg(unix)]
use ra_coding::{
    CodingHost,
    config::{ConfigBuilder, ConfigOverrides},
    plugins::{PluginConfigManager, PluginServices},
    skills::HostSkillsService,
    thread_hooks::CodingThreadHooks,
};
use ra_config::{ConfigLoadOptions, LoaderOverrides, NoopThreadConfigLoader};
use ra_core::session::SessionId;
use ra_paths::AbsolutePathBuf;
use ra_protocol::server::ThreadProductRuntime;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).unwrap()
}
struct Fixture {
    home: tempfile::TempDir,
    plugins: Arc<PluginServices>,
    hooks: CodingThreadHooks,
}
impl Fixture {
    async fn new(cli: Vec<(String, toml::Value)>) -> Self {
        Self::with_loader(cli, Arc::new(NoopThreadConfigLoader)).await
    }
    async fn with_loader(
        cli: Vec<(String, toml::Value)>,
        loader: Arc<dyn ra_config::ThreadConfigLoader>,
    ) -> Self {
        Self::with_initial(
            cli,
            loader,
            "[features]\ncodex_hooks = true\nplugins = true",
        )
        .await
    }
    async fn with_initial(
        cli: Vec<(String, toml::Value)>,
        loader: Arc<dyn ra_config::ThreadConfigLoader>,
        initial: &str,
    ) -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), initial).unwrap();
        let overrides = LoaderOverrides::without_managed_config_for_tests();
        let builder = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(overrides.clone())
            .cli_overrides(cli)
            .thread_config_loader(loader)
            .harness_overrides(ConfigOverrides {
                cwd: Some(home.path().to_path_buf()),
                bypass_hook_trust: Some(true),
                ..Default::default()
            });
        let config = builder.clone().build().await.unwrap();
        let plugins = PluginServices::new(
            PluginConfigManager::new(
                ra_executor::LOCAL_FS.clone(),
                absolute(home.path()),
                absolute(home.path()),
                ConfigLoadOptions::from(overrides),
                Arc::new(NoopThreadConfigLoader),
                config.http_client_factory().clone(),
            ),
            Arc::new(HostSkillsService::new(absolute(home.path()), false)),
        );
        let host = CodingHost::open(home.path())
            .unwrap()
            .with_plugin_skills(plugins.clone());
        let hooks =
            CodingThreadHooks::build_refreshing_for_test(builder, &host, SessionId::new("refresh"))
                .await
                .unwrap();
        Self {
            home,
            plugins,
            hooks,
        }
    }
    fn write(&self, text: &str) {
        std::fs::write(self.home.path().join("config.toml"), text).unwrap();
    }
    async fn settled(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.hooks.refresh_generation_for_test().await != Some(self.plugins.generation())
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    async fn end(&self) {
        self.hooks
            .runtime()
            .current()
            .run_session_end(ra_coding::hooks::SessionEndRequest {
                session_id: SessionId::new("refresh"),
                turn_id: "end".into(),
                cwd: absolute(self.home.path()),
                transcript_path: None,
            })
            .await;
    }
}
fn section(event: &str, command: &str) -> String {
    format!(
        "[[hooks.{event}]]\n[[hooks.{event}.hooks]]\ntype = 'command'\ncommand = '''{command}'''\n"
    )
}
async fn wait_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn idle_invalidation_refreshes_user_hooks_and_preserves_feature_gate() {
    let fixture = Fixture::new(vec![]).await;
    let log = fixture.home.path().join("end.log");
    fixture.write(&format!(
        "[features]\ncodex_hooks = false\n{}",
        section(
            "SessionEnd",
            &format!("echo updated >> '{}'", log.display())
        )
    ));
    fixture.plugins.invalidate();
    fixture.settled().await;
    fixture.end().await;
    assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "updated");
    fixture.write("[features]\ncodex_hooks = false");
    fixture.plugins.invalidate();
    fixture.settled().await;
    assert!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .is_empty()
    );
    fixture.hooks.close().await;
}
#[tokio::test]
async fn reload_uses_the_thread_builder_cli_overrides() {
    let output = tempfile::tempdir().unwrap();
    let log = output.path().join("handlers.log");
    let cli: toml::Value = toml::from_str(&section(
        "SessionEnd",
        &format!("echo cli >> '{}'", log.display()),
    ))
    .unwrap();
    let fixture = Fixture::new(vec![(
        "hooks.SessionEnd".into(),
        cli["hooks"]["SessionEnd"].clone(),
    )])
    .await;
    fixture.write(&format!(
        "[features]\ncodex_hooks = false\n{}",
        section("SessionEnd", &format!("echo user >> '{}'", log.display()))
    ));
    fixture.plugins.invalidate();
    fixture.settled().await;
    fixture.end().await;
    let contents = std::fs::read_to_string(log).unwrap();
    let mut lines: Vec<_> = contents.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["cli", "user"]);
    fixture.hooks.close().await;
}
#[tokio::test]
async fn turn_activation_keeps_loaded_config_until_an_invalidation() {
    let fixture = Fixture::new(vec![]).await;
    let before = fixture.hooks.runtime().current();
    fixture.write(&format!(
        "[features]\ncodex_hooks = true\n{}",
        section("SessionEnd", "true")
    ));
    fixture.hooks.activate_turn().await;
    assert!(Arc::ptr_eq(&before, &fixture.hooks.runtime().current()));
    fixture.plugins.invalidate();
    fixture.hooks.activate_turn().await;
    assert_eq!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .len(),
        1
    );
    fixture.hooks.close().await;
}
#[tokio::test]
async fn failed_reload_keeps_handlers_and_later_invalidation_recovers() {
    let fixture = Fixture::new(vec![]).await;
    let before = fixture.hooks.runtime().current();
    fixture.write("this is invalid TOML");
    fixture.plugins.invalidate();
    fixture.hooks.activate_turn().await;
    assert!(Arc::ptr_eq(&before, &fixture.hooks.runtime().current()));
    fixture.write(&format!(
        "[features]\ncodex_hooks = true\n{}",
        section("SessionEnd", "true")
    ));
    fixture.plugins.invalidate();
    fixture.settled().await;
    assert_eq!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .len(),
        1
    );
    fixture.hooks.close().await;
}
#[tokio::test]
async fn closing_stops_future_refreshes() {
    let fixture = Fixture::new(vec![]).await;
    fixture.hooks.close().await;
    let before = fixture.hooks.runtime().current();
    fixture.write("[features]\ncodex_hooks = true");
    fixture.plugins.invalidate();
    tokio::task::yield_now().await;
    assert!(Arc::ptr_eq(&before, &fixture.hooks.runtime().current()));
}
#[derive(Default)]
struct GatedLoader {
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    loads: AtomicUsize,
}
impl ra_config::ThreadConfigLoader for GatedLoader {
    fn load(
        &self,
        _: ra_config::ThreadConfigContext,
    ) -> ra_config::ThreadConfigLoaderFuture<'_, Vec<ra_config::ThreadConfigSource>> {
        Box::pin(async {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(vec![])
        })
    }
}
#[tokio::test]
async fn invalidation_during_load_retries_with_the_latest_owner() {
    let loader = Arc::new(GatedLoader::default());
    let fixture = Fixture::with_loader(vec![], loader.clone()).await;
    let allow = "[features]\ncodex_hooks = true\nplugins = true\n[plugins.\"sample@test\"]";
    install_plugin(fixture.home.path(), "1.0.0", "true");
    fixture.write(allow);
    fixture.plugins.invalidate();
    fixture.settled().await;
    assert_eq!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .len(),
        1
    );
    let initial_loads = loader.loads.load(Ordering::SeqCst);
    loader.armed.store(true, Ordering::SeqCst);
    fixture.plugins.invalidate();
    tokio::time::timeout(Duration::from_secs(5), loader.entered.notified())
        .await
        .unwrap();
    fixture.write(&format!("{allow}\n[plugins._default]\nenabled = false"));
    fixture.plugins.invalidate();
    loader.release.notify_one();
    fixture.settled().await;
    assert!(loader.loads.load(Ordering::SeqCst) >= initial_loads + 2);
    assert!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .is_empty()
    );
    fixture.hooks.activate_turn().await;
    assert!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .is_empty()
    );
    fixture.hooks.close().await;
}
fn install_plugin(home: &Path, version: &str, command: &str) {
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join(".codex-plugin")).unwrap();
    std::fs::create_dir_all(source.path().join("hooks")).unwrap();
    std::fs::write(
        source.path().join(".codex-plugin/plugin.json"),
        format!(r#"{{"name":"sample","version":"{version}"}}"#),
    )
    .unwrap();
    std::fs::write(source.path().join("hooks/hooks.json"), serde_json::json!({"hooks":{"SessionEnd":[{"hooks":[{"type":"command","command":command}]}]}}).to_string()).unwrap();
    ra_coding::plugins::store::PluginStore::new(home.to_path_buf())
        .install(
            absolute(source.path()),
            ra_coding::plugins::PluginId::parse("sample@test").unwrap(),
        )
        .unwrap();
}
#[tokio::test]
async fn activation_discovers_external_plugin_installation_and_toggles_refresh_idle_hooks() {
    let fixture = Fixture::new(vec![]).await;
    fixture.write(
        "[features]\ncodex_hooks = true\nplugins = true\n[plugins.\"sample@test\"]\nenabled = true",
    );
    install_plugin(fixture.home.path(), "1.0.0", "true");
    fixture.plugins.invalidate();
    fixture.settled().await;
    assert_eq!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .len(),
        1
    );
    let before = fixture.hooks.runtime().current();
    let generation = fixture.plugins.generation();
    install_plugin(fixture.home.path(), "2.0.0", "echo changed");
    fixture.hooks.activate_turn().await;
    assert_eq!(fixture.plugins.generation(), generation);
    assert!(!Arc::ptr_eq(&before, &fixture.hooks.runtime().current()));
    assert!(
        fixture.hooks.runtime().current().preview_session_end()[0]
            .source_path
            .to_string_lossy()
            .contains("2.0.0")
    );
    fixture
        .plugins
        .set_enabled("sample@test".into(), false)
        .await
        .unwrap();
    fixture.settled().await;
    assert!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .is_empty()
    );
    fixture
        .plugins
        .set_enabled("sample@test".into(), true)
        .await
        .unwrap();
    fixture.settled().await;
    assert_eq!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .len(),
        1
    );
    fixture.hooks.close().await;
}
#[tokio::test]
async fn invalidation_preserves_running_background_handlers() {
    let fixture = Fixture::new(vec![]).await;
    let gate = fixture.home.path().join("release");
    let ready = fixture.home.path().join("ready");
    let completed = fixture.home.path().join("completed");
    fixture.write(&format!(
        "[features]\ncodex_hooks = true\n{}async = true\n",
        section(
            "UserPromptSubmit",
            &format!(
                "cat >/dev/null; touch '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; touch '{}'",
                ready.display(),
                gate.display(),
                completed.display()
            )
        )
    ));
    fixture.plugins.invalidate();
    fixture.settled().await;
    fixture
        .hooks
        .runtime()
        .current()
        .run_user_prompt_submit(ra_coding::hooks::UserPromptSubmitRequest {
            session_id: SessionId::new("refresh"),
            turn_id: "turn".into(),
            subagent: None,
            cwd: absolute(fixture.home.path()),
            transcript_path: None,
            model: "test".into(),
            permission_mode: "default".into(),
            prompt: "hello".into(),
        })
        .await;
    wait_file(&ready).await;
    fixture.write("[features]\ncodex_hooks = true");
    fixture.plugins.invalidate();
    fixture.settled().await;
    std::fs::write(gate, "go").unwrap();
    wait_file(&completed).await;
    fixture.hooks.close().await;
}

#[tokio::test]
async fn refresh_during_a_sync_dispatch_updates_the_next_dispatch() {
    let fixture = Fixture::new(vec![]).await;
    let ready = fixture.home.path().join("sync-ready");
    let release = fixture.home.path().join("sync-release");
    let log = fixture.home.path().join("new-handler.log");
    fixture.write(&format!(
        "[features]\ncodex_hooks = true\n{}",
        section(
            "SessionEnd",
            &format!(
                "touch '{}'; while [ ! -f '{}' ]; do sleep 0.01; done",
                ready.display(),
                release.display()
            )
        )
    ));
    fixture.plugins.invalidate();
    fixture.settled().await;
    let runtime = fixture.hooks.runtime().clone();
    let cwd = absolute(fixture.home.path());
    let running = tokio::spawn(async move {
        runtime
            .current()
            .run_session_end(ra_coding::hooks::SessionEndRequest {
                session_id: SessionId::new("refresh"),
                turn_id: "active".into(),
                cwd,
                transcript_path: None,
            })
            .await
    });
    wait_file(&ready).await;
    fixture.write(&format!(
        "[features]\ncodex_hooks = true\n{}",
        section("SessionEnd", &format!("echo fresh >> '{}'", log.display()))
    ));
    fixture.plugins.invalidate();
    fixture.settled().await;
    assert!(!running.is_finished(), "the old dispatch remains in flight");
    fixture.end().await;
    assert_eq!(std::fs::read_to_string(log).unwrap().trim(), "fresh");
    std::fs::write(release, "go").unwrap();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    fixture.hooks.close().await;
}

#[tokio::test]
async fn a_disabled_session_does_not_enable_hooks_after_a_user_config_write() {
    let fixture = Fixture::with_initial(
        vec![],
        Arc::new(NoopThreadConfigLoader),
        "[features]\ncodex_hooks = false",
    )
    .await;
    fixture.write(&format!(
        "[features]\ncodex_hooks = true\n{}",
        section("SessionEnd", "true")
    ));
    fixture.plugins.invalidate();
    fixture.settled().await;
    assert!(
        fixture
            .hooks
            .runtime()
            .current()
            .preview_session_end()
            .is_empty()
    );
    fixture.hooks.close().await;
}

struct ChangingLayers(std::sync::Mutex<Vec<ra_config::ConfigLayerEntry>>);
impl ra_config::ThreadConfigLoader for ChangingLayers {
    fn load(
        &self,
        _: ra_config::ThreadConfigContext,
    ) -> ra_config::ThreadConfigLoaderFuture<'_, Vec<ra_config::ThreadConfigSource>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn load_config_layers(
        &self,
        _: ra_config::ThreadConfigContext,
    ) -> ra_config::ThreadConfigLoaderFuture<'_, Vec<ra_config::ConfigLayerEntry>> {
        Box::pin(async { Ok(self.0.lock().unwrap().clone()) })
    }
}

#[tokio::test]
async fn user_refresh_preserves_project_and_managed_hook_layers() {
    let output = tempfile::tempdir().unwrap();
    let log = output.path().join("layers.log");
    let layers = |version: &str| {
        vec![
            ra_config::ConfigLayerEntry::new(
                ra_config::ConfigLayerSource::Project {
                    dot_codex_folder: absolute(&output.path().join(".rusty-agent")),
                },
                toml::from_str(&section(
                    "SessionEnd",
                    &format!("echo project-{version} >> '{}'", log.display()),
                ))
                .unwrap(),
            ),
            ra_config::ConfigLayerEntry::new(
                ra_config::ConfigLayerSource::LegacyManagedConfigTomlFromFile {
                    file: absolute(&output.path().join("managed.toml")),
                },
                toml::from_str(&section(
                    "SessionEnd",
                    &format!("echo managed-{version} >> '{}'", log.display()),
                ))
                .unwrap(),
            ),
        ]
    };
    let loader = Arc::new(ChangingLayers(std::sync::Mutex::new(layers("initial"))));
    let fixture = Fixture::with_loader(vec![], loader.clone()).await;
    *loader.0.lock().unwrap() = layers("changed");
    fixture.write(&section(
        "SessionEnd",
        &format!("echo user-new >> '{}'", log.display()),
    ));
    fixture.plugins.invalidate();
    fixture.settled().await;
    fixture.end().await;
    let contents = std::fs::read_to_string(log).unwrap();
    let mut lines: Vec<_> = contents.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["managed-initial", "project-initial", "user-new"]);
    fixture.hooks.close().await;
}
