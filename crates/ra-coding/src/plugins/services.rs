// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Explicit product services joining configuration, package management and immutable consumers.
#![allow(
    missing_docs,
    reason = "Preserve the source plugin-management service operations."
)]
use super::marketplace::MarketplaceListError;
use super::marketplace_add::{MarketplaceAddOutcome, MarketplaceAddRequest, add_marketplace};
use super::marketplace_remove::{
    MarketplaceRemoveOutcome, MarketplaceRemoveRequest, remove_marketplace,
};
use super::marketplace_upgrade::{ConfigLayerReload, ConfiguredMarketplaceUpgradeOutcome};
use super::{
    EffectivePluginsChangedCallback, PluginInstallOutcome, PluginInstallRequest, PluginLoadOutcome,
    PluginMarketplaceContext, PluginMarketplaceScope, PluginReadOutcome, PluginReadRequest,
    PluginRuntimeConfig, PluginsManager,
};
use crate::skills::{HostSkillsLoadInput, HostSkillsService, HostSkillsSnapshot};
use futures::StreamExt;
use ra_config::{ConfigLayerStack, ConfigLoadOptions, McpServerConfig, ThreadConfigLoader};
use ra_filesystem::ExecutorFileSystem;
use ra_http::HttpClientFactory;
use ra_paths::AbsolutePathBuf;
use std::{
    io,
    sync::{Arc, Weak},
};
use tokio::sync::{Mutex, watch};

/// Captures the actual loader and entry-point options for every subsequent reload.
#[derive(Clone)]
pub struct PluginConfigManager {
    fs: Arc<dyn ExecutorFileSystem>,
    home: AbsolutePathBuf,
    cwd: AbsolutePathBuf,
    cli_overrides: Vec<(String, toml::Value)>,
    options: ConfigLoadOptions,
    thread_config_loader: Arc<dyn ThreadConfigLoader>,
    http_client_factory: HttpClientFactory,
}
impl PluginConfigManager {
    pub fn new(
        fs: Arc<dyn ExecutorFileSystem>,
        home: AbsolutePathBuf,
        cwd: AbsolutePathBuf,
        options: ConfigLoadOptions,
        thread_config_loader: Arc<dyn ThreadConfigLoader>,
        http_client_factory: HttpClientFactory,
    ) -> Self {
        Self {
            fs,
            home,
            cwd,
            cli_overrides: Vec::new(),
            options,
            thread_config_loader,
            http_client_factory,
        }
    }
    #[must_use]
    pub fn with_cli_overrides(mut self, overrides: Vec<(String, toml::Value)>) -> Self {
        self.cli_overrides = overrides;
        self
    }
    #[must_use]
    pub fn home(&self) -> &AbsolutePathBuf {
        &self.home
    }
    /// The only file the configuration writer accepts: the overridden or default user config.
    pub fn user_config_path(&self) -> io::Result<AbsolutePathBuf> {
        self.options
            .loader_overrides
            .user_config_path(self.home.as_path())
    }
    #[must_use]
    pub fn cwd(&self) -> &AbsolutePathBuf {
        &self.cwd
    }
    pub async fn load_config_layers_for_cwd(
        &self,
        cwd: Option<AbsolutePathBuf>,
    ) -> io::Result<ConfigLayerStack> {
        ra_config::loader::load_config_layers_state(
            self.fs.as_ref(),
            self.home.as_path(),
            cwd,
            &self.cli_overrides,
            self.options.clone(),
            self.thread_config_loader.as_ref(),
        )
        .await
    }
    pub async fn load_for_cwd(&self, cwd: AbsolutePathBuf) -> io::Result<PluginRuntimeConfig> {
        let layers = self.load_config_layers_for_cwd(Some(cwd.clone())).await?;
        self.project(layers, cwd)
    }
    pub async fn load_latest_config(&self) -> io::Result<PluginRuntimeConfig> {
        self.load_non_project_config().await
    }
    pub async fn load_non_project_config(&self) -> io::Result<PluginRuntimeConfig> {
        let layers = self.load_config_layers_for_cwd(None).await?;
        self.project(layers, self.cwd.clone())
    }
    fn project(
        &self,
        layers: ConfigLayerStack,
        cwd: AbsolutePathBuf,
    ) -> io::Result<PluginRuntimeConfig> {
        PluginRuntimeConfig::from_layer_stack(
            layers,
            self.home.clone(),
            cwd,
            self.http_client_factory.clone(),
        )
    }
    /// Adapts the same loader to the source blocking Git/activation worker.
    #[must_use]
    pub fn config_reloader(&self, cwd: Option<AbsolutePathBuf>) -> ConfigLayerReload {
        let runtime = tokio::runtime::Handle::current();
        let manager = self.clone();
        Arc::new(move || runtime.block_on(manager.load_config_layers_for_cwd(cwd.clone())))
    }
    pub async fn marketplace_context(
        &self,
        cwds: Vec<AbsolutePathBuf>,
    ) -> io::Result<PluginMarketplaceContext> {
        let catalog = if cwds.is_empty() {
            self.load_non_project_config().await?
        } else {
            self.load_latest_config().await?
        };
        let global_config = catalog.plugins_config_input();
        if cwds.is_empty() {
            return Ok(PluginMarketplaceContext {
                global_config: global_config.clone(),
                scopes: vec![PluginMarketplaceScope {
                    cwd: None,
                    config: global_config,
                }],
                load_errors: Vec::new(),
            });
        }
        let outcomes = futures::stream::iter(cwds)
            .map(|cwd| async move {
                self.load_for_cwd(cwd.clone())
                    .await
                    .map(|config| PluginMarketplaceScope {
                        cwd: Some(cwd.clone()),
                        config: config.plugins_config_input(),
                    })
                    .map_err(|err| MarketplaceListError {
                        path: cwd,
                        message: format!("failed to reload config: {err}"),
                    })
            })
            .buffered(5)
            .collect::<Vec<_>>()
            .await;
        let mut scopes = Vec::new();
        let mut load_errors = Vec::new();
        for outcome in outcomes {
            match outcome {
                Ok(scope) => scopes.push(scope),
                Err(err) => load_errors.push(err),
            }
        }
        Ok(PluginMarketplaceContext {
            global_config,
            scopes,
            load_errors,
        })
    }
}

/// One source-resolved generation. Keeping it retains the exact skill filesystems and attribution.
#[derive(Clone)]
pub struct PluginSnapshot {
    pub config: Arc<PluginRuntimeConfig>,
    pub plugins: Arc<PluginLoadOutcome<McpServerConfig>>,
    pub skills: HostSkillsSnapshot,
    pub mcp_catalog: Arc<ra_mcp::ResolvedMcpCatalog>,
}
impl PluginSnapshot {
    #[must_use]
    pub fn hooks_config(&self) -> crate::hooks::HooksConfig {
        self.config.hooks_config(&self.plugins)
    }
    #[must_use]
    pub fn explicit_plugin_instructions(
        &self,
        input: &[ra_protocol::server::protocol::UserInput],
        visible_mcp_servers: &[String],
    ) -> Vec<String> {
        let attribution = self.mcp_catalog.plugin_attributions_by_server_name();
        super::collect_explicit_plugin_mentions(input, self.plugins.capability_summaries())
            .iter()
            .filter_map(|plugin| {
                let available = visible_mcp_servers
                    .iter()
                    .filter(|name| {
                        attribution
                            .get(*name)
                            .is_some_and(|owner| owner.display_name() == plugin.display_name)
                    })
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                // Local app declarations do not install an account-backed app tool runtime.
                super::render_explicit_plugin_instructions(plugin, &available, &[])
            })
            .collect()
    }
    #[must_use]
    pub fn explicit_skills(
        &self,
        input: &[ra_protocol::server::protocol::UserInput],
    ) -> Vec<crate::skills::SkillMetadata> {
        crate::skills::collect_explicit_skill_mentions(
            input,
            self.skills.outcome(),
            &std::collections::HashMap::default(),
        )
    }
}

/// Process-owned package services. Hosts select them explicitly; SDK agents remain unaffected.
pub struct PluginServices {
    pub config_manager: PluginConfigManager,
    pub plugins: Arc<PluginsManager>,
    pub skills: Arc<HostSkillsService>,
    changes: watch::Sender<u64>,
    // Direct callers receive the same serialization as app-server's global config queue.
    writes: Mutex<()>,
}
impl PluginServices {
    pub fn new(config_manager: PluginConfigManager, skills: Arc<HostSkillsService>) -> Arc<Self> {
        let plugins = Arc::new(PluginsManager::new(
            config_manager.home().to_path_buf(),
            skills.clone(),
        ));
        Arc::new(Self {
            config_manager,
            plugins,
            skills,
            changes: watch::channel(0).0,
            writes: Mutex::new(()),
        })
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
    pub fn effective_plugins_changed_callback(self: &Arc<Self>) -> EffectivePluginsChangedCallback {
        let weak: Weak<Self> = Arc::downgrade(self);
        Arc::new(move |_| {
            if let Some(services) = weak.upgrade() {
                services.invalidate();
            }
        })
    }
    /// Clears shared parse caches and marks every registered session for source reconciliation.
    pub fn invalidate(&self) {
        self.plugins.clear_cache();
        self.skills.clear_cache();
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
    pub async fn snapshot(&self, cwd: AbsolutePathBuf) -> io::Result<PluginSnapshot> {
        let config = Arc::new(self.config_manager.load_for_cwd(cwd).await?);
        self.snapshot_for_config(config).await
    }
    pub async fn snapshot_for_config(
        &self,
        config: Arc<PluginRuntimeConfig>,
    ) -> io::Result<PluginSnapshot> {
        let input = config.plugins_config_input();
        let plugins = Arc::new(self.plugins.plugins_for_config(&input).await);
        let skill_input = HostSkillsLoadInput::new(
            config.cwd.clone(),
            plugins.effective_plugin_skill_roots(),
            config.config_layer_stack.clone(),
        )
        .with_plugin_skill_snapshots(self.plugins.plugin_skill_snapshots_for_config(&input));
        let skills = self
            .skills
            .snapshot_for_config(&skill_input, Some(self.config_manager.fs.clone()))
            .await;
        let mcp_catalog = Arc::new(config.mcp_catalog(&plugins));
        Ok(PluginSnapshot {
            config,
            plugins,
            skills,
            mcp_catalog,
        })
    }
    pub async fn install(
        &self,
        request: PluginInstallRequest,
    ) -> Result<PluginInstallOutcome, super::PluginInstallError> {
        let _write = self.writes.lock().await;
        let config = self
            .config_manager
            .load_for_cwd(
                request
                    .marketplace_path
                    .parent()
                    .map(AbsolutePathBuf::from_absolute_path)
                    .transpose()
                    .map_err(|err| super::PluginInstallError::Config(anyhow::Error::new(err)))?
                    .unwrap_or_else(|| self.config_manager.home().clone()),
            )
            .await
            .map_err(|err| super::PluginInstallError::Config(anyhow::Error::new(err)))?;
        let outcome = self
            .plugins
            .install_plugin(&config.config_layer_stack, request)
            .await?;
        self.invalidate();
        Ok(outcome)
    }
    pub async fn uninstall(&self, id: String) -> Result<(), super::PluginUninstallError> {
        let _write = self.writes.lock().await;
        self.plugins.uninstall_plugin(id).await?;
        self.invalidate();
        Ok(())
    }
    pub async fn set_enabled(&self, id: String, enabled: bool) -> io::Result<()> {
        let _write = self.writes.lock().await;
        // This is a configuration write, not an installation or an automatic trust grant.
        ra_config::set_user_plugin_enabled(self.config_manager.home().as_path(), id, enabled)
            .await?;
        self.invalidate();
        Ok(())
    }
    /// Writes one user-config value; plugin toggles go through this path as in the source.
    pub async fn write_config_value(
        &self,
        params: ra_protocol::server::protocol::ConfigValueWriteParams,
    ) -> Result<
        ra_protocol::server::protocol::ConfigWriteResponse,
        crate::config_manager_service::ConfigManagerError,
    > {
        let _write = self.writes.lock().await;
        let response = Box::pin(self.config_manager.write_value(params)).await?;
        self.invalidate();
        Ok(response)
    }
    /// Writes ordered user-config edits. Edits limited to session-static defaults do not clear
    /// plugin or skill caches, matching the source mutation handling.
    pub async fn batch_write_config(
        &self,
        params: ra_protocol::server::protocol::ConfigBatchWriteParams,
    ) -> Result<
        ra_protocol::server::protocol::ConfigWriteResponse,
        crate::config_manager_service::ConfigManagerError,
    > {
        let session_defaults_only = !params.edits.is_empty()
            && params.edits.iter().all(|edit| {
                matches!(
                    edit.key_path.as_str(),
                    "model"
                        | "model_reasoning_effort"
                        | "plan_mode_reasoning_effort"
                        | "service_tier"
                        | "personality"
                )
            });
        let _write = self.writes.lock().await;
        let response = Box::pin(self.config_manager.batch_write(params)).await?;
        if !session_defaults_only {
            self.invalidate();
        }
        Ok(response)
    }
    /// Persists a skill enablement override in the product home config.
    pub async fn write_skill_config(&self, edit: ra_config::ConfigEdit) -> anyhow::Result<()> {
        let _write = self.writes.lock().await;
        ra_config::ConfigEditsBuilder::new(self.config_manager.home().as_path())
            .with_edits([edit])
            .apply()
            .await?;
        self.invalidate();
        Ok(())
    }
    pub async fn read(&self, request: PluginReadRequest) -> Result<PluginReadOutcome, String> {
        let config = self
            .config_manager
            .load_non_project_config()
            .await
            .map_err(|err| err.to_string())?;
        self.plugins
            .read_plugin_for_config(&config.plugins_config_input(), &request)
            .await
            .map_err(|err| err.to_string())
    }
    pub async fn add_marketplace(
        &self,
        request: MarketplaceAddRequest,
    ) -> Result<MarketplaceAddOutcome, super::marketplace_add::MarketplaceAddError> {
        let _write = self.writes.lock().await;
        let config = self
            .config_manager
            .load_non_project_config()
            .await
            .map_err(|err| {
                super::marketplace_add::MarketplaceAddError::Internal(err.to_string())
            })?;
        let outcome = add_marketplace(
            self.config_manager.home().to_path_buf(),
            config.config_layer_stack.requirements().clone(),
            request,
        )
        .await?;
        self.invalidate();
        Ok(outcome)
    }
    pub async fn remove_marketplace(
        &self,
        request: MarketplaceRemoveRequest,
    ) -> Result<MarketplaceRemoveOutcome, super::marketplace_remove::MarketplaceRemoveError> {
        let _write = self.writes.lock().await;
        let config = self
            .config_manager
            .load_non_project_config()
            .await
            .map_err(|err| {
                super::marketplace_remove::MarketplaceRemoveError::Internal(err.to_string())
            })?;
        let outcome = remove_marketplace(
            self.config_manager.home().to_path_buf(),
            config.config_layer_stack,
            request,
        )
        .await?;
        self.invalidate();
        Ok(outcome)
    }
    pub async fn upgrade_marketplaces(
        &self,
        name: Option<String>,
    ) -> Result<ConfiguredMarketplaceUpgradeOutcome, String> {
        let _write = self.writes.lock().await;
        let config = self
            .config_manager
            .load_non_project_config()
            .await
            .map_err(|err| err.to_string())?;
        let input = config.plugins_config_input();
        let reload = self.config_manager.config_reloader(None);
        let plugins = self.plugins.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            plugins.upgrade_configured_marketplaces_for_config(&input, name.as_deref(), &reload)
        })
        .await
        .map_err(|err| err.to_string())??;
        self.invalidate();
        Ok(outcome)
    }
    /// Explicit startup entry point; ordinary read/list calls need not initiate downloads.
    pub async fn start_background_updates(self: &Arc<Self>) -> io::Result<()> {
        let config = self.config_manager.load_non_project_config().await?;
        self.plugins.maybe_start_plugin_startup_tasks_for_config(
            &config.plugins_config_input(),
            self.config_manager.config_reloader(None),
            Some(self.effective_plugins_changed_callback()),
        );
        Ok(())
    }
}

/// The outcome of the latest reconciliation of a bound consumer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsumerReconcile {
    /// The service invalidation generation this outcome reflects.
    generation: u64,
    /// Declarations left out of the consumer, with the reason.
    unavailable: Vec<(String, String)>,
    /// A configuration or replacement failure; the consumer keeps its previous state.
    error: Option<String>,
}

impl ConsumerReconcile {
    /// The service invalidation generation this outcome reflects.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Declarations left out of the consumer, with the reason.
    #[must_use]
    pub fn unavailable(&self) -> &[(String, String)] {
        &self.unavailable
    }

    /// A configuration or replacement failure; the consumer keeps its previous state.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// Keeps a consumer reconciled with the services' effective configuration and plugins.
///
/// Dropping the binding stops future reconciliation; the consumer keeps its last applied state.
pub struct ConsumerBinding {
    task: tokio::task::JoinHandle<()>,
    applied: watch::Receiver<Option<ConsumerReconcile>>,
}
impl ConsumerBinding {
    /// Observes reconciliation outcomes; empty until the first one is published.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Option<ConsumerReconcile>> {
        self.applied.clone()
    }
    /// Waits until an outcome for `generation` or a later invalidation is published. Returns the
    /// latest outcome, or none when the binding ended first.
    pub async fn wait_for(&self, generation: u64) -> Option<ConsumerReconcile> {
        let mut applied = self.applied.clone();
        applied
            .wait_for(|outcome| {
                outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.generation >= generation)
            })
            .await
            .ok()
            .and_then(|outcome| outcome.clone())
    }
}
impl Drop for ConsumerBinding {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PluginServices {
    /// The current invalidation generation; each successful mutation advances it.
    pub fn generation(&self) -> u64 {
        *self.changes.borrow()
    }

    /// Applies the effective state for `cwd` now and after every invalidation. Requests coalesce:
    /// only the newest state is applied. `consumer` is held weakly, so the binding ends with it.
    fn bind_consumer<T, F, Fut>(
        self: &Arc<Self>,
        consumer: &Arc<T>,
        cwd: AbsolutePathBuf,
        apply: F,
    ) -> ConsumerBinding
    where
        T: Send + Sync + 'static,
        F: Fn(Arc<T>, PluginSnapshot) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = (Vec<(String, String)>, Option<String>)> + Send,
    {
        let services = Arc::downgrade(self);
        let consumer = Arc::downgrade(consumer);
        let mut changes = self.changes.subscribe();
        let (publish, applied) = watch::channel(None);
        let task = tokio::spawn(async move {
            loop {
                let generation = *changes.borrow_and_update();
                let (Some(services), Some(consumer)) = (services.upgrade(), consumer.upgrade())
                else {
                    break;
                };
                let outcome = match services.snapshot(cwd.clone()).await {
                    Ok(snapshot) => {
                        let (unavailable, error) = apply(consumer, snapshot).await;
                        ConsumerReconcile {
                            generation,
                            unavailable,
                            error,
                        }
                    }
                    Err(error) => ConsumerReconcile {
                        generation,
                        unavailable: Vec::new(),
                        error: Some(format!("failed to reload config: {error}")),
                    },
                };
                drop(services);
                publish.send_replace(Some(outcome));
                if changes.changed().await.is_err() {
                    break;
                }
            }
        });
        ConsumerBinding { task, applied }
    }

    /// Keeps `runtime` reconciled with the effective MCP catalog, as the source requests an MCP
    /// runtime refresh for each loaded thread when effective plugins change.
    pub fn bind_mcp_runtime(
        self: &Arc<Self>,
        runtime: &Arc<crate::mcp::CodingMcpRuntime>,
        cwd: AbsolutePathBuf,
    ) -> ConsumerBinding {
        let fallback_cwd = cwd.clone();
        self.bind_consumer(runtime, cwd, move |runtime, snapshot| {
            let plan = crate::mcp::product_mcp_servers(
                &snapshot.mcp_catalog,
                fallback_cwd.as_path(),
                &snapshot.config.http_client_factory,
            );
            async move {
                let error = runtime
                    .replace_mcp_servers(plan.servers)
                    .await
                    .err()
                    .map(|error| error.to_string());
                (plan.unavailable, error)
            }
        })
    }

    /// Keeps `runtime` on the effective hook configuration, as the source refreshes loaded hook
    /// runtimes when effective plugins change. Running background handlers are kept.
    pub fn bind_hook_runtime(
        self: &Arc<Self>,
        runtime: &Arc<crate::hooks::ProductHookRuntime>,
        cwd: AbsolutePathBuf,
    ) -> ConsumerBinding {
        self.bind_consumer(runtime, cwd, |runtime, snapshot| {
            runtime.refresh(snapshot.hooks_config());
            async { (Vec::new(), None) }
        })
    }
}
