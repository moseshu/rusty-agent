// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Preserve source plugin load outcomes and capability resolution."
)]
use crate::plugins::AppDeclaration;
use crate::plugins::LoadedPlugin;
use crate::plugins::PluginCapabilitySummary;
use crate::plugins::PluginGitMode;
use crate::plugins::PluginHookSource;
use crate::plugins::PluginId;
use crate::plugins::PluginIdError;
use crate::plugins::PluginIdentity;
use crate::plugins::PluginSkillRoot;
use crate::plugins::SkillDiscoveryMode;
use crate::plugins::app_config::parse_plugin_app_config;
use crate::plugins::app_config::parse_plugin_app_config_value;
use crate::plugins::app_connector_ids_from_declarations;
use crate::plugins::discovery::find_plugin_manifest_path;
use crate::plugins::is_openai_curated_marketplace_name;
use crate::plugins::manifest::ManifestCache;
use crate::plugins::manifest::PluginManifest;
use crate::plugins::manifest::PluginManifestFormat;
use crate::plugins::manifest::PluginManifestHooks;
use crate::plugins::manifest::PluginManifestMcpServers;
use crate::plugins::manifest::PluginManifestPaths;
use crate::plugins::manifest::load_plugin_manifest_with_format;
use crate::plugins::marketplace::MarketplacePluginSource;
use crate::plugins::marketplace::find_marketplace_plugin;
use crate::plugins::marketplace::list_marketplaces_with_home;
use crate::plugins::marketplace::load_marketplace;
use crate::plugins::marketplace_policy::configured_plugins_from_stack;
use crate::plugins::migrated_command_skills_root;
use crate::plugins::store::PluginStore;
use crate::plugins::store::plugin_version_for_source;
use crate::plugins::store::plugin_version_for_source_with_fallback_manifest;
use crate::skills::SkillMetadata;
use crate::skills::SkillProduct as Product;
use crate::skills::SkillRootLoadRequest;
use crate::skills::SkillRootLoader;
use crate::skills::SkillRootSnapshots;
use ra_config::ConfigLayerStack;
use ra_config::HooksFile;
use ra_config::SkillConfigRules;
use ra_config::skill_config_rules_from_stack;
use ra_config::types::McpServerConfig;
use ra_config::types::McpServerTransportConfig;
use ra_config::types::PluginConfig;
use ra_config::types::PluginMcpServerConfig;
use ra_config::types::PluginsConfigToml;
use ra_mcp::plugin_config::parse_agent_plugin_mcp_config;
use ra_mcp::plugin_config::parse_plugin_mcp_config;
use ra_paths::AbsolutePathBuf;
use ra_paths::PathUri;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use tracing::instrument;
use tracing::warn;

#[path = "agent_plugin_mcp_overlay.rs"]
mod agent_plugin_mcp_overlay;

const DEFAULT_SKILLS_DIR_NAME: &str = "skills";
const DEFAULT_HOOKS_CONFIG_FILE: &str = "hooks/hooks.json";
const DEFAULT_MCP_CONFIG_FILE: &str = ".mcp.json";
const DEFAULT_APP_CONFIG_FILE: &str = ".app.json";
const CONFIG_TOML_FILE: &str = "config.toml";
const CURATED_PLUGIN_CACHE_VERSION_SHA_PREFIX_LEN: usize = 8;

/// Hook declarations and warnings resolved without loading other plugin capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginHookLoadOutcome {
    pub hook_sources: Vec<PluginHookSource>,
    pub hook_load_warnings: Vec<String>,
}

/// The built-in curated marketplace selection for the current runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetCuratedMarketplace {
    OpenAi,
    OpenAiApi,
}

enum PluginLoadScope<'a> {
    AllCapabilities {
        restriction_product: Option<Product>,
        skill_config_rules: &'a SkillConfigRules,
        plugin_skill_snapshots: Option<&'a SkillRootSnapshots<PluginSkillRoot>>,
        skill_root_loader: &'a dyn SkillRootLoader<PluginSkillRoot>,
    },
    HooksOnly,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NonCuratedCacheRefreshMode {
    IfVersionChanged,
    ForceReinstall,
}

#[derive(Debug)]
pub(crate) struct NonCuratedCacheRefreshOutcome {
    pub(crate) cache_refreshed: bool,
    pub(crate) errors: Vec<NonCuratedCacheRefreshError>,
}

#[derive(Debug)]
pub(crate) struct NonCuratedCacheRefreshError {
    pub(crate) marketplace_name: String,
    pub(crate) message: String,
}

pub(crate) fn log_plugin_load_errors(plugins: &[LoadedPlugin<McpServerConfig>]) {
    for plugin in plugins.iter().filter(|plugin| plugin.error.is_some()) {
        if let Some(error) = plugin.error.as_deref() {
            warn!(
                plugin = plugin.config_name,
                path = %plugin.root.display(),
                "failed to load plugin: {error}"
            );
        }
    }
}

/// Load configured plugins without applying auth-dependent runtime policies.
#[instrument(level = "trace", skip_all)]
// Keep validated policy with the existing snapshot inputs instead of reparsing layers.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn load_plugins_from_layer_stack(
    config_layer_stack: &ConfigLayerStack,
    plugin_policy: &ra_config::types::PluginsConfigToml,
    store: &PluginStore,
    plugin_skill_snapshots: Option<&SkillRootSnapshots<PluginSkillRoot>>,
    restriction_product: Option<Product>,
    skill_root_loader: &dyn SkillRootLoader<PluginSkillRoot>,
) -> Vec<LoadedPlugin<McpServerConfig>> {
    let skill_config_rules = skill_config_rules_from_stack(config_layer_stack);
    load_plugins_from_layer_stack_with_scope(
        config_layer_stack,
        plugin_policy,
        store,
        PluginLoadScope::AllCapabilities {
            restriction_product,
            skill_config_rules: &skill_config_rules,
            plugin_skill_snapshots,
            skill_root_loader,
        },
    )
    .await
}

async fn load_plugins_from_layer_stack_with_scope(
    config_layer_stack: &ConfigLayerStack,
    plugin_policy: &ra_config::types::PluginsConfigToml,
    store: &PluginStore,
    scope: PluginLoadScope<'_>,
) -> Vec<LoadedPlugin<McpServerConfig>> {
    let mut configured_plugins =
        configured_plugins_from_stack(config_layer_stack, store.codex_home().as_path());
    for (plugin_id, plugin) in &mut configured_plugins {
        plugin.enabled =
            Some(plugin.enabled.unwrap_or(true) && plugin_policy.allows_plugin(plugin_id));
    }
    let mut configured_plugins: Vec<_> = configured_plugins.into_iter().collect();
    configured_plugins.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

    let mut plugins = Vec::with_capacity(configured_plugins.len());
    let mut seen_mcp_server_names = HashMap::<String, String>::new();
    for (configured_name, plugin) in configured_plugins {
        let loaded_plugin = load_plugin(configured_name.clone(), &plugin, store, &scope).await;
        for name in loaded_plugin.mcp_servers.keys() {
            if let Some(previous_plugin) =
                seen_mcp_server_names.insert(name.clone(), configured_name.clone())
            {
                warn!(
                    plugin = configured_name,
                    previous_plugin,
                    server = name,
                    "skipping duplicate plugin MCP server name"
                );
            }
        }
        plugins.push(loaded_plugin);
    }

    plugins
}

/// Load hooks from enabled plugins without loading their skills, MCP servers, or apps.
pub async fn load_plugin_hooks_from_layer_stack(
    config_layer_stack: &ConfigLayerStack,
    plugin_policy: &ra_config::types::PluginsConfigToml,
    store: &PluginStore,
    target_curated_marketplace: TargetCuratedMarketplace,
) -> PluginHookLoadOutcome {
    let mut plugins = load_plugins_from_layer_stack_with_scope(
        config_layer_stack,
        plugin_policy,
        store,
        PluginLoadScope::HooksOnly,
    )
    .await;
    plugins.retain(|plugin| {
        plugin_is_eligible_for_target_marketplace(&plugin.config_name, target_curated_marketplace)
    });
    PluginHookLoadOutcome {
        hook_sources: plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_sources.iter().cloned())
            .collect(),
        hook_load_warnings: plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_load_warnings.iter().cloned())
            .collect(),
    }
}

pub(crate) fn plugin_is_eligible_for_target_marketplace(
    plugin_key: &str,
    target_curated_marketplace: TargetCuratedMarketplace,
) -> bool {
    let Ok(plugin_id) = PluginId::parse(plugin_key) else {
        return true;
    };
    match target_curated_marketplace {
        TargetCuratedMarketplace::OpenAi => {
            plugin_id.marketplace_name != crate::plugins::OPENAI_API_CURATED_MARKETPLACE_NAME
        }
        TargetCuratedMarketplace::OpenAiApi => {
            plugin_id.marketplace_name != crate::plugins::OPENAI_CURATED_MARKETPLACE_NAME
        }
    }
}

pub fn refresh_curated_plugin_cache(
    codex_home: &Path,
    plugin_version: &str,
    configured_curated_plugin_ids: &[PluginId],
) -> Result<bool, String> {
    let cache_plugin_version = curated_plugin_cache_version(plugin_version);
    let store = PluginStore::try_new(codex_home.to_path_buf()).map_err(|err| err.to_string())?;
    let curated_marketplace_paths = curated_marketplace_paths_for_cache_refresh(codex_home)?;
    let mut loaded_marketplace_names = HashSet::<String>::new();
    let mut marketplace_plugin_keys = HashSet::<String>::new();
    let mut plugin_sources = HashMap::<String, AbsolutePathBuf>::new();

    for curated_marketplace_path in curated_marketplace_paths {
        let curated_marketplace = load_marketplace(&curated_marketplace_path).map_err(|err| {
            format!("failed to load curated marketplace for cache refresh: {err}")
        })?;
        let marketplace_name = curated_marketplace.name;
        loaded_marketplace_names.insert(marketplace_name.clone());

        for plugin in curated_marketplace.plugins {
            let plugin_id =
                PluginId::new(plugin.name.clone(), marketplace_name.clone()).map_err(|err| {
                    match err {
                        PluginIdError::Invalid(message) => {
                            format!("failed to prepare curated plugin cache refresh: {message}")
                        }
                    }
                })?;
            let plugin_key = plugin_id.as_key();
            marketplace_plugin_keys.insert(plugin_key.clone());
            if plugin_sources.contains_key(&plugin_key) {
                warn!(
                    plugin = %plugin.name,
                    marketplace = %marketplace_name,
                    "ignoring duplicate curated plugin entry during cache refresh"
                );
                continue;
            }
            if let MarketplacePluginSource::Local { path } = plugin.source {
                plugin_sources.insert(plugin_key, path);
            }
        }
    }

    let mut cache_refreshed = false;
    for plugin_id in configured_curated_plugin_ids {
        let plugin_key = plugin_id.as_key();
        if !marketplace_plugin_keys.contains(&plugin_key) {
            if !loaded_marketplace_names.contains(&plugin_id.marketplace_name) {
                continue;
            }
            warn!(
                plugin = %plugin_id.plugin_name,
                marketplace = %plugin_id.marketplace_name,
                "configured curated plugin no longer exists in curated marketplace during cache refresh"
            );
            if store.plugin_base_root(plugin_id).as_path().exists() {
                store.uninstall(plugin_id).map_err(|err| {
                    format!(
                        "failed to remove stale curated plugin cache for {}: {err}",
                        plugin_id.as_key()
                    )
                })?;
                cache_refreshed = true;
            }
            continue;
        }

        let Some(source_path) = plugin_sources.get(&plugin_key).cloned() else {
            continue;
        };

        if store.active_plugin_version(plugin_id).as_deref() == Some(cache_plugin_version.as_str())
        {
            continue;
        }

        store
            .install_with_version(source_path, plugin_id.clone(), cache_plugin_version.clone())
            .map_err(|err| {
                format!(
                    "failed to refresh curated plugin cache for {}: {err}",
                    plugin_id.as_key()
                )
            })?;
        cache_refreshed = true;
    }

    Ok(cache_refreshed)
}

fn curated_marketplace_paths_for_cache_refresh(
    codex_home: &Path,
) -> Result<Vec<AbsolutePathBuf>, String> {
    let curated_marketplace_path = AbsolutePathBuf::try_from(
        codex_home
            .join(".tmp/plugins")
            .join(".agents/plugins/marketplace.json"),
    )
    .map_err(|_| "local curated marketplace is not available".to_string())?;
    let mut paths = vec![curated_marketplace_path];

    let api_marketplace_path = codex_home
        .join(".tmp/plugins")
        .join(".agents/plugins/api_marketplace.json");
    if api_marketplace_path.is_file() {
        paths.push(
            AbsolutePathBuf::try_from(api_marketplace_path)
                .map_err(|_| "local API curated marketplace is not available".to_string())?,
        );
    }

    Ok(paths)
}

#[must_use]
pub fn curated_plugin_cache_version(plugin_version: &str) -> String {
    if is_full_git_sha(plugin_version) {
        plugin_version[..CURATED_PLUGIN_CACHE_VERSION_SHA_PREFIX_LEN].to_string()
    } else {
        plugin_version.to_string()
    }
}

pub(crate) fn refresh_non_curated_plugin_cache_detailed(
    codex_home: &Path,
    additional_roots: &[AbsolutePathBuf],
    configured_plugin_keys: &[String],
    git_mode: PluginGitMode,
) -> Result<NonCuratedCacheRefreshOutcome, String> {
    refresh_non_curated_plugin_cache_with_mode(
        codex_home,
        additional_roots,
        configured_plugin_keys,
        NonCuratedCacheRefreshMode::IfVersionChanged,
        git_mode,
    )
}

pub(crate) fn refresh_non_curated_plugin_cache_force_reinstall_detailed(
    codex_home: &Path,
    additional_roots: &[AbsolutePathBuf],
    configured_plugin_keys: &[String],
    git_mode: PluginGitMode,
) -> Result<NonCuratedCacheRefreshOutcome, String> {
    refresh_non_curated_plugin_cache_with_mode(
        codex_home,
        additional_roots,
        configured_plugin_keys,
        NonCuratedCacheRefreshMode::ForceReinstall,
        git_mode,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the source function whole; splitting would obscure the port."
)]
fn refresh_non_curated_plugin_cache_with_mode(
    codex_home: &Path,
    additional_roots: &[AbsolutePathBuf],
    configured_plugin_keys: &[String],
    mode: NonCuratedCacheRefreshMode,
    git_mode: PluginGitMode,
) -> Result<NonCuratedCacheRefreshOutcome, String> {
    let mut configured_non_curated_plugin_ids = configured_plugin_keys
        .iter()
        .filter_map(|plugin_key| match PluginId::parse(plugin_key) {
            Ok(plugin_id) if !is_openai_curated_marketplace_name(&plugin_id.marketplace_name) => {
                Some(plugin_id)
            }
            Ok(_) => None,
            Err(err) => {
                warn!(
                    plugin_key,
                    error = %err,
                    "ignoring invalid plugin key during non-curated cache refresh setup"
                );
                None
            }
        })
        .collect::<Vec<_>>();
    configured_non_curated_plugin_ids.sort_unstable_by_key(PluginId::as_key);
    if configured_non_curated_plugin_ids.is_empty() {
        return Ok(NonCuratedCacheRefreshOutcome {
            cache_refreshed: false,
            errors: Vec::new(),
        });
    }
    let configured_non_curated_plugin_keys = configured_non_curated_plugin_ids
        .iter()
        .map(PluginId::as_key)
        .collect::<HashSet<_>>();

    let store = PluginStore::try_new(codex_home.to_path_buf()).map_err(|err| err.to_string())?;
    let marketplace_outcome = list_marketplaces_with_home(additional_roots, /*home_dir*/ None)
        .map_err(|err| format!("failed to discover marketplaces for cache refresh: {err}"))?;
    let mut plugin_sources = HashMap::<String, (MarketplacePluginSource, Option<String>)>::new();

    for marketplace in marketplace_outcome.marketplaces {
        if is_openai_curated_marketplace_name(&marketplace.name) {
            continue;
        }

        for plugin in marketplace.plugins {
            let plugin_id = match PluginId::new(plugin.name.clone(), marketplace.name.clone()) {
                Ok(plugin_id) => plugin_id,
                Err(PluginIdError::Invalid(message)) => {
                    warn!(
                        plugin = plugin.name,
                        marketplace = marketplace.name,
                        error = %message,
                        "ignoring invalid plugin entry during cache refresh"
                    );
                    continue;
                }
            };
            let plugin_key = plugin_id.as_key();
            if !configured_non_curated_plugin_keys.contains(&plugin_key) {
                continue;
            }
            if plugin_sources.contains_key(&plugin_key) {
                warn!(
                    plugin = plugin.name,
                    marketplace = marketplace.name,
                    "ignoring duplicate non-curated plugin entry during cache refresh"
                );
                continue;
            }

            let manifest_fallback = find_marketplace_plugin(&marketplace.path, &plugin.name).map_or_else(|err| {
                    warn!(
                        plugin = plugin.name,
                        marketplace = marketplace.name,
                        error = %err,
                        "failed to resolve marketplace plugin manifest fallback during cache refresh"
                    );
                    None
                }, |resolved| {
                    resolved
                        .manifest_fallback
                        .contents_if_has_metadata()
                        .map(str::to_string)
                });
            plugin_sources.insert(plugin_key, (plugin.source, manifest_fallback));
        }
    }

    let mut cache_refreshed = false;
    let mut refresh_errors = Vec::new();
    for plugin_id in configured_non_curated_plugin_ids {
        let plugin_key = plugin_id.as_key();
        let Some((source, manifest_fallback_contents)) = plugin_sources.get(&plugin_key).cloned()
        else {
            warn!(
                plugin = plugin_id.plugin_name,
                marketplace = plugin_id.marketplace_name,
                "configured non-curated plugin no longer exists in discovered marketplaces during cache refresh"
            );
            continue;
        };
        let refresh_result = (|| -> Result<bool, String> {
            let materialized =
                materialize_marketplace_plugin_source_with_mode(codex_home, &source, git_mode)
                    .map_err(|err| {
                        format!("failed to materialize plugin source for {plugin_key}: {err}")
                    })?;
            let source_path = materialized.path;
            let plugin_version = match manifest_fallback_contents.as_deref() {
                Some(manifest_contents) => plugin_version_for_source_with_fallback_manifest(
                    source_path.as_path(),
                    manifest_contents,
                ),
                None => plugin_version_for_source(source_path.as_path()),
            }
            .map_err(|err| format!("failed to read plugin version for {plugin_key}: {err}"))?;

            if mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && store.active_plugin_version(&plugin_id).as_deref()
                    == Some(plugin_version.as_str())
            {
                return Ok(false);
            }

            match manifest_fallback_contents.as_deref() {
                Some(manifest_contents) => store.install_with_version_and_fallback_manifest(
                    source_path,
                    plugin_id.clone(),
                    plugin_version,
                    manifest_contents,
                ),
                None => store.install_with_version(source_path, plugin_id.clone(), plugin_version),
            }
            .map_err(|err| format!("failed to refresh plugin cache for {plugin_key}: {err}"))?;
            Ok(true)
        })();
        match refresh_result {
            Ok(refreshed) => cache_refreshed |= refreshed,
            Err(message) => refresh_errors.push(NonCuratedCacheRefreshError {
                marketplace_name: plugin_id.marketplace_name,
                message,
            }),
        }
    }

    Ok(NonCuratedCacheRefreshOutcome {
        cache_refreshed,
        errors: refresh_errors,
    })
}

fn is_full_git_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn configured_plugins_from_config_value(
    user_config: &toml::Value,
) -> HashMap<String, PluginConfig> {
    let Some(plugins_value) = user_config.get("plugins") else {
        return HashMap::new();
    };
    match PluginsConfigToml::deserialize(plugins_value.clone()) {
        Ok(plugins) => plugins.plugins,
        Err(err) => {
            warn!("invalid plugins config: {err}");
            HashMap::new()
        }
    }
}

fn configured_plugins_from_codex_home(
    codex_home: &Path,
    read_error_message: &str,
    parse_error_message: &str,
) -> HashMap<String, PluginConfig> {
    let config_path = codex_home.join(CONFIG_TOML_FILE);
    let user_config = match fs::read_to_string(&config_path) {
        Ok(user_config) => user_config,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(err) => {
            warn!(
                path = %config_path.display(),
                error = %err,
                "{read_error_message}"
            );
            return HashMap::new();
        }
    };

    let user_config = match toml::from_str::<toml::Value>(&user_config) {
        Ok(user_config) => user_config,
        Err(err) => {
            warn!(
                path = %config_path.display(),
                error = %err,
                "{parse_error_message}"
            );
            return HashMap::new();
        }
    };

    configured_plugins_from_config_value(&user_config)
}

fn configured_plugin_ids(
    configured_plugins: HashMap<String, PluginConfig>,
    invalid_plugin_key_message: &str,
) -> Vec<PluginId> {
    configured_plugins
        .into_keys()
        .filter_map(|plugin_key| match PluginId::parse(&plugin_key) {
            Ok(plugin_id) => Some(plugin_id),
            Err(err) => {
                warn!(
                    plugin_key,
                    error = %err,
                    "{invalid_plugin_key_message}"
                );
                None
            }
        })
        .collect()
}

fn curated_plugin_ids_from_config_keys(
    configured_plugins: HashMap<String, PluginConfig>,
) -> Vec<PluginId> {
    let mut configured_curated_plugin_ids = configured_plugin_ids(
        configured_plugins,
        "ignoring invalid configured plugin key during curated sync setup",
    )
    .into_iter()
    .filter(|plugin_id| is_openai_curated_marketplace_name(&plugin_id.marketplace_name))
    .collect::<Vec<_>>();
    configured_curated_plugin_ids.sort_unstable_by_key(PluginId::as_key);
    configured_curated_plugin_ids
}

#[must_use]
pub fn configured_curated_plugin_ids_from_codex_home(codex_home: &Path) -> Vec<PluginId> {
    curated_plugin_ids_from_config_keys(configured_plugins_from_codex_home(
        codex_home,
        "failed to read user config while refreshing curated plugin cache",
        "failed to parse user config while refreshing curated plugin cache",
    ))
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the source function whole; splitting would obscure the port."
)]
async fn load_plugin(
    config_name: String,
    plugin: &PluginConfig,
    store: &PluginStore,
    scope: &PluginLoadScope<'_>,
) -> LoadedPlugin<McpServerConfig> {
    let plugin_id = PluginId::parse(&config_name);
    let active_plugin_installation = plugin_id
        .as_ref()
        .ok()
        .and_then(|plugin_id| store.active_plugin_installation(plugin_id));
    let root = active_plugin_installation.as_ref().map_or_else(
        || match &plugin_id {
            Ok(plugin_id) => store.plugin_base_root(plugin_id),
            Err(_) => store.root().clone(),
        },
        |installation| installation.root.clone(),
    );
    let mut loaded_plugin = LoadedPlugin {
        config_name,
        manifest_name: None,
        plugin_namespace: None,
        manifest_description: None,
        root,
        enabled: plugin.enabled.unwrap_or(true),
        skill_roots: Vec::new(),
        skill_discovery_mode: SkillDiscoveryMode::Recursive,
        disabled_skill_paths: HashSet::new(),
        has_enabled_skills: false,
        mcp_servers: HashMap::new(),
        apps: Vec::new(),
        hook_sources: Vec::new(),
        hook_load_warnings: Vec::new(),
        error: None,
    };

    if !loaded_plugin.enabled {
        return loaded_plugin;
    }

    let (loaded_plugin_id, installation) = match plugin_id {
        Ok(plugin_id) => {
            let Some(installation) = active_plugin_installation else {
                loaded_plugin.error = Some("plugin is not installed".to_string());
                return loaded_plugin;
            };
            (plugin_id, installation)
        }
        Err(err) => {
            loaded_plugin.error = Some(err.to_string());
            return loaded_plugin;
        }
    };

    let plugin_root = installation.root;

    if !plugin_root.as_path().is_dir() {
        loaded_plugin.error = Some("path does not exist or is not a directory".to_string());
        return loaded_plugin;
    }

    let Some(loaded_manifest) = store.manifest_cache.load(plugin_root.as_path()) else {
        loaded_plugin.error = Some("missing or invalid plugin.json".to_string());
        return loaded_plugin;
    };
    loaded_plugin.skill_discovery_mode = match loaded_manifest.format {
        PluginManifestFormat::Legacy => SkillDiscoveryMode::Recursive,
        PluginManifestFormat::AgentPlugin => SkillDiscoveryMode::DirectChildren,
    };
    let manifest = loaded_manifest.manifest;

    let manifest_paths = &manifest.paths;
    let plugin_data_root = store.plugin_data_root(&loaded_plugin_id);
    let mcp_plugin_data_root = store.mcp_data_root(&loaded_plugin_id, loaded_manifest.format);
    loaded_plugin.plugin_namespace = Some(manifest.name.clone());
    match scope {
        PluginLoadScope::AllCapabilities {
            restriction_product,
            skill_config_rules,
            plugin_skill_snapshots,
            skill_root_loader,
        } => {
            loaded_plugin.manifest_name = Some(manifest.display_name().to_string());
            loaded_plugin.manifest_description = manifest.description.clone();
            loaded_plugin.skill_roots =
                plugin_skill_roots(&plugin_root, manifest_paths, loaded_manifest.format);
            let plugin_identity = PluginIdentity {
                plugin_id: loaded_plugin_id.as_key(),
            };
            let resolved_skills = load_plugin_skill_inventory(
                &plugin_root,
                &plugin_identity,
                &manifest,
                loaded_manifest.format,
                *restriction_product,
                *plugin_skill_snapshots,
                *skill_root_loader,
            )
            .await
            .resolve(skill_config_rules);
            let has_enabled_skills = resolved_skills.has_enabled_skills();
            loaded_plugin.disabled_skill_paths = resolved_skills.disabled_skill_paths;
            loaded_plugin.has_enabled_skills = has_enabled_skills;
            loaded_plugin.mcp_servers = load_plugin_mcp_servers_from_manifest_with_format(
                plugin_root.as_path(),
                manifest_paths,
                Some(&plugin.mcp_servers),
                Some(mcp_plugin_data_root.as_path()),
                loaded_manifest.format,
            )
            .await;
            if loaded_manifest.format == PluginManifestFormat::Legacy {
                loaded_plugin.apps =
                    load_plugin_apps_from_manifest(plugin_root.as_path(), manifest_paths).await;
            }
        }
        PluginLoadScope::HooksOnly => {}
    }
    let (hook_sources, hook_load_warnings) =
        if loaded_manifest.format == PluginManifestFormat::AgentPlugin {
            (Vec::new(), Vec::new())
        } else {
            load_plugin_hooks(
                &plugin_root,
                &loaded_plugin_id,
                &plugin_data_root,
                manifest_paths,
            )
        };
    loaded_plugin.hook_sources = hook_sources;
    loaded_plugin.hook_load_warnings = hook_load_warnings;
    loaded_plugin
}

fn apply_plugin_mcp_server_policy(config: &mut McpServerConfig, policy: &PluginMcpServerConfig) {
    config.enabled = policy.enabled && !policy.has_unsupported_ema_auth;
    if policy.has_unsupported_ema_auth {
        config.auth = ra_config::McpServerAuth::EmaAuth;
    }
    if let Some(approval_mode) = policy.default_tools_approval_mode {
        config.default_tools_approval_mode = Some(approval_mode);
    }
    if let Some(enabled_tools) = &policy.enabled_tools {
        config.enabled_tools = Some(enabled_tools.clone());
    }
    if let Some(disabled_tools) = &policy.disabled_tools {
        config.disabled_tools = Some(disabled_tools.clone());
    }
    for (tool_name, tool_policy) in &policy.tools {
        let tool_config = config.tools.entry(tool_name.clone()).or_default();
        if let Some(approval_mode) = tool_policy.approval_mode {
            tool_config.approval_mode = Some(approval_mode);
        }
        tool_config.restrict_output_token_limit(tool_policy.output_token_limit);
    }
}

pub struct PluginSkillInventory {
    skills: Vec<SkillMetadata>,
    had_errors: bool,
}

impl PluginSkillInventory {
    #[expect(
        dead_code,
        reason = "Its only source caller, tool-suggestion metadata, is not ported."
    )]
    pub(crate) fn has_enabled_skills(&self, skill_config_rules: &SkillConfigRules) -> bool {
        contains_enabled_skill(
            &self.skills,
            &skill_config_rules.resolve_disabled_paths(
                self.skills
                    .iter()
                    .map(|skill| (skill.name.as_str(), &skill.path_to_skills_md)),
            ),
        )
    }

    #[must_use]
    pub fn resolve(self, skill_config_rules: &SkillConfigRules) -> ResolvedPluginSkills {
        let disabled_skill_paths = skill_config_rules.resolve_disabled_paths(
            self.skills
                .iter()
                .map(|skill| (skill.name.as_str(), &skill.path_to_skills_md)),
        );
        ResolvedPluginSkills {
            skills: self.skills,
            disabled_skill_paths,
            had_errors: self.had_errors,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPluginSkills {
    pub skills: Vec<SkillMetadata>,
    pub disabled_skill_paths: HashSet<PathUri>,
    pub had_errors: bool,
}

impl ResolvedPluginSkills {
    #[must_use]
    pub fn has_enabled_skills(&self) -> bool {
        self.had_errors || contains_enabled_skill(&self.skills, &self.disabled_skill_paths)
    }
}

fn contains_enabled_skill(
    skills: &[SkillMetadata],
    disabled_skill_paths: &HashSet<PathUri>,
) -> bool {
    skills
        .iter()
        .any(|skill| !disabled_skill_paths.contains(&skill.path_to_skills_md))
}

#[allow(
    unreachable_pub,
    reason = "External source tests call this helper through the test facade."
)]
pub async fn load_plugin_skill_inventory(
    plugin_root: &AbsolutePathBuf,
    plugin_identity: &PluginIdentity,
    manifest: &PluginManifest,
    manifest_format: PluginManifestFormat,
    restriction_product: Option<Product>,
    plugin_skill_snapshots: Option<&SkillRootSnapshots<PluginSkillRoot>>,
    skill_root_loader: &dyn SkillRootLoader<PluginSkillRoot>,
) -> PluginSkillInventory {
    let discovery_mode = match manifest_format {
        PluginManifestFormat::Legacy => SkillDiscoveryMode::Recursive,
        PluginManifestFormat::AgentPlugin => SkillDiscoveryMode::DirectChildren,
    };
    let roots = plugin_skill_roots(plugin_root, &manifest.paths, manifest_format)
        .into_iter()
        .map(|path| PluginSkillRoot {
            path,
            plugin_identity: plugin_identity.clone(),
            plugin_namespace: manifest.name.clone(),
            plugin_root: plugin_root.clone(),
            discovery_mode,
        })
        .collect();
    let outcome = skill_root_loader
        .load_roots(SkillRootLoadRequest {
            roots,
            restriction_product,
            snapshots: plugin_skill_snapshots.cloned(),
        })
        .await;

    PluginSkillInventory {
        skills: outcome.skills,
        had_errors: !outcome.errors.is_empty(),
    }
}

pub(crate) fn plugin_skill_roots(
    plugin_root: &AbsolutePathBuf,
    manifest_paths: &PluginManifestPaths,
    manifest_format: PluginManifestFormat,
) -> Vec<AbsolutePathBuf> {
    let mut paths = if manifest_paths.skills.is_empty() {
        default_skill_roots(plugin_root)
    } else {
        manifest_paths.skills.clone()
    };
    if manifest_format == PluginManifestFormat::Legacy {
        let migrated_command_skills = migrated_command_skills_root(plugin_root);
        if migrated_command_skills.is_dir() {
            paths.push(migrated_command_skills);
        }
    }
    paths.sort_unstable();
    paths.dedup();
    paths
}

fn default_skill_roots(plugin_root: &AbsolutePathBuf) -> Vec<AbsolutePathBuf> {
    let skills_dir = plugin_root.join(DEFAULT_SKILLS_DIR_NAME);
    if skills_dir.is_dir() {
        vec![skills_dir]
    } else {
        Vec::new()
    }
}

fn plugin_mcp_config_paths(
    plugin_root: &Path,
    manifest_paths: &PluginManifestPaths,
) -> Vec<AbsolutePathBuf> {
    if let Some(PluginManifestMcpServers::Path(path)) = &manifest_paths.mcp_servers {
        return vec![path.clone()];
    }
    default_mcp_config_paths(plugin_root)
}

fn default_mcp_config_paths(plugin_root: &Path) -> Vec<AbsolutePathBuf> {
    let mut paths = Vec::new();
    let default_path = plugin_root.join(DEFAULT_MCP_CONFIG_FILE);
    if default_path.is_file()
        && let Ok(default_path) = AbsolutePathBuf::try_from(default_path)
    {
        paths.push(default_path);
    }
    paths.sort_unstable_by(|left, right| left.as_path().cmp(right.as_path()));
    paths.dedup_by(|left, right| left.as_path() == right.as_path());
    paths
}

pub async fn load_plugin_apps(plugin_root: &Path) -> Vec<AppDeclaration> {
    if let Some(loaded_manifest) = load_plugin_manifest_with_format(plugin_root) {
        if loaded_manifest.format == PluginManifestFormat::AgentPlugin {
            return Vec::new();
        }
        return load_plugin_apps_from_manifest(plugin_root, &loaded_manifest.manifest.paths).await;
    }
    load_apps_from_paths(plugin_root, default_app_config_paths(plugin_root)).await
}

pub(crate) async fn load_plugin_apps_from_manifest(
    plugin_root: &Path,
    manifest_paths: &PluginManifestPaths,
) -> Vec<AppDeclaration> {
    load_apps_from_paths(
        plugin_root,
        plugin_app_config_paths(plugin_root, manifest_paths),
    )
    .await
}

#[must_use]
pub fn plugin_app_declarations_from_value(value: &JsonValue) -> Vec<AppDeclaration> {
    let Ok(mut apps) = parse_plugin_app_config_value(value.clone()) else {
        return Vec::new();
    };
    apps.retain(|app| !app.connector_id.0.trim().is_empty());
    let mut seen_connector_ids = HashSet::new();
    apps.retain(|app| seen_connector_ids.insert(app.connector_id.0.clone()));
    apps
}

fn plugin_app_config_paths(
    plugin_root: &Path,
    manifest_paths: &PluginManifestPaths,
) -> Vec<AbsolutePathBuf> {
    if let Some(path) = &manifest_paths.apps {
        return vec![path.clone()];
    }
    default_app_config_paths(plugin_root)
}

fn default_app_config_paths(plugin_root: &Path) -> Vec<AbsolutePathBuf> {
    let mut paths = Vec::new();
    let default_path = plugin_root.join(DEFAULT_APP_CONFIG_FILE);
    if default_path.is_file()
        && let Ok(default_path) = AbsolutePathBuf::try_from(default_path)
    {
        paths.push(default_path);
    }
    paths.sort_unstable_by(|left, right| left.as_path().cmp(right.as_path()));
    paths.dedup_by(|left, right| left.as_path() == right.as_path());
    paths
}

// Discover plugin-bundled hooks from manifest `hooks` entries when present
// (path, paths, inline object, or inline objects), otherwise from the default
// `hooks/hooks.json` file.
#[must_use]
pub fn load_plugin_hooks(
    plugin_root: &AbsolutePathBuf,
    plugin_id: &PluginId,
    plugin_data_root: &AbsolutePathBuf,
    manifest_paths: &PluginManifestPaths,
) -> (Vec<PluginHookSource>, Vec<String>) {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    match &manifest_paths.hooks {
        Some(PluginManifestHooks::Paths(paths)) => {
            for path in paths {
                append_plugin_hook_file(
                    plugin_root,
                    plugin_id,
                    plugin_data_root,
                    path,
                    &mut sources,
                    &mut warnings,
                );
            }
        }
        Some(PluginManifestHooks::Inline(hooks_files)) => {
            let manifest_path = find_plugin_manifest_path(plugin_root.as_path())
                .and_then(|path| AbsolutePathBuf::try_from(path).ok())
                .unwrap_or_else(|| plugin_root.join(".codex-plugin/plugin.json"));
            for (index, hooks_file) in hooks_files.iter().enumerate() {
                if hooks_file.hooks.is_empty() {
                    continue;
                }
                sources.push(PluginHookSource {
                    plugin_id: plugin_id.clone(),
                    plugin_root: plugin_root.clone(),
                    plugin_data_root: plugin_data_root.clone(),
                    source_path: manifest_path.clone(),
                    source_relative_path: format!("plugin.json#hooks[{index}]"),
                    hooks: hooks_file.hooks.clone(),
                });
            }
        }
        None => {
            let default_path = plugin_root.join(DEFAULT_HOOKS_CONFIG_FILE);
            if default_path.as_path().is_file() {
                append_plugin_hook_file(
                    plugin_root,
                    plugin_id,
                    plugin_data_root,
                    &default_path,
                    &mut sources,
                    &mut warnings,
                );
            }
        }
    }
    (sources, warnings)
}

// Append one resolved plugin hook file, keeping source metadata for runtime
// reporting and collecting load warnings for startup surfacing.
fn append_plugin_hook_file(
    plugin_root: &AbsolutePathBuf,
    plugin_id: &PluginId,
    plugin_data_root: &AbsolutePathBuf,
    path: &AbsolutePathBuf,
    sources: &mut Vec<PluginHookSource>,
    warnings: &mut Vec<String>,
) {
    let contents = match fs::read_to_string(path.as_path()) {
        Ok(contents) => contents,
        Err(err) => {
            warnings.push(format!(
                "failed to read plugin hooks config {}: {err}",
                path.display()
            ));
            return;
        }
    };
    let parsed = match serde_json::from_str::<HooksFile>(&contents) {
        Ok(parsed) => parsed,
        Err(err) => {
            warnings.push(format!(
                "failed to parse plugin hooks config {}: {err}",
                path.display()
            ));
            return;
        }
    };
    if parsed.hooks.is_empty() {
        return;
    }

    let source_relative_path = path
        .as_path()
        .strip_prefix(plugin_root.as_path())
        .unwrap_or(path.as_path())
        .to_string_lossy()
        .replace('\\', "/");

    sources.push(PluginHookSource {
        plugin_id: plugin_id.clone(),
        plugin_root: plugin_root.clone(),
        plugin_data_root: plugin_data_root.clone(),
        source_path: path.clone(),
        source_relative_path,
        hooks: parsed.hooks,
    });
}

async fn load_apps_from_paths(
    plugin_root: &Path,
    app_config_paths: Vec<AbsolutePathBuf>,
) -> Vec<AppDeclaration> {
    let mut app_declarations = Vec::new();
    for app_config_path in app_config_paths {
        let Ok(contents) = tokio::fs::read_to_string(app_config_path.as_path()).await else {
            continue;
        };
        let declarations = match parse_plugin_app_config(&contents) {
            Ok(declarations) => declarations,
            Err(err) => {
                warn!(
                    path = %app_config_path.display(),
                    "failed to parse plugin app config: {err}"
                );
                continue;
            }
        };

        app_declarations.extend(declarations.into_iter().filter(|app| {
            if app.connector_id.0.trim().is_empty() {
                warn!(
                    plugin = %plugin_root.display(),
                    "plugin app config is missing an app id"
                );
                false
            } else {
                true
            }
        }));
    }
    app_declarations
}

#[cfg_attr(
    not(feature = "test-api"),
    expect(
        dead_code,
        reason = "Only the source tests reach this helper, through the test facade."
    )
)]
pub(crate) async fn plugin_capability_summary_from_root(
    plugin_id: &PluginId,
    plugin_root: &AbsolutePathBuf,
    skill_root_loader: &dyn SkillRootLoader<PluginSkillRoot>,
    manifest_cache: &ManifestCache,
) -> Option<PluginCapabilitySummary> {
    let loaded_manifest = manifest_cache.load(plugin_root.as_path())?;
    let manifest_format = loaded_manifest.format;
    let manifest = loaded_manifest.manifest;
    let plugin_identity = PluginIdentity {
        plugin_id: plugin_id.as_key(),
    };

    let manifest_paths = &manifest.paths;
    let has_skills = match manifest_format {
        PluginManifestFormat::Legacy => {
            !plugin_skill_roots(plugin_root, manifest_paths, manifest_format).is_empty()
        }
        PluginManifestFormat::AgentPlugin => {
            !load_plugin_skill_inventory(
                plugin_root,
                &plugin_identity,
                &manifest,
                manifest_format,
                /*restriction_product*/ None,
                /*plugin_skill_snapshots*/ None,
                skill_root_loader,
            )
            .await
            .skills
            .is_empty()
        }
    };
    let mut mcp_server_names = load_plugin_mcp_servers_from_manifest_with_format(
        plugin_root.as_path(),
        manifest_paths,
        /*plugin_policy*/ None,
        /*plugin_data_root*/ None,
        manifest_format,
    )
    .await
    .into_keys()
    .collect::<Vec<_>>();
    mcp_server_names.sort_unstable();
    mcp_server_names.dedup();

    let app_declarations = if manifest_format == PluginManifestFormat::AgentPlugin {
        Vec::new()
    } else {
        load_plugin_apps_from_manifest(plugin_root.as_path(), manifest_paths).await
    };
    let app_connector_ids = app_connector_ids_from_declarations(&app_declarations);

    Some(PluginCapabilitySummary {
        config_name: plugin_id.as_key(),
        display_name: plugin_id.plugin_name.clone(),
        plugin_namespace: Some(manifest.name.clone()),
        description: None,
        has_skills,
        mcp_server_names,
        app_connector_ids,
    })
}

/// Loads plugin MCP servers without applying user-specific policy overrides.
pub async fn load_plugin_mcp_servers(plugin_root: &Path) -> HashMap<String, McpServerConfig> {
    load_plugin_mcp_servers_with_policy(plugin_root, /*plugin_policy*/ None).await
}

/// Loads plugin MCP servers with the effective configuration policy for an installed plugin.
pub async fn load_configured_plugin_mcp_servers(
    plugin_root: &Path,
    plugin_id: &PluginId,
    config_layer_stack: &ConfigLayerStack,
    codex_home: &Path,
) -> HashMap<String, McpServerConfig> {
    let configured_plugins = configured_plugins_from_stack(config_layer_stack, codex_home);
    let plugin_id = plugin_id.as_key();
    let plugin_policy = configured_plugins
        .get(&plugin_id)
        .map(|plugin| &plugin.mcp_servers);

    load_plugin_mcp_servers_with_policy(plugin_root, plugin_policy).await
}

/// Applies user policy without widening the selected plugin's declared restrictions.
async fn load_plugin_mcp_servers_with_policy(
    plugin_root: &Path,
    plugin_policy: Option<&HashMap<String, PluginMcpServerConfig>>,
) -> HashMap<String, McpServerConfig> {
    load_declared_plugin_mcp_servers(plugin_root, plugin_policy).await
}

async fn load_declared_plugin_mcp_servers(
    plugin_root: &Path,
    plugin_policy: Option<&HashMap<String, PluginMcpServerConfig>>,
) -> HashMap<String, McpServerConfig> {
    let Some(loaded_manifest) = load_plugin_manifest_with_format(plugin_root) else {
        return HashMap::new();
    };

    load_plugin_mcp_servers_from_manifest_with_format(
        plugin_root,
        &loaded_manifest.manifest.paths,
        plugin_policy,
        /*plugin_data_root*/ None,
        loaded_manifest.format,
    )
    .await
}

pub(crate) async fn load_plugin_mcp_servers_from_manifest_with_format(
    plugin_root: &Path,
    manifest_paths: &PluginManifestPaths,
    plugin_policy: Option<&HashMap<String, PluginMcpServerConfig>>,
    plugin_data_root: Option<&Path>,
    manifest_format: PluginManifestFormat,
) -> HashMap<String, McpServerConfig> {
    let mut mcp_servers = HashMap::new();
    match &manifest_paths.mcp_servers {
        Some(PluginManifestMcpServers::Object(object_servers)) => {
            let plugin_mcp = load_mcp_servers_from_manifest_object(plugin_root, object_servers);
            for (name, mut config) in plugin_mcp.mcp_servers {
                if let Some(policy) = plugin_policy.and_then(|policy| policy.get(&name)) {
                    apply_plugin_mcp_server_policy(&mut config, policy);
                }
                if mcp_servers.insert(name.clone(), config).is_some() {
                    warn!(
                        plugin = %plugin_root.display(),
                        server = name,
                        "plugin manifest MCP object overwrote an earlier server definition"
                    );
                }
            }
        }
        Some(PluginManifestMcpServers::Path(_)) | None => {
            for mcp_config_path in plugin_mcp_config_paths(plugin_root, manifest_paths) {
                let plugin_mcp = load_mcp_servers_from_file(
                    plugin_root,
                    plugin_data_root,
                    manifest_format,
                    &mcp_config_path,
                )
                .await;
                for (name, mut config) in plugin_mcp.mcp_servers {
                    if let Some(policy) = plugin_policy.and_then(|policy| policy.get(&name)) {
                        apply_plugin_mcp_server_policy(&mut config, policy);
                    }
                    if mcp_servers.insert(name.clone(), config).is_some() {
                        warn!(
                            plugin = %plugin_root.display(),
                            path = %mcp_config_path.display(),
                            server = name,
                            "plugin MCP file overwrote an earlier server definition"
                        );
                    }
                }
            }
        }
    }

    if manifest_format == PluginManifestFormat::AgentPlugin {
        agent_plugin_mcp_overlay::apply_codex_env_overlay(plugin_root, &mut mcp_servers).await;
    }

    mcp_servers
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the source function whole; splitting would obscure the port."
)]
async fn load_mcp_servers_from_file(
    plugin_root: &Path,
    plugin_data_root: Option<&Path>,
    manifest_format: PluginManifestFormat,
    mcp_config_path: &AbsolutePathBuf,
) -> PluginMcpDiscovery {
    let is_agent_plugin_mcp = manifest_format == PluginManifestFormat::AgentPlugin;
    if is_agent_plugin_mcp {
        match tokio::fs::symlink_metadata(mcp_config_path.as_path()).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                warn!(
                    path = %mcp_config_path.display(),
                    "Agent Plugins MCP config is not a regular file; disabling MCP"
                );
                return PluginMcpDiscovery::default();
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return PluginMcpDiscovery::default();
            }
            Err(err) => {
                warn!(
                    path = %mcp_config_path.display(),
                    "failed to inspect Agent Plugins MCP config; disabling MCP: {err}"
                );
                return PluginMcpDiscovery::default();
            }
        }
        let resolved_root = match tokio::fs::canonicalize(plugin_root).await {
            Ok(path) => path,
            Err(err) => {
                warn!(
                    plugin = %plugin_root.display(),
                    "failed to resolve Agent Plugins root; disabling MCP: {err}"
                );
                return PluginMcpDiscovery::default();
            }
        };
        let resolved_config = match tokio::fs::canonicalize(mcp_config_path.as_path()).await {
            Ok(path) => path,
            Err(err) => {
                warn!(
                    path = %mcp_config_path.display(),
                    "failed to resolve Agent Plugins MCP config; disabling MCP: {err}"
                );
                return PluginMcpDiscovery::default();
            }
        };
        if !resolved_config.starts_with(&resolved_root) {
            warn!(
                plugin = %plugin_root.display(),
                path = %mcp_config_path.display(),
                "Agent Plugins MCP config resolves outside the plugin root; disabling MCP"
            );
            return PluginMcpDiscovery::default();
        }
    }
    let Ok(contents) = tokio::fs::read_to_string(mcp_config_path.as_path()).await else {
        return PluginMcpDiscovery::default();
    };
    let fallback_data_root = plugin_root.join(".plugin-data");
    let mut parsed = match if is_agent_plugin_mcp {
        parse_agent_plugin_mcp_config(
            plugin_root,
            plugin_data_root.unwrap_or(&fallback_data_root),
            &contents,
        )
    } else {
        parse_plugin_mcp_config(plugin_root, &contents)
    } {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(
                path = %mcp_config_path.display(),
                "failed to parse plugin MCP config: {err}"
            );
            return PluginMcpDiscovery::default();
        }
    };
    if is_agent_plugin_mcp
        && let Some(plugin_data_root) = plugin_data_root
        && parsed
            .servers
            .values()
            .any(|server| matches!(&server.transport, McpServerTransportConfig::Stdio { .. }))
        && let Err(err) = tokio::fs::create_dir_all(plugin_data_root).await
    {
        warn!(
            plugin = %plugin_root.display(),
            path = %plugin_data_root.display(),
            "failed to create Agent Plugins data directory; disabling stdio MCP servers: {err}"
        );
        parsed.servers.retain(|_, server| {
            !matches!(&server.transport, McpServerTransportConfig::Stdio { .. })
        });
    }
    for error in parsed.errors {
        warn!(
            plugin = %plugin_root.display(),
            server = error.name,
            path = %mcp_config_path.display(),
            error = error.message,
            "failed to parse plugin MCP server"
        );
    }
    PluginMcpDiscovery {
        mcp_servers: parsed.servers.into_iter().collect(),
    }
}

fn load_mcp_servers_from_manifest_object(
    plugin_root: &Path,
    object_config: &str,
) -> PluginMcpDiscovery {
    let parsed = match parse_plugin_mcp_config(plugin_root, object_config) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(
                plugin = %plugin_root.display(),
                "failed to parse plugin manifest MCP object: {err}"
            );
            return PluginMcpDiscovery::default();
        }
    };
    for error in parsed.errors {
        warn!(
            plugin = %plugin_root.display(),
            server = error.name,
            error = error.message,
            "failed to parse plugin manifest MCP object server"
        );
    }
    PluginMcpDiscovery {
        mcp_servers: parsed.servers.into_iter().collect(),
    }
}

#[derive(Debug, Default)]
struct PluginMcpDiscovery {
    mcp_servers: HashMap<String, McpServerConfig>,
}

use super::materialize::materialize_marketplace_plugin_source_with_mode;
pub use super::mcp_policy::apply_configured_plugin_mcp_server_policies;

/// Source helpers used by the external manager integration suite.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    fn collapse_non_curated_cache_refresh(
        outcome: Result<NonCuratedCacheRefreshOutcome, String>,
    ) -> Result<bool, String> {
        let outcome = outcome?;
        if outcome.errors.is_empty() {
            Ok(outcome.cache_refreshed)
        } else {
            Err(outcome
                .errors
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>()
                .join("; "))
        }
    }

    pub fn refresh_non_curated_plugin_cache_force_reinstall(
        codex_home: &Path,
        additional_roots: &[AbsolutePathBuf],
        configured_plugin_keys: &[String],
    ) -> Result<bool, String> {
        collapse_non_curated_cache_refresh(
            refresh_non_curated_plugin_cache_force_reinstall_detailed(
                codex_home,
                additional_roots,
                configured_plugin_keys,
                PluginGitMode::Automatic,
            ),
        )
    }

    pub fn refresh_non_curated_plugin_cache(
        codex_home: &Path,
        additional_roots: &[AbsolutePathBuf],
        configured_plugin_keys: &[String],
    ) -> Result<bool, String> {
        collapse_non_curated_cache_refresh(refresh_non_curated_plugin_cache_detailed(
            codex_home,
            additional_roots,
            configured_plugin_keys,
            PluginGitMode::Automatic,
        ))
    }

    pub use super::load_plugin_skill_inventory;
    use super::{
        AbsolutePathBuf, NonCuratedCacheRefreshOutcome, Path, PluginCapabilitySummary,
        PluginGitMode, PluginId, PluginSkillRoot, SkillRootLoader,
        refresh_non_curated_plugin_cache_detailed,
        refresh_non_curated_plugin_cache_force_reinstall_detailed,
    };
    pub async fn plugin_capability_summary_without_cache(
        plugin_id: &PluginId,
        root: &AbsolutePathBuf,
        loader: &dyn SkillRootLoader<PluginSkillRoot>,
    ) -> Option<PluginCapabilitySummary> {
        super::plugin_capability_summary_from_root(
            plugin_id,
            root,
            loader,
            &crate::plugins::manifest::ManifestCache::disabled(),
        )
        .await
    }
}
