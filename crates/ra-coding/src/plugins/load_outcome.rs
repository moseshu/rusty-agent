// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Retain source skill metadata, loading and snapshot contracts."
)]
use std::collections::HashMap;
use std::collections::HashSet;

use crate::plugins::PluginIdentity;
use crate::plugins::PluginSkillRoot;
use crate::plugins::SkillDiscoveryMode;
use ra_paths::AbsolutePathBuf;
use ra_paths::PathUri;

use crate::plugins::AppConnectorId;
use crate::plugins::AppDeclaration;
use crate::plugins::PluginCapabilitySummary;
use crate::plugins::PluginHookSource;
use crate::plugins::app_connector_ids_from_declarations;

const MAX_CAPABILITY_SUMMARY_DESCRIPTION_LEN: usize = 1024;

/// A plugin that was loaded from disk, including merged MCP server definitions.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPlugin<M> {
    pub config_name: String,
    pub manifest_name: Option<String>,
    pub plugin_namespace: Option<String>,
    pub manifest_description: Option<String>,
    pub root: AbsolutePathBuf,
    pub enabled: bool,
    pub skill_roots: Vec<AbsolutePathBuf>,
    pub skill_discovery_mode: SkillDiscoveryMode,
    pub disabled_skill_paths: HashSet<PathUri>,
    pub has_enabled_skills: bool,
    pub mcp_servers: HashMap<String, M>,
    pub apps: Vec<AppDeclaration>,
    pub hook_sources: Vec<PluginHookSource>,
    pub hook_load_warnings: Vec<String>,
    pub error: Option<String>,
}

impl<M> LoadedPlugin<M> {
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.error.is_none()
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        self.manifest_name.as_deref().unwrap_or(&self.config_name)
    }

    #[must_use]
    pub fn is_agent_plugin(&self) -> bool {
        self.skill_discovery_mode == SkillDiscoveryMode::DirectChildren
    }
}

fn plugin_capability_summary_from_loaded<M>(
    plugin: &LoadedPlugin<M>,
) -> Option<PluginCapabilitySummary> {
    if !plugin.is_active() {
        return None;
    }

    let mut mcp_server_names: Vec<String> = plugin.mcp_servers.keys().cloned().collect();
    mcp_server_names.sort_unstable();

    let summary = PluginCapabilitySummary {
        config_name: plugin.config_name.clone(),
        display_name: plugin.display_name().to_string(),
        plugin_namespace: plugin.plugin_namespace.clone(),
        description: prompt_safe_plugin_description(plugin.manifest_description.as_deref()),
        has_skills: plugin.has_enabled_skills,
        mcp_server_names,
        app_connector_ids: app_connector_ids_from_declarations(&plugin.apps),
    };

    (summary.has_skills
        || !summary.mcp_server_names.is_empty()
        || !summary.app_connector_ids.is_empty())
    .then_some(summary)
}

/// Normalizes plugin descriptions for inclusion in model-facing capability summaries.
#[must_use]
pub fn prompt_safe_plugin_description(description: Option<&str>) -> Option<String> {
    let description = description?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if description.is_empty() {
        return None;
    }

    Some(
        description
            .chars()
            .take(MAX_CAPABILITY_SUMMARY_DESCRIPTION_LEN)
            .collect(),
    )
}

/// Runtime view of loaded plugins and their derived capability summaries.
///
/// Runtime exclusions retain loaded metadata while removing derived capabilities.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginLoadOutcome<M> {
    plugins: Vec<LoadedPlugin<M>>,
    capability_summaries: Vec<PluginCapabilitySummary>,
}

impl<M: Clone> Default for PluginLoadOutcome<M> {
    fn default() -> Self {
        Self::from_plugins(Vec::new())
    }
}

impl<M: Clone> PluginLoadOutcome<M> {
    pub fn from_plugins(plugins: Vec<LoadedPlugin<M>>) -> Self {
        let capability_summaries = plugins
            .iter()
            .filter_map(plugin_capability_summary_from_loaded)
            .collect::<Vec<_>>();
        Self {
            plugins,
            capability_summaries,
        }
    }

    /// Marks matching canonical plugin IDs inactive while retaining their loaded metadata.
    #[must_use]
    pub fn without_plugins(mut self, disabled_plugin_ids: &[String]) -> Self {
        if disabled_plugin_ids.is_empty() {
            return self;
        }
        for plugin in &mut self.plugins {
            if disabled_plugin_ids.contains(&plugin.config_name) {
                plugin.enabled = false;
            }
        }
        Self::from_plugins(self.plugins)
    }

    #[must_use]
    pub fn effective_plugin_skill_roots(&self) -> Vec<PluginSkillRoot> {
        let mut skill_roots = Vec::new();
        let mut seen_paths = HashSet::new();
        for plugin in self.plugins.iter().filter(|plugin| plugin.is_active()) {
            let Some(plugin_namespace) = &plugin.plugin_namespace else {
                continue;
            };
            for path in &plugin.skill_roots {
                if seen_paths.insert(path.clone()) {
                    skill_roots.push(PluginSkillRoot {
                        path: path.clone(),
                        plugin_identity: PluginIdentity {
                            plugin_id: plugin.config_name.clone(),
                        },
                        plugin_namespace: plugin_namespace.clone(),
                        plugin_root: plugin.root.clone(),
                        discovery_mode: plugin.skill_discovery_mode,
                    });
                }
            }
        }

        skill_roots.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        skill_roots
    }

    #[must_use]
    pub fn effective_mcp_servers(&self) -> HashMap<String, M> {
        let mut mcp_servers = HashMap::new();
        for plugin in self.plugins.iter().filter(|plugin| plugin.is_active()) {
            for (name, config) in &plugin.mcp_servers {
                mcp_servers
                    .entry(name.clone())
                    .or_insert_with(|| config.clone());
            }
        }
        mcp_servers
    }

    #[must_use]
    pub fn effective_apps(&self) -> Vec<AppConnectorId> {
        app_connector_ids_from_declarations(
            self.plugins
                .iter()
                .filter(|plugin| plugin.is_active())
                .flat_map(|plugin| plugin.apps.iter()),
        )
    }

    #[must_use]
    pub fn effective_plugin_hook_sources(&self) -> Vec<PluginHookSource> {
        self.iter_effective_plugin_hook_sources().cloned().collect()
    }

    pub fn iter_effective_plugin_hook_sources(&self) -> impl Iterator<Item = &PluginHookSource> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_sources.iter())
    }

    #[must_use]
    pub fn effective_plugin_hook_warnings(&self) -> Vec<String> {
        self.iter_effective_plugin_hook_warnings()
            .cloned()
            .collect()
    }

    pub fn iter_effective_plugin_hook_warnings(&self) -> impl Iterator<Item = &String> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_load_warnings.iter())
    }

    #[must_use]
    pub fn capability_summaries(&self) -> &[PluginCapabilitySummary] {
        &self.capability_summaries
    }

    #[must_use]
    pub fn plugins(&self) -> &[LoadedPlugin<M>] {
        &self.plugins
    }
}
