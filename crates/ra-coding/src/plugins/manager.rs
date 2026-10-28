// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Preserve source plugin management public records and operations."
)]
use super::installed_marketplaces::installed_marketplace_roots_from_layer_stack;
use super::loader::{
    PluginHookLoadOutcome, TargetCuratedMarketplace, configured_curated_plugin_ids_from_codex_home,
    curated_plugin_cache_version, load_plugin_apps_from_manifest, load_plugin_hooks,
    load_plugin_hooks_from_layer_stack, load_plugin_mcp_servers_from_manifest_with_format,
    load_plugin_skill_inventory, load_plugins_from_layer_stack, log_plugin_load_errors,
    plugin_is_eligible_for_target_marketplace, refresh_curated_plugin_cache,
    refresh_non_curated_plugin_cache_detailed,
    refresh_non_curated_plugin_cache_force_reinstall_detailed,
};
use super::manifest::{PluginManifestFormat, PluginManifestInterface};
use super::marketplace::{
    MarketplaceError, MarketplaceInterface, MarketplaceListError, MarketplaceListOutcome,
    MarketplacePluginAuthPolicy, MarketplacePluginManifestFallback, MarketplacePluginPolicy,
    MarketplacePluginSource, ResolvedMarketplacePlugin, find_installable_marketplace_plugin,
    find_marketplace_plugin, home_dir, list_marketplaces_with_cache,
    plugin_interface_with_marketplace_category,
};
use super::marketplace_policy::{MarketplacePolicy, configured_plugins_from_stack};
use super::marketplace_upgrade::{
    ConfigLayerReload, ConfiguredMarketplaceUpgradeError, ConfiguredMarketplaceUpgradeOutcome,
    upgrade_configured_git_marketplaces_with_mode,
};
use super::materialize::materialize_marketplace_plugin_source;
use super::remote_legacy::RemotePluginFetchError;
use super::skill_snapshots::new_plugin_skill_snapshots;
use super::startup_sync::{
    OPENAI_PLUGINS_GIT_URL, curated_plugins_api_marketplace_path, read_curated_plugins_sha,
    sync_openai_plugins_repo,
};
use super::store::{
    PluginInstallResult as StorePluginInstallResult, PluginStore, PluginStoreError,
    error_context_sub_error_type,
};
use super::{
    AppConnectorId, PluginCapabilitySummary, PluginGitMode, PluginId, PluginIdError,
    PluginIdentity, PluginSkillRoot, app_connector_ids_from_declarations,
    is_openai_curated_marketplace_name, prompt_safe_plugin_description,
};
use crate::hooks::plugin_hook_declarations;
use crate::skills::{SkillMetadata, SkillProduct as Product, SkillRootLoader, SkillRootSnapshots};
use ra_config::{
    ConfigLayerStack, McpServerConfig, PluginConfig, PluginsConfigToml, SkillConfigRules,
    clear_user_plugin, set_user_plugin_enabled, skill_config_rules_from_stack,
};
use ra_core::hook::HookEventName;
use ra_http::HttpClientFactory;
use ra_paths::{AbsolutePathBuf, PathUri};
use std::time::{Duration, Instant};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Semaphore, watch};
use tracing::warn;
type LoadedPlugin = super::LoadedPlugin<McpServerConfig>;
type PluginLoadOutcome = super::PluginLoadOutcome<McpServerConfig>;
#[path = "marketplace_context.rs"]
mod marketplace_context;
pub use marketplace_context::{PluginMarketplaceContext, PluginMarketplaceScope};
static CURATED_REPO_SYNC_STARTED: AtomicBool = AtomicBool::new(false);
const LOADED_PLUGINS_CACHE_CAPACITY: usize = 8;
const FEATURED_PLUGIN_IDS_CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 3);
pub type EffectivePluginsChangedCallback =
    Arc<dyn Fn(EffectivePluginsChange) + Send + Sync + 'static>;
#[derive(Debug, Clone)]
pub struct PluginsConfigInput {
    pub config_layer_stack: ConfigLayerStack,
    pub plugins: PluginsConfigToml,
    pub model_provider_id: String,
    pub plugins_enabled: bool,
    pub chatgpt_base_url: String,
    remote_http_clients: Arc<OnceLock<Arc<dyn super::http_client_selector::HttpClientSelector>>>,
    http_client_factory: HttpClientFactory,
}
impl PluginsConfigInput {
    #[must_use]
    pub fn new(
        config_layer_stack: ConfigLayerStack,
        plugins: PluginsConfigToml,
        model_provider_id: String,
        plugins_enabled: bool,
        http_client_factory: HttpClientFactory,
    ) -> Self {
        Self {
            config_layer_stack,
            plugins,
            model_provider_id,
            plugins_enabled,
            chatgpt_base_url: super::remote::DEFAULT_CHATGPT_BASE_URL.to_string(),
            remote_http_clients: Arc::new(OnceLock::new()),
            http_client_factory,
        }
    }

    /// Clones share a lazy route-aware pool while each request keeps its current endpoint.
    #[must_use]
    pub fn remote_plugin_service_config(&self) -> super::remote::RemotePluginServiceConfig {
        let http_clients = self.remote_http_clients.get_or_init(|| {
            Arc::new(ra_http::RouteAwareClientPool::new_without_request_logging(
                self.http_client_factory.clone(),
                ra_http::ClientRouteClass::Api,
            ))
        });
        super::remote::RemotePluginServiceConfig {
            chatgpt_base_url: self.chatgpt_base_url.clone(),
            http_clients: Arc::clone(http_clients),
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectivePluginsChange {}
#[derive(Clone, PartialEq, Eq)]
struct NonCuratedCacheRefreshRequest {
    roots: Vec<AbsolutePathBuf>,
    configured_plugin_keys: Vec<String>,
    configured_plugin_sources: Vec<NonCuratedPluginSource>,
    mode: NonCuratedCacheRefreshMode,
    git_mode: PluginGitMode,
}

#[derive(Clone, PartialEq, Eq)]
struct NonCuratedPluginSource {
    marketplace_path: AbsolutePathBuf,
    plugin_key: String,
    source: MarketplacePluginSource,
    local_version: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NonCuratedCacheRefreshMode {
    IfVersionChanged,
    ForceReinstall,
}

#[derive(Default)]
struct NonCuratedCacheRefreshState {
    requested: Option<NonCuratedCacheRefreshRequest>,
    last_refreshed: Option<NonCuratedCacheRefreshRequest>,
    in_flight: bool,
}

#[derive(Clone, Copy, Default)]
struct NonCuratedCacheRefreshCompletion {
    sequence: u64,
    changed_sequence: u64,
}

#[derive(Default)]
struct ConfiguredMarketplaceUpgradeState {
    in_flight: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallRequest {
    pub plugin_name: String,
    pub marketplace_path: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginReadRequest {
    pub plugin_name: String,
    pub marketplace_path: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallOutcome {
    pub plugin_id: PluginId,
    pub plugin_version: String,
    pub installed_path: AbsolutePathBuf,
    pub auth_policy: MarketplacePluginAuthPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginReadOutcome {
    pub marketplace_name: String,
    pub marketplace_path: Option<AbsolutePathBuf>,
    pub plugin: PluginDetail,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginDetail {
    pub id: String,
    pub name: String,
    pub local_version: Option<String>,
    pub description: Option<String>,
    pub source: MarketplacePluginSource,
    pub policy: MarketplacePluginPolicy,
    pub interface: Option<PluginManifestInterface>,
    pub keywords: Vec<String>,
    pub installed: bool,
    pub enabled: bool,
    pub skills: Vec<SkillMetadata>,
    pub disabled_skill_paths: HashSet<PathUri>,
    /// Packaged onboarding path; callers apply visibility and enablement.
    pub onboarding_skill: Option<AbsolutePathBuf>,
    pub hooks: Vec<PluginHookSummary>,
    pub apps: Vec<AppConnectorId>,
    pub app_category_by_id: HashMap<String, String>,
    pub mcp_server_names: Vec<String>,
    pub details_unavailable_reason: Option<PluginDetailsUnavailableReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginHookSummary {
    pub key: String,
    pub event_name: HookEventName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginDetailsUnavailableReason {
    InstallRequiredForRemoteSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMarketplace {
    pub name: String,
    pub path: AbsolutePathBuf,
    pub interface: Option<MarketplaceInterface>,
    pub plugins: Vec<ConfiguredMarketplacePlugin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMarketplacePlugin {
    pub id: String,
    pub name: String,
    pub local_version: Option<String>,
    pub installed_version: Option<String>,
    pub source: MarketplacePluginSource,
    pub policy: MarketplacePluginPolicy,
    pub interface: Option<PluginManifestInterface>,
    pub keywords: Vec<String>,
    pub manifest_fallback: Option<MarketplacePluginManifestFallback>,
    pub installed: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfiguredMarketplaceListOutcome {
    pub marketplaces: Vec<ConfiguredMarketplace>,
    pub errors: Vec<MarketplaceListError>,
}

#[derive(Default)]
struct ConfiguredPluginStates {
    installed: HashSet<String>,
    enabled: HashSet<String>,
}

impl From<PluginDetail> for PluginCapabilitySummary {
    fn from(value: PluginDetail) -> Self {
        let has_skills = value.skills.iter().any(|skill| {
            !value
                .disabled_skill_paths
                .contains(&skill.path_to_skills_md)
        });
        Self {
            config_name: value.id,
            display_name: value.name,
            plugin_namespace: None,
            description: prompt_safe_plugin_description(value.description.as_deref()),
            has_skills,
            mcp_server_names: value.mcp_server_names,
            app_connector_ids: value.apps,
        }
    }
}

pub struct PluginsManager {
    codex_home: PathBuf,
    store: PluginStore,
    configured_marketplace_upgrade_state: RwLock<ConfiguredMarketplaceUpgradeState>,
    non_curated_cache_refresh_lock: Semaphore,
    non_curated_cache_refresh_state: RwLock<NonCuratedCacheRefreshState>,
    non_curated_cache_refresh_completion: watch::Sender<NonCuratedCacheRefreshCompletion>,
    loaded_plugins_cache: Mutex<LoadedPluginsCache>,
    featured_plugin_ids_cache: RwLock<Option<CachedFeaturedPluginIds>>,
    loaded_plugins_load_semaphore: Semaphore,
    skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    restriction_product: Option<Product>,
}
// No account identity participates in anonymous metadata requests; the source URL key remains.
#[derive(Clone, PartialEq, Eq)]
struct FeaturedPluginIdsCacheKey {
    chatgpt_base_url: String,
}

#[derive(Clone)]
struct CachedFeaturedPluginIds {
    key: FeaturedPluginIdsCacheKey,
    expires_at: Instant,
    featured_plugin_ids: Vec<String>,
}

fn featured_plugin_ids_cache_key(config: &PluginsConfigInput) -> FeaturedPluginIdsCacheKey {
    FeaturedPluginIdsCacheKey {
        chatgpt_base_url: config.chatgpt_base_url.clone(),
    }
}

#[derive(Clone)]
struct LoadedPluginsCacheEntry {
    key: PluginLoadCacheKey,
    plugins: Vec<LoadedPlugin>,
    plugin_skill_snapshots: SkillRootSnapshots<PluginSkillRoot>,
}

#[derive(Default)]
struct LoadedPluginsCache {
    generation: u64,
    // Most recently used first.
    entries: VecDeque<Arc<LoadedPluginsCacheEntry>>,
}

impl LoadedPluginsCache {
    fn get(
        cache: &Mutex<Self>,
        key: &PluginLoadCacheKey,
        store: &PluginStore,
    ) -> Option<Arc<LoadedPluginsCacheEntry>> {
        let (generation, entry) = {
            let cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = cache.entries.iter().find(|entry| &entry.key == key)?;
            (cache.generation, Arc::clone(entry))
        };
        // Another process can replace installed versions without invalidating this manager.
        // Keep the parsed skills paired with the installation whose paths they advertise.
        // Validate outside the cache lock so filesystem work does not block other cache keys.
        let stale = entry.plugins.iter().any(|plugin| {
            let Ok(plugin_id) = PluginId::parse(&plugin.config_name) else {
                return false;
            };
            let installed_root = store
                .active_plugin_root(&plugin_id)
                .unwrap_or_else(|| store.plugin_base_root(&plugin_id));
            installed_root != plugin.root
        });
        let mut cache = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.generation != generation {
            return None;
        }
        // A load can replace this key without changing the cache generation.
        let index = cache
            .entries
            .iter()
            .position(|current| Arc::ptr_eq(current, &entry))?;
        let current = cache.entries.remove(index)?;
        if stale {
            return None;
        }
        cache.entries.push_front(current);
        Some(entry)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct PluginLoadCacheKey {
    configured_plugins: HashMap<String, PluginConfig>,
    plugin_policy: PluginsConfigToml,
    skill_config_rules: SkillConfigRules,
}

impl PluginLoadCacheKey {
    fn from_config(config: &PluginsConfigInput, codex_home: &Path) -> Self {
        Self {
            configured_plugins: configured_plugins_from_stack(
                &config.config_layer_stack,
                codex_home,
            ),
            plugin_policy: config.plugins.clone(),
            skill_config_rules: skill_config_rules_from_stack(&config.config_layer_stack),
        }
    }
}

impl PluginsManager {
    pub fn new(
        codex_home: PathBuf,
        skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    ) -> Self {
        Self::new_with_options(codex_home, Some(Product::Codex), skill_root_loader)
    }
    pub fn new_with_options(
        codex_home: PathBuf,
        restriction_product: Option<Product>,
        skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    ) -> Self {
        // Product restrictions are enforced at marketplace admission time for a given RA_HOME:
        // listing, install, and curated refresh all consult this restriction context before new
        // plugins enter local config or cache. After admission, runtime plugin loading trusts the
        // contents of that RA_HOME and does not re-filter configured plugins by product, so
        // already-admitted plugins may continue exposing MCP servers/tools from shared local state.
        //
        // This assumes a single RA_HOME is only used by one product.
        let store = PluginStore::new(codex_home.clone());
        Self {
            codex_home,
            store,
            configured_marketplace_upgrade_state: RwLock::new(
                ConfiguredMarketplaceUpgradeState::default(),
            ),
            non_curated_cache_refresh_lock: Semaphore::new(/*permits*/ 1),
            non_curated_cache_refresh_state: RwLock::new(NonCuratedCacheRefreshState::default()),
            non_curated_cache_refresh_completion: watch::channel(
                NonCuratedCacheRefreshCompletion::default(),
            )
            .0,
            loaded_plugins_cache: Mutex::new(LoadedPluginsCache::default()),
            featured_plugin_ids_cache: RwLock::new(None),
            loaded_plugins_load_semaphore: Semaphore::new(/*permits*/ 1),
            skill_root_loader,
            restriction_product,
        }
    }
    fn cached_featured_plugin_ids(
        &self,
        cache_key: &FeaturedPluginIdsCacheKey,
    ) -> Option<Vec<String>> {
        {
            let cache = match self.featured_plugin_ids_cache.read() {
                Ok(cache) => cache,
                Err(err) => err.into_inner(),
            };
            let now = Instant::now();
            if let Some(cached) = cache.as_ref()
                && now < cached.expires_at
                && cached.key == *cache_key
            {
                return Some(cached.featured_plugin_ids.clone());
            }
        }

        let mut cache = match self.featured_plugin_ids_cache.write() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        let now = Instant::now();
        if cache
            .as_ref()
            .is_some_and(|cached| now >= cached.expires_at || cached.key != *cache_key)
        {
            *cache = None;
        }
        None
    }

    fn write_featured_plugin_ids_cache(
        &self,
        cache_key: FeaturedPluginIdsCacheKey,
        featured_plugin_ids: &[String],
    ) {
        let mut cache = match self.featured_plugin_ids_cache.write() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        *cache = Some(CachedFeaturedPluginIds {
            key: cache_key,
            expires_at: Instant::now() + FEATURED_PLUGIN_IDS_CACHE_TTL,
            featured_plugin_ids: featured_plugin_ids.to_vec(),
        });
    }

    pub async fn featured_plugin_ids_for_config(
        &self,
        config: &PluginsConfigInput,
    ) -> Result<Vec<String>, RemotePluginFetchError> {
        if !config.plugins_enabled {
            return Ok(Vec::new());
        }

        let cache_key = featured_plugin_ids_cache_key(config);
        if let Some(featured_plugin_ids) = self.cached_featured_plugin_ids(&cache_key) {
            return Ok(featured_plugin_ids);
        }
        let featured_plugin_ids = super::remote_legacy::fetch_remote_featured_plugin_ids(
            &config.remote_plugin_service_config(),
            self.restriction_product,
        )
        .await?;
        self.write_featured_plugin_ids_cache(cache_key, &featured_plugin_ids);
        Ok(featured_plugin_ids)
    }

    pub fn maybe_start_curated_repo_sync_for_config(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        if config.plugins_enabled
            && MarketplacePolicy::from_requirements(config.config_layer_stack.requirements())
                .validate_git_source(OPENAI_PLUGINS_GIT_URL, /*ref_name*/ None)
                .is_ok()
        {
            self.start_curated_repo_sync(
                config.http_client_factory.clone(),
                on_effective_plugins_changed,
            );
        }
    }

    fn restriction_product_matches(&self, products: Option<&[Product]>) -> bool {
        match products {
            None => true,
            Some([]) => false,
            Some(products) => self
                .restriction_product
                .is_some_and(|product| product.matches_product_restriction(products)),
        }
    }

    pub fn plugin_skill_snapshots_for_config(
        &self,
        config: &PluginsConfigInput,
    ) -> Option<SkillRootSnapshots<PluginSkillRoot>> {
        if !config.plugins_enabled {
            return None;
        }
        let key = PluginLoadCacheKey::from_config(config, self.codex_home.as_path());
        LoadedPluginsCache::get(&self.loaded_plugins_cache, &key, &self.store)
            .map(|cached| cached.plugin_skill_snapshots.clone())
    }

    #[allow(clippy::unused_self, reason = "Retain the source method shape.")]
    fn resolve_loaded_plugins(&self, mut plugins: Vec<LoadedPlugin>) -> PluginLoadOutcome {
        let target_curated_marketplace = TargetCuratedMarketplace::OpenAiApi;
        plugins.retain(|plugin| {
            plugin_is_eligible_for_target_marketplace(
                &plugin.config_name,
                target_curated_marketplace,
            )
        });
        PluginLoadOutcome::from_plugins(plugins)
    }

    fn clear_caches_after_marketplace_source_refresh(
        &self,
        installed_plugin_cache_refreshed: bool,
        on_effective_plugins_changed: Option<&EffectivePluginsChangedCallback>,
    ) {
        if installed_plugin_cache_refreshed {
            self.clear_cache();
            if let Some(on_effective_plugins_changed) = on_effective_plugins_changed {
                on_effective_plugins_changed(EffectivePluginsChange::default());
            }
        }
    }

    pub async fn plugin_hooks_for_layer_stack(
        &self,
        config_layer_stack: &ConfigLayerStack,
        config: &PluginsConfigInput,
    ) -> PluginHookLoadOutcome {
        if !config.plugins_enabled {
            return PluginHookLoadOutcome::default();
        }
        let target_curated_marketplace = TargetCuratedMarketplace::OpenAiApi;
        load_plugin_hooks_from_layer_stack(
            config_layer_stack,
            &config.plugins,
            &self.store,
            target_curated_marketplace,
        )
        .await
    }

    fn cached_loaded_plugins(&self, key: &PluginLoadCacheKey) -> Option<Vec<LoadedPlugin>> {
        LoadedPluginsCache::get(&self.loaded_plugins_cache, key, &self.store)
            .map(|cached| cached.plugins.clone())
    }

    fn loaded_plugins_cache_generation(&self) -> u64 {
        match self.loaded_plugins_cache.lock() {
            Ok(cache) => cache.generation,
            Err(err) => err.into_inner().generation,
        }
    }

    fn cache_loaded_plugins_if_current(
        &self,
        generation: u64,
        key: PluginLoadCacheKey,
        plugins: Vec<LoadedPlugin>,
        plugin_skill_snapshots: SkillRootSnapshots<PluginSkillRoot>,
    ) {
        let mut cache = match self.loaded_plugins_cache.lock() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        if cache.generation != generation {
            return;
        }
        cache.entries.retain(|entry| entry.key != key);
        cache.entries.push_front(Arc::new(LoadedPluginsCacheEntry {
            key,
            plugins,
            plugin_skill_snapshots,
        }));
        cache.entries.truncate(LOADED_PLUGINS_CACHE_CAPACITY);
        drop(cache);
    }

    fn resolve_installable_plugin(
        &self,
        config_layer_stack: &ConfigLayerStack,
        request: &PluginInstallRequest,
    ) -> Result<ResolvedMarketplacePlugin, PluginInstallError> {
        let resolved = match find_installable_marketplace_plugin(
            &request.marketplace_path,
            &request.plugin_name,
            self.restriction_product,
        ) {
            Ok(resolved) => resolved,
            Err(err) => {
                return Err(err.into());
            }
        };
        if let Err(message) =
            MarketplacePolicy::from_requirements(config_layer_stack.requirements())
                .validate_install(
                    config_layer_stack,
                    self.codex_home.as_path(),
                    &request.marketplace_path,
                    &resolved.plugin_id.marketplace_name,
                )
        {
            let err = MarketplaceError::InvalidMarketplaceFile {
                path: request.marketplace_path.to_path_buf(),
                message,
            };
            return Err(err.into());
        }
        Ok(resolved)
    }

    async fn install_resolved_plugin(
        &self,
        resolved: ResolvedMarketplacePlugin,
    ) -> Result<PluginInstallOutcome, PluginInstallError> {
        let auth_policy = resolved.policy.authentication;
        let plugin_version =
            if is_openai_curated_marketplace_name(&resolved.plugin_id.marketplace_name) {
                let curated_plugin_version = read_curated_plugins_sha(self.codex_home.as_path())
                    .ok_or_else(|| {
                        PluginStoreError::Invalid(
                            "local curated marketplace sha is not available".to_string(),
                        )
                    })?;
                Some(curated_plugin_cache_version(&curated_plugin_version))
            } else {
                None
            };
        let store = self.store.clone();
        let codex_home = self.codex_home.clone();
        let manifest_fallback_contents = resolved
            .manifest_fallback
            .contents_if_has_metadata()
            .map(str::to_string);
        let result: StorePluginInstallResult = tokio::task::spawn_blocking(move || {
            let materialized =
                materialize_marketplace_plugin_source(codex_home.as_path(), &resolved.source)
                    .map_err(PluginStoreError::Invalid)?;
            let source_path = materialized.path;
            match (plugin_version, manifest_fallback_contents.as_deref()) {
                (Some(plugin_version), Some(manifest_contents)) => store
                    .install_with_version_and_fallback_manifest(
                        source_path,
                        resolved.plugin_id,
                        plugin_version,
                        manifest_contents,
                    ),
                (Some(plugin_version), None) => {
                    store.install_with_version(source_path, resolved.plugin_id, plugin_version)
                }
                (None, Some(manifest_contents)) => store.install_with_fallback_manifest(
                    source_path,
                    resolved.plugin_id,
                    manifest_contents,
                ),
                (None, None) => store.install(source_path, resolved.plugin_id),
            }
        })
        .await
        .map_err(PluginInstallError::join)??;

        set_user_plugin_enabled(
            &self.codex_home,
            result.plugin_id.as_key(),
            /*enabled*/ true,
        )
        .await
        .map_err(anyhow::Error::from)?;

        Ok(PluginInstallOutcome {
            plugin_id: result.plugin_id,
            plugin_version: result.plugin_version,
            installed_path: result.installed_path,
            auth_policy,
        })
    }

    pub async fn uninstall_plugin(&self, plugin_id: String) -> Result<(), PluginUninstallError> {
        let plugin_id = PluginId::parse(&plugin_id)?;
        self.uninstall_plugin_id(plugin_id).await
    }

    async fn uninstall_plugin_id(&self, plugin_id: PluginId) -> Result<(), PluginUninstallError> {
        let store = self.store.clone();
        let plugin_id_for_store = plugin_id.clone();
        tokio::task::spawn_blocking(move || store.uninstall(&plugin_id_for_store))
            .await
            .map_err(PluginUninstallError::join)??;

        clear_user_plugin(&self.codex_home, plugin_id.as_key())
            .await
            .map_err(anyhow::Error::from)?;

        Ok(())
    }

    pub fn list_marketplaces_for_context(
        &self,
        context: &PluginMarketplaceContext,
        include_openai_curated: bool,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        context.list_marketplaces(self, include_openai_curated)
    }

    pub fn list_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Ok(ConfiguredMarketplaceListOutcome::default());
        }

        let plugin_states = self.configured_plugin_states(config);
        self.list_marketplaces_for_config_with_states(
            config,
            additional_roots,
            include_openai_curated,
            &plugin_states,
        )
    }

    fn list_marketplaces_for_config_with_states(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
        plugin_states: &ConfiguredPluginStates,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        let marketplace_roots =
            self.marketplace_roots(config, additional_roots, include_openai_curated);
        let marketplace_outcome = self.list_marketplaces_with_policy(config, &marketplace_roots)?;
        let mut seen_plugin_keys = HashSet::new();
        let marketplaces = marketplace_outcome
            .marketplaces
            .into_iter()
            .filter_map(|marketplace| {
                let marketplace_name = marketplace.name.clone();
                let plugins = marketplace
                    .plugins
                    .into_iter()
                    .filter_map(|plugin| {
                        let plugin_key = format!("{}@{marketplace_name}", plugin.name);
                        if !seen_plugin_keys.insert(plugin_key.clone()) {
                            return None;
                        }
                        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
                            return None;
                        }
                        let plugin_id =
                            PluginId::new(plugin.name.clone(), marketplace_name.clone()).ok();
                        let installed = plugin_states.installed.contains(&plugin_key);
                        let installed_version = installed.then_some(()).and_then(|()| {
                            plugin_id
                                .as_ref()
                                .and_then(|plugin_id| self.store.active_plugin_version(plugin_id))
                        });
                        let enabled = plugin_states.enabled.contains(&plugin_key);
                        let mut interface = plugin.interface;
                        let mut local_version = plugin.local_version;
                        let manifest_fallback = plugin.manifest_fallback.clone();
                        if installed
                            && plugin.source.is_install_materialized()
                            && let Some(plugin_id) = plugin_id.as_ref()
                            && let Some(plugin_root) = self.store.active_plugin_root(plugin_id)
                            && let Some(manifest) = self
                                .store
                                .manifest_cache
                                .load(plugin_root.as_path())
                                .map(|loaded| loaded.manifest)
                        {
                            local_version.clone_from(&manifest.version);
                            let marketplace_category = interface
                                .as_ref()
                                .and_then(|interface| interface.category.clone());
                            interface = plugin_interface_with_marketplace_category(
                                manifest.interface,
                                marketplace_category,
                            );
                        }

                        Some(ConfiguredMarketplacePlugin {
                            // Enabled state is keyed by `<plugin>@<marketplace>`, so duplicate
                            // plugin entries from duplicate marketplace files intentionally
                            // resolve to the first discovered source.
                            id: plugin_key,
                            installed_version,
                            installed,
                            enabled,
                            name: plugin.name,
                            local_version,
                            source: plugin.source,
                            policy: plugin.policy,
                            keywords: plugin.keywords,
                            interface,
                            manifest_fallback,
                        })
                    })
                    .collect::<Vec<_>>();

                (!plugins.is_empty()).then_some(ConfiguredMarketplace {
                    name: marketplace.name,
                    path: marketplace.path,
                    interface: marketplace.interface,
                    plugins,
                })
            })
            .collect();

        Ok(ConfiguredMarketplaceListOutcome {
            marketplaces,
            errors: marketplace_outcome.errors,
        })
    }

    pub fn discover_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Ok(MarketplaceListOutcome::default());
        }

        let marketplace_roots = self.marketplace_roots(
            config,
            additional_roots,
            /*include_openai_curated*/ true,
        );
        self.list_marketplaces_with_policy(config, &marketplace_roots)
    }

    pub async fn read_plugin_for_config(
        &self,
        config: &PluginsConfigInput,
        request: &PluginReadRequest,
    ) -> Result<PluginReadOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Err(MarketplaceError::PluginsDisabled);
        }

        let plugin = find_marketplace_plugin(&request.marketplace_path, &request.plugin_name)?;
        MarketplacePolicy::from_requirements(config.config_layer_stack.requirements())
            .validate_install(
                &config.config_layer_stack,
                self.codex_home.as_path(),
                &request.marketplace_path,
                &plugin.plugin_id.marketplace_name,
            )
            .map_err(|message| MarketplaceError::InvalidMarketplaceFile {
                path: request.marketplace_path.to_path_buf(),
                message,
            })?;
        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
            return Err(MarketplaceError::PluginNotFound {
                plugin_name: plugin.plugin_id.plugin_name,
                marketplace_name: plugin.plugin_id.marketplace_name,
            });
        }

        let marketplace_name = plugin.plugin_id.marketplace_name.clone();
        let plugin_key = plugin.plugin_id.as_key();
        let manifest_fallback = plugin
            .manifest_fallback
            .contents_if_has_metadata()
            .map(|_| plugin.manifest_fallback.clone());
        let plugin_states = self.configured_plugin_states(config);
        let installed = plugin_states.installed.contains(&plugin_key);
        let installed_version = if installed {
            self.store.active_plugin_version(&plugin.plugin_id)
        } else {
            None
        };
        let plugin = self
            .read_plugin_detail_for_marketplace_plugin(
                config,
                &marketplace_name,
                ConfiguredMarketplacePlugin {
                    id: plugin_key.clone(),
                    name: plugin.plugin_id.plugin_name,
                    local_version: plugin
                        .manifest
                        .as_ref()
                        .and_then(|manifest| manifest.version.clone()),
                    installed_version,
                    source: plugin.source,
                    policy: plugin.policy,
                    interface: plugin.interface,
                    keywords: plugin
                        .manifest
                        .as_ref()
                        .map(|manifest| manifest.keywords.clone())
                        .unwrap_or_default(),
                    manifest_fallback,
                    installed,
                    enabled: plugin_states.enabled.contains(&plugin_key),
                },
            )
            .await?;

        Ok(PluginReadOutcome {
            marketplace_name,
            marketplace_path: Some(request.marketplace_path.clone()),
            plugin,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Keep the source function whole; splitting would obscure the port."
    )]
    pub async fn read_plugin_detail_for_marketplace_plugin(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: &str,
        plugin: ConfiguredMarketplacePlugin,
    ) -> Result<PluginDetail, MarketplaceError> {
        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
            return Err(MarketplaceError::PluginNotFound {
                plugin_name: plugin.name,
                marketplace_name: marketplace_name.to_string(),
            });
        }

        let plugin_id =
            PluginId::new(plugin.name.clone(), marketplace_name.to_string()).map_err(|err| {
                match err {
                    PluginIdError::Invalid(message) => MarketplaceError::InvalidPlugin(message),
                }
            })?;
        let plugin_key = plugin_id.as_key();
        if plugin.source.is_install_materialized() && !plugin.installed {
            let description = remote_plugin_install_required_description(&plugin.source);
            return Ok(PluginDetail {
                id: plugin_key,
                name: plugin.name,
                local_version: None,
                description: Some(description),
                source: plugin.source,
                policy: plugin.policy,
                interface: plugin.interface,
                keywords: plugin.keywords,
                installed: plugin.installed,
                enabled: plugin.enabled,
                skills: Vec::new(),
                disabled_skill_paths: HashSet::new(),
                onboarding_skill: None,
                hooks: Vec::new(),
                apps: Vec::new(),
                app_category_by_id: HashMap::new(),
                mcp_server_names: Vec::new(),
                details_unavailable_reason: Some(
                    PluginDetailsUnavailableReason::InstallRequiredForRemoteSource,
                ),
            });
        }

        let source_path = if plugin.source.is_install_materialized() && plugin.installed {
            self.store.active_plugin_root(&plugin_id).ok_or_else(|| {
                MarketplaceError::InvalidPlugin(format!(
                    "installed plugin cache entry is missing for {plugin_key}"
                ))
            })?
        } else {
            let codex_home = self.codex_home.clone();
            let source = plugin.source.clone();
            let materialized = tokio::task::spawn_blocking(move || {
                materialize_marketplace_plugin_source(codex_home.as_path(), &source)
            })
            .await
            .map_err(|err| {
                MarketplaceError::InvalidPlugin(format!(
                    "failed to materialize plugin source: {err}"
                ))
            })?
            .map_err(MarketplaceError::InvalidPlugin)?;
            materialized.path.clone()
        };
        if !source_path.as_path().is_dir() {
            return Err(MarketplaceError::InvalidPlugin(
                "path does not exist or is not a directory".to_string(),
            ));
        }
        let loaded_manifest = if crate::plugins::discovery::find_plugin_manifest_path(
            source_path.as_path(),
        )
        .is_some()
        {
            self.store.manifest_cache.load(source_path.as_path())
        } else {
            plugin
                .manifest_fallback
                .as_ref()
                .and_then(|fallback| fallback.parse_for_plugin_root(source_path.as_path()))
                .map(|manifest| crate::plugins::manifest::LoadedPluginManifest {
                    manifest,
                    format: PluginManifestFormat::Legacy,
                })
        }
        .ok_or_else(|| {
            MarketplaceError::InvalidPlugin("missing or invalid plugin.json".to_string())
        })?;
        let manifest_format = loaded_manifest.format;
        let manifest = loaded_manifest.manifest;
        let description = manifest.description.clone();
        let marketplace_category = plugin
            .interface
            .as_ref()
            .and_then(|interface| interface.category.clone());
        let interface = plugin_interface_with_marketplace_category(
            manifest.interface.clone(),
            marketplace_category,
        );
        let plugin_identity = PluginIdentity {
            plugin_id: plugin_id.as_key(),
        };
        let skill_config_rules = skill_config_rules_from_stack(&config.config_layer_stack);
        let resolved_skills = load_plugin_skill_inventory(
            &source_path,
            &plugin_identity,
            &manifest,
            manifest_format,
            self.restriction_product,
            /*plugin_skill_snapshots*/ None,
            self.skill_root_loader.as_ref(),
        )
        .await
        .resolve(&skill_config_rules);
        let onboarding_skill = manifest.paths.onboarding_skill.as_ref().and_then(|path| {
            let plugin_root = source_path.canonicalize().ok()?;
            let path = path.canonicalize().ok()?;
            if !path.as_path().starts_with(plugin_root.as_path()) {
                return None;
            }
            let path_uri = PathUri::from_abs_path(&path);
            resolved_skills
                .skills
                .iter()
                .any(|skill| skill.path_to_skills_md == path_uri)
                .then_some(path)
        });
        let plugin_data_root = self.store.plugin_data_root(&plugin_id);
        let (hook_sources, _hook_load_warnings) = if manifest_format == PluginManifestFormat::Legacy
        {
            load_plugin_hooks(&source_path, &plugin_id, &plugin_data_root, &manifest.paths)
        } else {
            (Vec::new(), Vec::new())
        };
        let hooks = plugin_hook_declarations(&hook_sources)
            .into_iter()
            .map(|hook| PluginHookSummary {
                key: hook.key,
                event_name: hook.event_name,
            })
            .collect();
        let app_declarations = if manifest_format == PluginManifestFormat::Legacy {
            load_plugin_apps_from_manifest(source_path.as_path(), &manifest.paths).await
        } else {
            Vec::new()
        };
        let mcp_data_root = (manifest_format == PluginManifestFormat::AgentPlugin)
            .then(|| self.store.mcp_data_root(&plugin_id, manifest_format));
        let mcp_servers = load_plugin_mcp_servers_from_manifest_with_format(
            source_path.as_path(),
            &manifest.paths,
            /*plugin_policy*/ None,
            mcp_data_root.as_deref(),
            manifest_format,
        )
        .await;
        let apps = app_connector_ids_from_declarations(&app_declarations);
        let mut seen_app_connector_ids = HashSet::new();
        let mut app_category_by_id = HashMap::new();
        for app in &app_declarations {
            if seen_app_connector_ids.insert(app.connector_id.0.as_str())
                && let Some(category) = &app.category
            {
                app_category_by_id.insert(app.connector_id.0.clone(), category.clone());
            }
        }
        let mut mcp_server_names = mcp_servers.into_keys().collect::<Vec<_>>();
        mcp_server_names.sort_unstable();
        mcp_server_names.dedup();

        Ok(PluginDetail {
            id: plugin.id,
            name: plugin.name,
            local_version: manifest.version.clone(),
            description,
            source: plugin.source,
            policy: plugin.policy,
            interface,
            keywords: manifest.keywords,
            installed: plugin.installed,
            enabled: plugin.enabled,
            skills: resolved_skills.skills,
            disabled_skill_paths: resolved_skills.disabled_skill_paths,
            onboarding_skill,
            hooks,
            apps,
            app_category_by_id,
            mcp_server_names,
            details_unavailable_reason: None,
        })
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "Retain the source signature; callers hand over ownership."
    )]
    pub fn maybe_start_plugin_startup_tasks_for_config(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        reload_config: ConfigLayerReload,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        if config.plugins_enabled {
            self.maybe_start_curated_repo_sync_for_config(
                config,
                on_effective_plugins_changed.clone(),
            );
            let should_spawn_marketplace_auto_upgrade = {
                let mut state = match self.configured_marketplace_upgrade_state.write() {
                    Ok(state) => state,
                    Err(err) => err.into_inner(),
                };
                if state.in_flight {
                    false
                } else {
                    state.in_flight = true;
                    true
                }
            };
            if should_spawn_marketplace_auto_upgrade {
                let manager = Arc::clone(self);
                let config = config.clone();
                let on_effective_plugins_changed = on_effective_plugins_changed.clone();
                let runtime = tokio::runtime::Handle::current();
                if let Err(err) = std::thread::Builder::new()
                    .name("plugins-marketplace-auto-upgrade".to_string())
                    .spawn(move || {
                        let outcome = manager.upgrade_configured_marketplaces_for_config_with_mode(
                            &config,
                            /*marketplace_name*/ None,
                            PluginGitMode::Automatic,
                            &reload_config,
                        );
                        match outcome {
                            Ok(outcome) => {
                                if !outcome.upgraded_roots.is_empty()
                                    && let Some(on_effective_plugins_changed) =
                                        on_effective_plugins_changed
                                {
                                    runtime.spawn(async move {
                                        on_effective_plugins_changed(
                                            EffectivePluginsChange::default(),
                                        );
                                    });
                                }
                                for error in outcome.errors {
                                    warn!(
                                        marketplace = error.marketplace_name,
                                        error = %error.message,
                                        "failed to auto-upgrade configured marketplace"
                                    );
                                }
                            }
                            Err(err) => {
                                warn!("failed to auto-upgrade configured marketplaces: {err}");
                            }
                        }

                        let mut state = match manager.configured_marketplace_upgrade_state.write() {
                            Ok(state) => state,
                            Err(err) => err.into_inner(),
                        };
                        state.in_flight = false;
                    })
                {
                    let mut state = match self.configured_marketplace_upgrade_state.write() {
                        Ok(state) => state,
                        Err(err) => err.into_inner(),
                    };
                    state.in_flight = false;
                    warn!("failed to start configured marketplace auto-upgrade task: {err}");
                }
            }
        }
    }

    pub fn upgrade_configured_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: Option<&str>,
        reload_config: &ConfigLayerReload,
    ) -> Result<ConfiguredMarketplaceUpgradeOutcome, String> {
        self.upgrade_configured_marketplaces_for_config_with_mode(
            config,
            marketplace_name,
            PluginGitMode::Manual,
            reload_config,
        )
    }

    fn upgrade_configured_marketplaces_for_config_with_mode(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: Option<&str>,
        mode: PluginGitMode,
        reload_config: &ConfigLayerReload,
    ) -> Result<ConfiguredMarketplaceUpgradeOutcome, String> {
        let mut outcome = upgrade_configured_git_marketplaces_with_mode(
            self.codex_home.as_path(),
            &config.config_layer_stack,
            marketplace_name,
            mode,
            reload_config,
        );
        if let Some(marketplace_name) = marketplace_name
            && outcome.selected_marketplaces.is_empty()
        {
            return Err(format!(
                "marketplace `{marketplace_name}` is not configured as a Git marketplace"
            ));
        }
        if !outcome.upgraded_roots.is_empty() {
            let mut configured_plugin_keys = configured_plugins_from_stack(
                &config.config_layer_stack,
                self.codex_home.as_path(),
            )
            .into_keys()
            .collect::<Vec<_>>();
            configured_plugin_keys.sort_unstable();
            match refresh_non_curated_plugin_cache_force_reinstall_detailed(
                self.codex_home.as_path(),
                &outcome.upgraded_roots,
                &configured_plugin_keys,
                mode,
            ) {
                Ok(refresh_outcome) => {
                    self.clear_caches_after_marketplace_source_refresh(
                        refresh_outcome.cache_refreshed,
                        /*on_effective_plugins_changed*/ None,
                    );
                    outcome
                        .errors
                        .extend(refresh_outcome.errors.into_iter().map(|error| {
                            ConfiguredMarketplaceUpgradeError {
                                marketplace_name: error.marketplace_name,
                                message: error.message,
                            }
                        }));
                }
                Err(err) => {
                    self.clear_cache();
                    outcome.errors.push(ConfiguredMarketplaceUpgradeError {
                        marketplace_name: marketplace_name
                            .unwrap_or("all configured marketplaces")
                            .to_string(),
                        message: format!(
                            "failed to refresh installed plugin cache after marketplace upgrade: {err}"
                        ),
                    });
                }
            }
        }
        Ok(outcome)
    }

    pub fn maybe_start_non_curated_plugin_cache_refresh(
        self: &Arc<Self>,
        context: &PluginMarketplaceContext,
        marketplaces: &[ConfiguredMarketplace],
    ) {
        if let Some(request) = context.non_curated_cache_refresh_request(
            self,
            marketplaces,
            NonCuratedCacheRefreshMode::IfVersionChanged,
            PluginGitMode::Automatic,
        ) {
            self.schedule_non_curated_plugin_cache_refresh(request);
        }
    }

    pub async fn refresh_non_curated_plugin_cache_for_context(
        self: &Arc<Self>,
        context: &PluginMarketplaceContext,
    ) -> bool {
        let Ok(_refresh_permit) = self.non_curated_cache_refresh_lock.acquire().await else {
            return false;
        };
        let mut completion = self.non_curated_cache_refresh_completion.subscribe();
        let changed_sequence = completion.borrow_and_update().changed_sequence;
        let marketplaces =
            match self.list_marketplaces_for_context(context, /*include_openai_curated*/ false) {
                Ok(outcome) => outcome.marketplaces,
                Err(err) => {
                    warn!("failed to prepare non-curated plugin cache refresh: {err}");
                    Vec::new()
                }
            };
        if let Some(request) = context.non_curated_cache_refresh_request(
            self,
            &marketplaces,
            NonCuratedCacheRefreshMode::IfVersionChanged,
            PluginGitMode::Manual,
        ) {
            self.schedule_non_curated_plugin_cache_refresh(request);
        }

        loop {
            let in_flight = match self.non_curated_cache_refresh_state.read() {
                Ok(state) => state.in_flight,
                Err(err) => err.into_inner().in_flight,
            };
            if !in_flight {
                return completion.borrow().changed_sequence != changed_sequence;
            }
            if completion.changed().await.is_err() {
                return false;
            }
        }
    }

    fn schedule_non_curated_plugin_cache_refresh(
        self: &Arc<Self>,
        mut request: NonCuratedCacheRefreshRequest,
    ) {
        let should_spawn = {
            let mut state = match self.non_curated_cache_refresh_state.write() {
                Ok(state) => state,
                Err(err) => err.into_inner(),
            };
            if request.mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && state.last_refreshed.as_ref().is_some_and(|last_refreshed| {
                    request.configured_plugin_sources.iter().any(|source| {
                        last_refreshed
                            .configured_plugin_sources
                            .iter()
                            .any(|previous_source| {
                                previous_source.plugin_key == source.plugin_key
                                    && previous_source.local_version == source.local_version
                                    && (previous_source.marketplace_path != source.marketplace_path
                                        || previous_source.source != source.source)
                            })
                    })
                })
            {
                request.mode = NonCuratedCacheRefreshMode::ForceReinstall;
            }
            if request.mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && state.requested.as_ref().is_some_and(|requested| {
                    requested.mode == NonCuratedCacheRefreshMode::ForceReinstall
                        && requested.roots == request.roots
                })
            {
                request.mode = NonCuratedCacheRefreshMode::ForceReinstall;
            }
            // Reconcile each canonical plugin generation once before publishing its resource.
            if state.requested.as_ref().is_some_and(|requested| {
                requested == &request
                    || (requested.git_mode == PluginGitMode::Manual
                        && request.git_mode == PluginGitMode::Automatic
                        && requested.roots == request.roots
                        && requested.configured_plugin_keys == request.configured_plugin_keys
                        && requested.configured_plugin_sources == request.configured_plugin_sources
                        && requested.mode == request.mode)
            }) || (request.mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && !state.in_flight
                && state.last_refreshed.as_ref().is_some_and(|last_refreshed| {
                    last_refreshed.roots == request.roots
                        && last_refreshed.configured_plugin_keys == request.configured_plugin_keys
                        && last_refreshed.configured_plugin_sources
                            == request.configured_plugin_sources
                        && (last_refreshed.git_mode == request.git_mode
                            || last_refreshed.git_mode == PluginGitMode::Manual)
                }))
            {
                return;
            }
            state.requested = Some(request);
            if state.in_flight {
                false
            } else {
                state.in_flight = true;
                true
            }
        };
        if !should_spawn {
            return;
        }

        let manager = Arc::clone(self);
        if let Err(err) = std::thread::Builder::new()
            .name("plugins-non-curated-cache-refresh".to_string())
            .spawn(move || manager.run_non_curated_plugin_cache_refresh_loop())
        {
            let mut state = match self.non_curated_cache_refresh_state.write() {
                Ok(state) => state,
                Err(err) => err.into_inner(),
            };
            state.in_flight = false;
            state.requested = None;
            self.non_curated_cache_refresh_completion
                .send_modify(|completion| {
                    completion.sequence = completion.sequence.wrapping_add(1);
                });
            warn!("failed to start non-curated plugin cache refresh task: {err}");
        }
    }

    fn start_curated_repo_sync(
        self: &Arc<Self>,
        http_client_factory: HttpClientFactory,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        if CURATED_REPO_SYNC_STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let on_effective_plugins_changed =
            on_effective_plugins_changed.map(|on_effective_plugins_changed| {
                let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                    return on_effective_plugins_changed;
                };
                let callback: EffectivePluginsChangedCallback = Arc::new(move |change| {
                    let on_effective_plugins_changed = Arc::clone(&on_effective_plugins_changed);
                    runtime.spawn(async move {
                        on_effective_plugins_changed(change);
                    });
                });
                callback
            });
        let manager = Arc::clone(self);
        let codex_home = self.codex_home.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("plugins-curated-repo-sync".to_string())
            .spawn(move || {
                match sync_openai_plugins_repo(codex_home.as_path(), http_client_factory) {
                    Ok(curated_plugin_version) => {
                        let configured_curated_plugin_ids =
                            configured_curated_plugin_ids_from_codex_home(codex_home.as_path());
                        match refresh_curated_plugin_cache(
                            codex_home.as_path(),
                            &curated_plugin_version,
                            &configured_curated_plugin_ids,
                        ) {
                            Ok(cache_refreshed) => {
                                manager.clear_caches_after_marketplace_source_refresh(
                                    cache_refreshed,
                                    on_effective_plugins_changed.as_ref(),
                                );
                            }
                            Err(err) => {
                                manager.clear_cache();
                                CURATED_REPO_SYNC_STARTED.store(false, Ordering::SeqCst);
                                warn!("failed to refresh curated plugin cache after sync: {err}");
                            }
                        }
                    }
                    Err(err) => {
                        CURATED_REPO_SYNC_STARTED.store(false, Ordering::SeqCst);
                        warn!("failed to sync curated plugins repo: {err}");
                    }
                }
            })
        {
            CURATED_REPO_SYNC_STARTED.store(false, Ordering::SeqCst);
            warn!("failed to start curated plugins repo sync task: {err}");
        }
    }

    fn run_non_curated_plugin_cache_refresh_loop(self: Arc<Self>) {
        loop {
            let request = {
                let state = match self.non_curated_cache_refresh_state.read() {
                    Ok(state) => state,
                    Err(err) => err.into_inner(),
                };
                state.requested.clone()
            };

            let Some(request) = request else {
                let mut state = match self.non_curated_cache_refresh_state.write() {
                    Ok(state) => state,
                    Err(err) => err.into_inner(),
                };
                state.in_flight = false;
                self.non_curated_cache_refresh_completion
                    .send_modify(|completion| {
                        completion.sequence = completion.sequence.wrapping_add(1);
                    });
                return;
            };

            let refresh_result = match request.mode {
                NonCuratedCacheRefreshMode::IfVersionChanged => {
                    refresh_non_curated_plugin_cache_detailed(
                        self.codex_home.as_path(),
                        &request.roots,
                        &request.configured_plugin_keys,
                        request.git_mode,
                    )
                }
                NonCuratedCacheRefreshMode::ForceReinstall => {
                    refresh_non_curated_plugin_cache_force_reinstall_detailed(
                        self.codex_home.as_path(),
                        &request.roots,
                        &request.configured_plugin_keys,
                        request.git_mode,
                    )
                }
            };
            let (refreshed, cache_changed) = match refresh_result {
                Ok(refresh_outcome) => {
                    if refresh_outcome.cache_refreshed {
                        self.clear_cache();
                    }
                    for error in &refresh_outcome.errors {
                        warn!(
                            marketplace = error.marketplace_name,
                            error = %error.message,
                            "failed to refresh configured plugin cache"
                        );
                    }
                    (
                        refresh_outcome.errors.is_empty(),
                        refresh_outcome.cache_refreshed,
                    )
                }
                Err(err) => {
                    self.clear_cache();
                    warn!("failed to refresh non-curated plugin cache: {err}");
                    (false, false)
                }
            };

            let mut state = match self.non_curated_cache_refresh_state.write() {
                Ok(state) => state,
                Err(err) => err.into_inner(),
            };
            if refreshed {
                state.last_refreshed = Some(request.clone());
            }
            let complete = state.requested.as_ref() == Some(&request);
            if complete {
                state.requested = None;
                state.in_flight = false;
            }
            self.non_curated_cache_refresh_completion
                .send_modify(|completion| {
                    completion.sequence = completion.sequence.wrapping_add(1);
                    if cache_changed {
                        completion.changed_sequence = completion.changed_sequence.wrapping_add(1);
                    }
                });
            if complete {
                return;
            }
        }
    }

    fn configured_plugin_states(&self, config: &PluginsConfigInput) -> ConfiguredPluginStates {
        let configured_plugins =
            configured_plugins_from_stack(&config.config_layer_stack, self.codex_home.as_path());
        let installed = configured_plugins
            .keys()
            .filter(|plugin_key| {
                PluginId::parse(plugin_key)
                    .ok()
                    .is_some_and(|plugin_id| self.store.is_installed(&plugin_id))
            })
            .cloned()
            .collect::<HashSet<_>>();
        let enabled = configured_plugins
            .into_iter()
            .filter_map(|(plugin_key, plugin)| {
                (plugin.enabled.unwrap_or(true) && config.plugins.allows_plugin(&plugin_key))
                    .then_some(plugin_key)
            })
            .collect::<HashSet<_>>();
        ConfiguredPluginStates { installed, enabled }
    }

    fn marketplace_roots(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
    ) -> Vec<AbsolutePathBuf> {
        // Treat the curated catalog as an extra marketplace root so plugin listing can surface it
        // without requiring every caller to know where it is stored.
        let mut roots = additional_roots.to_vec();
        roots.extend(installed_marketplace_roots_from_layer_stack(
            &config.config_layer_stack,
            self.codex_home.as_path(),
        ));
        let curated_marketplace_path = if include_openai_curated {
            let api_marketplace_path =
                curated_plugins_api_marketplace_path(self.codex_home.as_path());
            api_marketplace_path
                .is_file()
                .then_some(api_marketplace_path)
        } else {
            None
        };
        if let Some(curated_marketplace_path) = curated_marketplace_path
            && let Ok(curated_marketplace_path) =
                AbsolutePathBuf::try_from(curated_marketplace_path)
        {
            roots.push(curated_marketplace_path);
        }
        roots.sort_unstable();
        roots.dedup();
        roots
    }

    fn list_marketplaces_with_policy(
        &self,
        config: &PluginsConfigInput,
        roots: &[AbsolutePathBuf],
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        let mut outcome =
            list_marketplaces_with_cache(roots, home_dir().as_deref(), &self.store.manifest_cache)?;
        let policy = MarketplacePolicy::from_requirements(config.config_layer_stack.requirements());
        outcome.marketplaces.retain(|marketplace| {
            policy
                .validate_install(
                    &config.config_layer_stack,
                    self.codex_home.as_path(),
                    &marketplace.path,
                    &marketplace.name,
                )
                .is_ok()
        });
        Ok(outcome)
    }

    pub fn clear_cache(&self) {
        self.clear_loaded_plugins_cache();
    }
    fn clear_loaded_plugins_cache(&self) {
        let mut cache = self
            .loaded_plugins_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.generation = cache.generation.wrapping_add(1);
        cache.entries.clear();
    }
    pub async fn install_plugin(
        &self,
        stack: &ConfigLayerStack,
        request: PluginInstallRequest,
    ) -> Result<PluginInstallOutcome, PluginInstallError> {
        let resolved = self.resolve_installable_plugin(stack, &request)?;
        self.install_resolved_plugin(resolved).await
    }
    pub async fn plugins_for_config(&self, config: &PluginsConfigInput) -> PluginLoadOutcome {
        if !config.plugins_enabled {
            return PluginLoadOutcome::default();
        }
        let key = PluginLoadCacheKey::from_config(config, self.codex_home.as_path());
        if let Some(plugins) = self.cached_loaded_plugins(&key) {
            return self.resolve_loaded_plugins(plugins);
        }
        let Ok(_load_permit) = self.loaded_plugins_load_semaphore.acquire().await else {
            warn!("plugin load semaphore closed");
            return PluginLoadOutcome::default();
        };
        if let Some(plugins) = self.cached_loaded_plugins(&key) {
            return self.resolve_loaded_plugins(plugins);
        }
        let generation = self.loaded_plugins_cache_generation();
        let snapshots = new_plugin_skill_snapshots();
        let plugins = load_plugins_from_layer_stack(
            &config.config_layer_stack,
            &config.plugins,
            &self.store,
            Some(&snapshots),
            self.restriction_product,
            self.skill_root_loader.as_ref(),
        )
        .await;
        log_plugin_load_errors(&plugins);
        self.cache_loaded_plugins_if_current(generation, key, plugins.clone(), snapshots);
        self.resolve_loaded_plugins(plugins)
    }
}
pub(crate) fn remote_plugin_install_required_description(
    source: &MarketplacePluginSource,
) -> String {
    let source_description = match source {
        MarketplacePluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        } => {
            let mut parts = vec![url.clone()];
            if let Some(path) = path {
                parts.push(format!("path `{path}`"));
            }
            if let Some(ref_name) = ref_name {
                parts.push(format!("ref `{ref_name}`"));
            }
            if let Some(sha) = sha {
                parts.push(format!("sha `{sha}`"));
            }
            parts.join(", ")
        }
        MarketplacePluginSource::Local { path } => path.as_path().display().to_string(),
        MarketplacePluginSource::Npm {
            package,
            version,
            registry,
        } => {
            let mut parts = vec![package.clone()];
            if let Some(version) = version {
                parts.push(format!("version `{version}`"));
            }
            if let Some(registry) = registry {
                parts.push(format!("registry `{registry}`"));
            }
            parts.join(", ")
        }
    };

    let source_kind = if matches!(source, MarketplacePluginSource::Npm { .. }) {
        "an npm plugin"
    } else {
        "a cross-repo plugin"
    };
    format!(
        "This is {source_kind}. Install it to view more detailed information. The source of the plugin is {source_description}."
    )
}

#[derive(Debug, thiserror::Error)]
pub enum PluginInstallError {
    #[error("{0}")]
    Marketplace(#[from] MarketplaceError),

    #[error("{0}")]
    Store(#[from] PluginStoreError),

    #[error("{0}")]
    Config(#[from] anyhow::Error),

    #[error("failed to join plugin install task: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl PluginInstallError {
    fn join(source: tokio::task::JoinError) -> Self {
        Self::Join(source)
    }

    #[must_use]
    pub fn is_invalid_request(&self) -> bool {
        matches!(
            self,
            Self::Marketplace(
                MarketplaceError::MarketplaceNotFound { .. }
                    | MarketplaceError::InvalidMarketplaceFile { .. }
                    | MarketplaceError::PluginNotFound { .. }
                    | MarketplaceError::PluginNotAvailable { .. }
                    | MarketplaceError::InvalidPlugin(_)
            ) | Self::Store(PluginStoreError::Invalid(_))
        )
    }

    #[must_use]
    pub fn sub_error_type(&self) -> Option<String> {
        match self {
            Self::Marketplace(err) => marketplace_error_sub_error_type(err),
            Self::Store(err) => err.sub_error_type(),
            Self::Config(_) => Some("failed_to_enable_plugin".to_string()),
            Self::Join(_) => Some("plugin_install_task_failed".to_string()),
        }
    }
}

fn marketplace_error_sub_error_type(err: &MarketplaceError) -> Option<String> {
    match err {
        MarketplaceError::Io { context, .. } => Some(error_context_sub_error_type(context)),
        MarketplaceError::MarketplaceNotFound { .. }
        | MarketplaceError::InvalidMarketplaceFile { .. }
        | MarketplaceError::PluginNotFound { .. }
        | MarketplaceError::PluginNotAvailable { .. }
        | MarketplaceError::PluginsDisabled
        | MarketplaceError::InvalidPlugin(_) => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginUninstallError {
    #[error("{0}")]
    InvalidPluginId(#[from] PluginIdError),

    #[error("{0}")]
    Store(#[from] PluginStoreError),

    #[error("{0}")]
    Config(#[from] anyhow::Error),

    #[error("failed to join plugin uninstall task: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl PluginUninstallError {
    fn join(source: tokio::task::JoinError) -> Self {
        Self::Join(source)
    }

    #[must_use]
    pub fn is_invalid_request(&self) -> bool {
        matches!(self, Self::InvalidPluginId(_))
    }
}

/// Source cache-time boundary exposed only to integration tests.
#[cfg(feature = "test-api")]
pub mod test_api {
    /// Makes the existing entry expire without changing production TTL or fetch behavior.
    pub fn expire_featured_plugin_ids_cache(manager: &super::PluginsManager) {
        if let Some(entry) = manager
            .featured_plugin_ids_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            entry.expires_at = std::time::Instant::now();
        }
    }
}
