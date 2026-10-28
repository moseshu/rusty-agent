// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! App-server lowering for source-backed non-account plugin and capability operations.
use super::marketplace::MarketplacePluginSource;
use super::{ConfiguredMarketplacePlugin, PluginReadOutcome, PluginServices};
use ra_protocol::{
    frame::JsonRpcErrorObject,
    server::{PluginManagement, protocol as wire},
};
use serde::Serialize;
use std::sync::Arc;

pub(super) struct PluginRequestProcessor {
    services: Arc<PluginServices>,
}
impl PluginServices {
    /// Installs these explicitly selected services at the app-server's product boundary.
    pub fn protocol_adapter(self: &Arc<Self>) -> Arc<dyn PluginManagement> {
        Arc::new(PluginRequestProcessor {
            services: self.clone(),
        })
    }
}
fn internal(err: impl std::fmt::Display) -> JsonRpcErrorObject {
    JsonRpcErrorObject::internal_error(err.to_string())
}
fn invalid(err: impl std::fmt::Display) -> JsonRpcErrorObject {
    JsonRpcErrorObject::invalid_request(err.to_string())
}
fn json(value: impl Serialize) -> Result<serde_json::Value, JsonRpcErrorObject> {
    serde_json::to_value(value).map_err(internal)
}
#[async_trait::async_trait]
impl PluginManagement for PluginRequestProcessor {
    #[allow(
        clippy::too_many_lines,
        reason = "One arm per method, as Codex's dispatch has."
    )]
    async fn handle(
        &self,
        request: wire::PluginManagementRequest,
    ) -> Result<serde_json::Value, JsonRpcErrorObject> {
        let services = &self.services;
        match request {
            wire::PluginManagementRequest::PluginList(params) => {
                let include_local = params
                    .marketplace_kinds
                    .as_ref()
                    .is_none_or(|kinds| kinds.contains(&wire::PluginListMarketplaceKind::Local));
                let context = services
                    .config_manager
                    .marketplace_context(params.cwds.unwrap_or_default())
                    .await
                    .map_err(internal)?;
                if !include_local {
                    return json(wire::PluginListResponse {
                        marketplaces: Vec::new(),
                        marketplace_load_errors: Vec::new(),
                        featured_plugin_ids: Vec::new(),
                    });
                }
                if params.force_refetch
                    && services
                        .plugins
                        .refresh_non_curated_plugin_cache_for_context(&context)
                        .await
                {
                    services.invalidate();
                }
                let manager = services.plugins.clone();
                let scope = context.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    manager.list_marketplaces_for_context(&scope, true)
                })
                .await
                .map_err(internal)?
                .map_err(|err| marketplace_error(err, "list marketplaces"))?;
                services
                    .plugins
                    .maybe_start_non_curated_plugin_cache_refresh(&context, &outcome.marketplaces);
                // Keep the source exact-name trigger; the API-key curated catalog does not fetch.
                let featured_plugin_ids = if outcome
                    .marketplaces
                    .iter()
                    .any(|marketplace| marketplace.name == super::OPENAI_CURATED_MARKETPLACE_NAME)
                {
                    match services
                        .plugins
                        .featured_plugin_ids_for_config(&context.global_config)
                        .await
                    {
                        Ok(ids) => ids,
                        Err(error) => {
                            tracing::warn!(%error, "plugin/list featured plugin fetch failed; returning empty featured ids");
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                };
                json(wire::PluginListResponse {
                    featured_plugin_ids,
                    marketplaces: outcome.marketplaces.into_iter().map(marketplace).collect(),
                    marketplace_load_errors: outcome
                        .errors
                        .into_iter()
                        .map(|err| wire::MarketplaceLoadErrorInfo {
                            marketplace_path: err.path,
                            message: err.message,
                        })
                        .collect(),
                })
            }
            wire::PluginManagementRequest::PluginInstalled(params) => {
                let context = services
                    .config_manager
                    .marketplace_context(params.cwds.unwrap_or_default())
                    .await
                    .map_err(internal)?;
                let suggestions = params.install_suggestion_plugin_names.unwrap_or_default();
                let manager = services.plugins.clone();
                let mut outcome = tokio::task::spawn_blocking(move || {
                    manager.list_marketplaces_for_context(&context, true)
                })
                .await
                .map_err(internal)?
                .map_err(|err| marketplace_error(err, "list marketplaces"))?;
                for marketplace in &mut outcome.marketplaces {
                    marketplace
                        .plugins
                        .retain(|plugin| plugin.installed || suggestions.contains(&plugin.name));
                }
                outcome
                    .marketplaces
                    .retain(|marketplace| !marketplace.plugins.is_empty());
                json(wire::PluginInstalledResponse {
                    marketplaces: outcome.marketplaces.into_iter().map(marketplace).collect(),
                    marketplace_load_errors: outcome
                        .errors
                        .into_iter()
                        .map(|err| wire::MarketplaceLoadErrorInfo {
                            marketplace_path: err.path,
                            message: err.message,
                        })
                        .collect(),
                })
            }
            wire::PluginManagementRequest::PluginRead(params) => {
                let path = params
                    .marketplace_path
                    .ok_or_else(|| invalid("plugin/read requires marketplacePath"))?;
                let cwd = path
                    .parent()
                    .map(ra_paths::AbsolutePathBuf::from_absolute_path)
                    .transpose()
                    .map_err(invalid)?
                    .unwrap_or_else(|| services.config_manager.home().clone());
                let config = services
                    .config_manager
                    .load_for_cwd(cwd)
                    .await
                    .map_err(internal)?;
                let outcome = services
                    .plugins
                    .read_plugin_for_config(
                        &config.plugins_config_input(),
                        &super::PluginReadRequest {
                            plugin_name: params.plugin_name,
                            marketplace_path: path,
                        },
                    )
                    .await
                    .map_err(|err| marketplace_error(err, "read plugin details"))?;
                json(wire::PluginReadResponse {
                    plugin: detail(outcome),
                })
            }
            wire::PluginManagementRequest::PluginInstall(params) => {
                let path = params
                    .marketplace_path
                    .ok_or_else(|| invalid("plugin/install requires marketplacePath"))?;
                let result = services
                    .install(super::PluginInstallRequest {
                        marketplace_path: path,
                        plugin_name: params.plugin_name,
                    })
                    .await
                    .map_err(plugin_install_error)?;
                json(wire::PluginInstallResponse {
                    auth_policy: result.auth_policy.into(),
                })
            }
            wire::PluginManagementRequest::PluginUninstall(params) => {
                services
                    .uninstall(params.plugin_id)
                    .await
                    .map_err(plugin_uninstall_error)?;
                json(wire::PluginUninstallResponse {})
            }
            wire::PluginManagementRequest::MarketplaceAdd(params) => {
                let result = services
                    .add_marketplace(super::marketplace_add::MarketplaceAddRequest {
                        source: params.source,
                        ref_name: params.ref_name,
                        sparse_paths: params.sparse_paths.unwrap_or_default(),
                    })
                    .await
                    .map_err(|err| match err {
                        super::marketplace_add::MarketplaceAddError::InvalidRequest(message) => {
                            invalid(message)
                        }
                        super::marketplace_add::MarketplaceAddError::Internal(message) => {
                            internal(message)
                        }
                    })?;
                json(wire::MarketplaceAddResponse {
                    marketplace_name: result.marketplace_name,
                    installed_root: result.installed_root,
                    already_added: result.already_added,
                })
            }
            wire::PluginManagementRequest::MarketplaceRemove(params) => {
                let result = services
                    .remove_marketplace(super::marketplace_remove::MarketplaceRemoveRequest {
                        marketplace_name: params.marketplace_name,
                    })
                    .await
                    .map_err(|err| match err {
                        super::marketplace_remove::MarketplaceRemoveError::InvalidRequest(
                            message,
                        ) => invalid(message),
                        super::marketplace_remove::MarketplaceRemoveError::Internal(message) => {
                            internal(message)
                        }
                    })?;
                json(wire::MarketplaceRemoveResponse {
                    marketplace_name: result.marketplace_name,
                    installed_root: result.removed_installed_root,
                })
            }
            wire::PluginManagementRequest::MarketplaceUpgrade(params) => {
                let result = services
                    .upgrade_marketplaces(params.marketplace_name)
                    .await
                    .map_err(invalid)?;
                json(wire::MarketplaceUpgradeResponse {
                    selected_marketplaces: result.selected_marketplaces,
                    upgraded_roots: result.upgraded_roots,
                    errors: result
                        .errors
                        .into_iter()
                        .map(|error| wire::MarketplaceUpgradeErrorInfo {
                            marketplace_name: error.marketplace_name,
                            message: error.message,
                        })
                        .collect(),
                })
            }
            wire::PluginManagementRequest::SkillsList(params) => {
                if params.force_reload {
                    services.skills.clear_cache();
                }
                let cwds = if params.cwds.is_empty() {
                    vec![services.config_manager.cwd().to_path_buf()]
                } else {
                    params.cwds
                };
                let mut data = Vec::new();
                for cwd in cwds {
                    let absolute =
                        ra_paths::AbsolutePathBuf::from_absolute_path(&cwd).map_err(invalid)?;
                    match services.snapshot(absolute).await {
                        Ok(snapshot) => data.push(wire::SkillsListEntry {
                            cwd,
                            skills: snapshot
                                .skills
                                .outcome()
                                .skills
                                .iter()
                                .map(|skill| {
                                    skill_metadata(
                                        skill,
                                        snapshot.skills.outcome().is_skill_enabled(skill),
                                    )
                                })
                                .collect(),
                            errors: snapshot
                                .skills
                                .outcome()
                                .errors
                                .iter()
                                .map(|err| wire::SkillErrorInfo {
                                    path: err.path.to_path_buf(),
                                    message: err.message.clone(),
                                })
                                .collect(),
                        }),
                        Err(err) => data.push(wire::SkillsListEntry {
                            cwd: cwd.clone(),
                            skills: Vec::new(),
                            errors: vec![wire::SkillErrorInfo {
                                path: cwd,
                                message: format!("failed to reload config: {err}"),
                            }],
                        }),
                    }
                }
                json(wire::SkillsListResponse { data })
            }
            wire::PluginManagementRequest::SkillsExtraRootsSet(params) => {
                services.skills.set_extra_roots(params.extra_roots);
                services.invalidate();
                json(wire::SkillsExtraRootsSetResponse {})
            }
            wire::PluginManagementRequest::HooksList(params) => {
                let cwds = if params.cwds.is_empty() {
                    vec![services.config_manager.cwd().to_path_buf()]
                } else {
                    params.cwds
                };
                let mut data = Vec::new();
                for cwd in cwds {
                    let absolute =
                        ra_paths::AbsolutePathBuf::from_absolute_path(&cwd).map_err(invalid)?;
                    match services.config_manager.load_for_cwd(absolute).await {
                        Ok(config) => {
                            let plugins = services
                                .plugins
                                .plugins_for_config(&config.plugins_config_input())
                                .await;
                            let hooks = crate::hooks::list_hooks(config.hooks_config(&plugins));
                            data.push(wire::HooksListEntry {
                                cwd,
                                hooks: hooks
                                    .hooks
                                    .into_iter()
                                    .filter(|hook| !hook.builtin)
                                    .map(hook_metadata)
                                    .collect(),
                                warnings: hooks.warnings,
                                errors: Vec::new(),
                            });
                        }
                        Err(err) => data.push(wire::HooksListEntry {
                            cwd: cwd.clone(),
                            hooks: Vec::new(),
                            warnings: Vec::new(),
                            errors: vec![wire::HookErrorInfo {
                                path: cwd,
                                message: format!("failed to reload config: {err}"),
                            }],
                        }),
                    }
                }
                json(wire::HooksListResponse { data })
            }
            wire::PluginManagementRequest::ConfigRead(params) => {
                let fallback_cwd = params.cwd.clone();
                let mut response = services
                    .config_manager
                    .read(params)
                    .await
                    .map_err(config_error)?;
                let config = match fallback_cwd {
                    Some(cwd) => {
                        ra_paths::AbsolutePathBuf::from_absolute_path(cwd).map_err(internal)?
                    }
                    None => services.config_manager.cwd().clone(),
                };
                let config = services
                    .config_manager
                    .load_for_cwd(config)
                    .await
                    .map_err(|err| {
                        internal(format!(
                            "failed to resolve feature override precedence: {err}"
                        ))
                    })?;
                for feature_key in SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT {
                    let Some(feature) = ra_config::features::feature_for_key(feature_key) else {
                        continue;
                    };
                    let features = response
                        .config
                        .additional
                        .entry("features".to_string())
                        .or_insert_with(|| serde_json::json!({}));
                    if !features.is_object() {
                        *features = serde_json::json!({});
                    }
                    if let Some(features) = features.as_object_mut() {
                        features.insert(
                            (*feature_key).to_string(),
                            serde_json::json!(config.features.enabled(feature)),
                        );
                    }
                }
                json(response)
            }
            wire::PluginManagementRequest::ConfigValueWrite(params) => json(
                services
                    .write_config_value(params)
                    .await
                    .map_err(config_error)?,
            ),
            wire::PluginManagementRequest::ConfigBatchWrite(params) => json(
                services
                    .batch_write_config(params)
                    .await
                    .map_err(config_error)?,
            ),
            wire::PluginManagementRequest::SkillsConfigWrite(params) => {
                let wire::SkillsConfigWriteParams {
                    path,
                    name,
                    enabled,
                } = params;
                let edit = match (path, name) {
                    (Some(path), None) => ra_config::ConfigEdit::SetSkillConfig {
                        path: path.into_path_buf(),
                        enabled,
                    },
                    (None, Some(name)) if !name.trim().is_empty() => {
                        ra_config::ConfigEdit::SetSkillConfigByName { name, enabled }
                    }
                    _ => {
                        return Err(JsonRpcErrorObject::invalid_params(
                            "skills/config/write requires exactly one of path or name",
                        ));
                    }
                };
                services
                    .write_skill_config(edit)
                    .await
                    .map_err(|err| internal(format!("failed to update skill settings: {err}")))?;
                json(wire::SkillsConfigWriteResponse {
                    effective_enabled: enabled,
                })
            }
            _ => Err(invalid("unsupported plugin operation")),
        }
    }
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.services.subscribe()
    }
}
fn source(source: MarketplacePluginSource) -> wire::PluginSource {
    match source {
        MarketplacePluginSource::Local { path } => wire::PluginSource::Local { path },
        MarketplacePluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        } => wire::PluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        },
        MarketplacePluginSource::Npm {
            package,
            version,
            registry,
        } => wire::PluginSource::Npm {
            package,
            version,
            registry,
        },
    }
}
fn interface(value: super::manifest::PluginManifestInterface) -> wire::PluginInterface {
    wire::PluginInterface {
        display_name: value.display_name,
        short_description: value.short_description,
        long_description: value.long_description,
        developer_name: value.developer_name,
        category: value.category,
        capabilities: value.capabilities,
        website_url: value.website_url,
        privacy_policy_url: value.privacy_policy_url,
        terms_of_service_url: value.terms_of_service_url,
        default_prompt: value.default_prompt,
        brand_color: value.brand_color,
        composer_icon: value.composer_icon,
        composer_icon_url: None,
        logo: value.logo,
        logo_dark: value.logo_dark,
        logo_url: None,
        logo_url_dark: None,
        screenshots: value.screenshots,
        screenshot_urls: Vec::new(),
    }
}
fn summary(plugin: ConfiguredMarketplacePlugin) -> wire::PluginSummary {
    wire::PluginSummary {
        id: plugin.id,
        local_version: plugin.local_version,
        name: plugin.name,
        source: source(plugin.source),
        installed: plugin.installed,
        enabled: plugin.enabled,
        install_policy: plugin.policy.installation.into(),
        install_policy_source: None,
        must_show_installation_interstitial: None,
        auth_policy: plugin.policy.authentication.into(),
        interface: plugin.interface.map(interface),
        keywords: plugin.keywords,
    }
}
fn marketplace(value: super::ConfiguredMarketplace) -> wire::PluginMarketplaceEntry {
    wire::PluginMarketplaceEntry {
        name: value.name,
        path: Some(value.path),
        interface: value.interface.map(|value| wire::MarketplaceInterface {
            display_name: value.display_name,
        }),
        plugins: value.plugins.into_iter().map(summary).collect(),
    }
}
fn skill_interface(value: &crate::skills::SkillInterface) -> wire::SkillInterface {
    wire::SkillInterface {
        display_name: value.display_name.clone(),
        short_description: value.short_description.clone(),
        icon_small: value.icon_small.clone(),
        icon_large: value.icon_large.clone(),
        icon_small_url: None,
        icon_large_url: None,
        brand_color: value.brand_color.clone(),
        default_prompt: value.default_prompt.clone(),
    }
}
fn skill_summary(value: &crate::skills::SkillMetadata, enabled: bool) -> wire::SkillSummary {
    wire::SkillSummary {
        name: value.name.clone(),
        description: value.description.clone(),
        short_description: value.short_description.clone(),
        interface: value.interface.as_ref().map(skill_interface),
        path: Some(value.path_to_skills_md.clone().into()),
        enabled,
    }
}
fn skill_metadata(value: &crate::skills::SkillMetadata, enabled: bool) -> wire::SkillMetadata {
    let scope = match value.scope {
        ra_tools::skills_ns::catalog::SkillScope::User => wire::SkillScope::User,
        ra_tools::skills_ns::catalog::SkillScope::Repo => wire::SkillScope::Repo,
        ra_tools::skills_ns::catalog::SkillScope::System => wire::SkillScope::System,
        _ => wire::SkillScope::Admin,
    };
    wire::SkillMetadata {
        name: value.name.clone(),
        description: value.description.clone(),
        short_description: value.short_description.clone(),
        interface: value.interface.as_ref().map(skill_interface),
        dependencies: value
            .dependencies
            .as_ref()
            .map(|dependencies| wire::SkillDependencies {
                tools: dependencies
                    .tools
                    .iter()
                    .map(|tool| wire::SkillToolDependency {
                        r#type: tool.r#type.clone(),
                        value: tool.value.clone(),
                        description: tool.description.clone(),
                        transport: tool.transport.clone(),
                        command: tool.command.clone(),
                        url: tool.url.clone(),
                    })
                    .collect(),
            }),
        path: value.path_to_skills_md.clone().into(),
        scope,
        enabled,
        plugin_id: value.plugin_id.clone(),
    }
}
fn detail(outcome: PluginReadOutcome) -> wire::PluginDetail {
    let plugin = outcome.plugin;
    let skills = plugin
        .skills
        .iter()
        .filter(|skill| skill.matches_product_restriction_for_product(Some(wire::Product::Codex)))
        .map(|skill| {
            skill_summary(
                skill,
                !plugin
                    .disabled_skill_paths
                    .contains(&skill.path_to_skills_md),
            )
        })
        .collect::<Vec<_>>();
    let onboarding_skill = plugin
        .onboarding_skill
        .as_ref()
        .filter(|_| plugin.enabled)
        .and_then(|path| {
            skills.iter().find(|skill| {
                skill.enabled
                    && skill
                        .path
                        .as_ref()
                        .and_then(ra_paths::LegacyAppPathString::to_inferred_path_uri)
                        .as_ref()
                        == Some(&ra_paths::PathUri::from_abs_path(path))
            })
        })
        .cloned();
    wire::PluginDetail {
        marketplace_name: outcome.marketplace_name,
        marketplace_path: outcome.marketplace_path,
        summary: wire::PluginSummary {
            id: plugin.id,
            local_version: plugin.local_version,
            name: plugin.name,
            source: source(plugin.source),
            installed: plugin.installed,
            enabled: plugin.enabled,
            install_policy: plugin.policy.installation.into(),
            install_policy_source: None,
            must_show_installation_interstitial: None,
            auth_policy: plugin.policy.authentication.into(),
            interface: plugin.interface.map(interface),
            keywords: plugin.keywords,
        },
        description: plugin.description,
        skills,
        onboarding_skill,
        hooks: plugin
            .hooks
            .into_iter()
            .map(|hook| wire::PluginHookSummary {
                key: hook.key,
                event_name: hook.event_name,
            })
            .collect(),
        mcp_servers: plugin.mcp_server_names,
        scheduled_tasks: None,
    }
}
fn hook_metadata(value: crate::hooks::HookListEntry) -> wire::HookMetadata {
    let handler = match value.handler {
        crate::hooks::HookListEntryHandler::Command { command, r#async } => {
            wire::HookHandlerMetadata::Command { command, r#async }
        }
        crate::hooks::HookListEntryHandler::McpTool { server, tool } => {
            wire::HookHandlerMetadata::McpTool { server, tool }
        }
    };
    wire::HookMetadata {
        key: value.key,
        event_name: value.event_name,
        handler,
        matcher: value.matcher,
        timeout_sec: value.timeout_sec,
        status_message: value.status_message,
        additional_context_limit: value.additional_context_limit,
        source_path: value.source_path,
        source: value.source,
        plugin_id: value.plugin_id,
        display_order: value.display_order,
        enabled: value.enabled,
        is_managed: value.is_managed,
        current_hash: value.current_hash,
        trust_status: value.trust_status,
    }
}

/// Features whose effective value `config/read` reports even when no layer sets them.
const SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT: &[&str] = &[
    "api_key_cyber_access_programs",
    "api_key_model_discovery",
    "auth_elicitation",
    "background_paginated_rollout_migration",
    "codex_apps_mcp_2026_07_28",
    "mcp_2026_07_28",
    "memories",
    "mentions_v2",
    "remote_control",
    "remote_plugin",
    "tool_suggest",
    "windows_sandbox_service",
];
fn config_error(err: crate::config_manager_service::ConfigManagerError) -> JsonRpcErrorObject {
    if let Some(code) = err.write_error_code() {
        return JsonRpcErrorObject::invalid_request(err.to_string())
            .with_data(serde_json::json!({"config_write_error_code":code}));
    }
    internal(err)
}
fn marketplace_error(
    err: super::marketplace::MarketplaceError,
    action: &str,
) -> JsonRpcErrorObject {
    use super::marketplace::MarketplaceError;
    match err {
        MarketplaceError::MarketplaceNotFound { .. }
        | MarketplaceError::InvalidMarketplaceFile { .. }
        | MarketplaceError::PluginNotFound { .. }
        | MarketplaceError::PluginNotAvailable { .. }
        | MarketplaceError::PluginsDisabled
        | MarketplaceError::InvalidPlugin(_) => invalid(err),
        MarketplaceError::Io { .. } => internal(format!("failed to {action}: {err}")),
    }
}
fn plugin_install_error(err: super::PluginInstallError) -> JsonRpcErrorObject {
    if err.is_invalid_request() {
        return invalid(err);
    }
    match err {
        super::PluginInstallError::Marketplace(err) => marketplace_error(err, "install plugin"),
        super::PluginInstallError::Config(err) => {
            internal(format!("failed to persist installed plugin config: {err}"))
        }
        super::PluginInstallError::Store(err) => {
            internal(format!("failed to install plugin: {err}"))
        }
        super::PluginInstallError::Join(err) => {
            internal(format!("failed to install plugin: {err}"))
        }
    }
}
fn plugin_uninstall_error(err: super::PluginUninstallError) -> JsonRpcErrorObject {
    if err.is_invalid_request() {
        return invalid(err);
    }
    match err {
        super::PluginUninstallError::Config(err) => {
            internal(format!("failed to clear plugin config: {err}"))
        }
        super::PluginUninstallError::Store(err) => {
            internal(format!("failed to uninstall plugin: {err}"))
        }
        super::PluginUninstallError::Join(err) => {
            internal(format!("failed to uninstall plugin: {err}"))
        }
        super::PluginUninstallError::InvalidPluginId(_) => {
            unreachable!("invalid plugin ids are handled above")
        }
    }
}
