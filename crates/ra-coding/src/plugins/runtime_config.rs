// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The effective configuration consumed by explicitly selected plugin services.
//!
//! This projection uses the real product layer stack and managed constraints. It does not
//! install product configuration in the provider-neutral SDK or construct unrelated UI state.
#![allow(
    missing_docs,
    reason = "Retain source plugin-facing effective configuration fields."
)]
use super::PluginsConfigInput;
use ra_config::{
    ConfigLayerStack, ConfigToml, Constrained, ConstraintResult, Feature, FeatureConfigSource,
    FeatureOverrides, Features, ManagedFeatures, McpEnterpriseManagedAuthConfig, McpServerConfig,
    McpServerDisabledReason, McpServerRequirement, PluginsConfigToml, Sourced,
};
use ra_http::HttpClientFactory;
use ra_paths::AbsolutePathBuf;
use std::{
    collections::{BTreeMap, HashMap},
    io,
};

#[derive(Debug, Clone)]
pub struct PluginRuntimeConfig {
    pub config_layer_stack: ConfigLayerStack,
    pub features: ManagedFeatures,
    pub plugins: PluginsConfigToml,
    pub model_provider_id: String,
    pub chatgpt_base_url: String,
    pub mcp_servers: Constrained<HashMap<String, McpServerConfig>>,
    pub mcp_enterprise_managed_auth: Option<McpEnterpriseManagedAuthConfig>,
    pub startup_warnings: Vec<String>,
    pub legacy_notify_argv: Option<Vec<String>>,
    pub bypass_hook_trust: bool,
    pub cwd: AbsolutePathBuf,
    pub home: AbsolutePathBuf,
    pub http_client_factory: HttpClientFactory,
}
impl PluginRuntimeConfig {
    /// Derives the plugin-facing runtime from validated layers, including managed feature pins.
    pub fn from_layer_stack(
        config_layer_stack: ConfigLayerStack,
        home: AbsolutePathBuf,
        cwd: AbsolutePathBuf,
        http_client_factory: HttpClientFactory,
    ) -> io::Result<Self> {
        let cfg: ConfigToml = config_layer_stack
            .effective_config()
            .try_into()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if let Some(profile) = cfg.profile.as_deref() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "legacy `profile = \"{profile}\"` config is no longer supported; use `--profile {profile}` with `{profile}.config.toml` instead"
                ),
            ));
        }
        let mut startup_warnings = config_layer_stack
            .startup_warnings()
            .unwrap_or_default()
            .to_vec();
        let configured_features = Features::from_sources(
            FeatureConfigSource {
                features: cfg.features.as_ref(),
                experimental_use_unified_exec_tool: cfg.experimental_use_unified_exec_tool,
            },
            FeatureConfigSource::default(),
            FeatureOverrides::default(),
        );
        let features = ManagedFeatures::from_configured_with_warnings(
            configured_features,
            config_layer_stack
                .requirements()
                .feature_requirements
                .clone(),
            &mut startup_warnings,
        )?;
        let mcp_enterprise_managed_auth = McpEnterpriseManagedAuthConfig::resolve(
            &config_layer_stack,
            cfg.mcp_enterprise_managed_auth.as_ref(),
            &cfg.mcp_servers,
            features.enabled(Feature::UseXaa),
        )?;
        let mcp_servers = constrain_mcp_servers(
            cfg.mcp_servers,
            config_layer_stack.requirements().mcp_servers.as_ref(),
        )
        .map_err(io::Error::other)?;
        Ok(Self {
            config_layer_stack,
            features,
            plugins: cfg.plugins,
            chatgpt_base_url: cfg
                .chatgpt_base_url
                .unwrap_or_else(|| super::remote::DEFAULT_CHATGPT_BASE_URL.to_string()),
            model_provider_id: cfg.model_provider.unwrap_or_else(|| "openai".to_string()),
            mcp_servers,
            mcp_enterprise_managed_auth,
            startup_warnings,
            legacy_notify_argv: cfg.notify,
            bypass_hook_trust: false,
            cwd,
            home,
            http_client_factory,
        })
    }
    #[must_use]
    pub fn plugins_config_input(&self) -> PluginsConfigInput {
        let mut config = PluginsConfigInput::new(
            self.config_layer_stack.clone(),
            self.plugins.clone(),
            self.model_provider_id.clone(),
            self.features.enabled(Feature::Plugins),
            self.http_client_factory.clone(),
        );
        config.chatgpt_base_url.clone_from(&self.chatgpt_base_url);
        config
    }
    #[must_use]
    pub fn hooks_config(
        &self,
        plugins: &super::PluginLoadOutcome<McpServerConfig>,
    ) -> crate::hooks::HooksConfig {
        crate::hooks::HooksConfig {
            legacy_notify_argv: self.legacy_notify_argv.clone(),
            feature_enabled: self.features.enabled(Feature::CodexHooks),
            bypass_hook_trust: self.bypass_hook_trust,
            config_layer_stack: Some(self.config_layer_stack.clone()),
            plugin_hook_sources: plugins.effective_plugin_hook_sources(),
            plugin_hook_load_warnings: plugins.effective_plugin_hook_warnings(),
            ..Default::default()
        }
    }
    /// Constructs the source-resolved immutable catalog before connections are opened.
    #[must_use]
    pub fn mcp_catalog(
        &self,
        plugins: &super::PluginLoadOutcome<McpServerConfig>,
    ) -> ra_mcp::ResolvedMcpCatalog {
        let mut catalog = ra_mcp::ResolvedMcpCatalog::builder();
        if self.features.enabled(Feature::UseXaa)
            && let Some(auth) = &self.mcp_enterprise_managed_auth
        {
            catalog.enable_ema(auth.idp.clone());
        }
        for (order, plugin) in plugins
            .plugins()
            .iter()
            .filter(|p| p.is_active())
            .enumerate()
        {
            let attribution = if plugin.is_agent_plugin() {
                ra_mcp::McpPluginAttribution::agent_plugin(
                    plugin.config_name.clone(),
                    plugin.display_name().to_string(),
                )
            } else {
                ra_mcp::McpPluginAttribution::new(
                    plugin.config_name.clone(),
                    plugin.display_name().to_string(),
                )
            }
            .with_host_root(ra_paths::PathUri::from_abs_path(&plugin.root));
            for (name, server) in &plugin.mcp_servers {
                catalog.register(ra_mcp::McpServerRegistration::from_plugin(
                    name.clone(),
                    attribution.clone(),
                    order,
                    server.clone(),
                ));
            }
        }
        for (name, server) in self.mcp_servers.get() {
            catalog.register(ra_mcp::McpServerRegistration::from_config(
                name.clone(),
                server.clone(),
            ));
        }
        catalog.build()
    }
}

fn filter_mcp_servers_by_requirements(
    mcp_servers: &mut HashMap<String, McpServerConfig>,
    mcp_requirements: Option<&Sourced<BTreeMap<String, McpServerRequirement>>>,
) {
    let Some(allowlist) = mcp_requirements else {
        return;
    };

    let source = allowlist.source.clone();
    for (name, server) in mcp_servers.iter_mut() {
        let allowed = allowlist
            .value
            .get(name)
            .is_some_and(|requirement| server.matches_requirement(requirement));
        if allowed {
            server.disabled_reason = None;
        } else {
            server.enabled = false;
            server.disabled_reason = Some(McpServerDisabledReason::Requirements {
                source: source.clone(),
            });
        }
    }
}

fn constrain_mcp_servers(
    mcp_servers: HashMap<String, McpServerConfig>,
    mcp_requirements: Option<&Sourced<BTreeMap<String, McpServerRequirement>>>,
) -> ConstraintResult<Constrained<HashMap<String, McpServerConfig>>> {
    if mcp_requirements.is_none() {
        return Ok(Constrained::allow_any(mcp_servers));
    }

    let mcp_requirements = mcp_requirements.cloned();
    Constrained::normalized(mcp_servers, move |mut servers| {
        filter_mcp_servers_by_requirements(&mut servers, mcp_requirements.as_ref());
        servers
    })
}
