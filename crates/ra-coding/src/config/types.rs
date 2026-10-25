// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product plugin activation, server overlays and marketplace configuration values.
//!
//! Deserialization accepts explicitly supplied values without resolving configuration layers,
//! applying marketplace requirements, loading packages, or initializing MCP servers.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::mcp_types::{AppToolApproval, McpServerToolConfig};

/// Default activation settings for installed plugins.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PluginsDefaultConfig {
    /// Default activation policy for merged plugin configuration entries.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// Plugin activation and capability settings supplied by the product configuration.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PluginsConfigToml {
    /// Omission leaves activation to the merged entry. Explicit entry settings override this default.
    #[serde(default, rename = "_default", skip_serializing_if = "Option::is_none")]
    pub default: Option<PluginsDefaultConfig>,
    /// Per-plugin overrides keyed by their configured identity.
    #[serde(default, flatten)]
    pub plugins: HashMap<String, PluginConfig>,
}

impl PluginsConfigToml {
    /// Computes the default-policy part of plugin enablement.
    ///
    /// The loader applies `entry.enabled.unwrap_or(true) && policy.allows_plugin(id)`, where
    /// `entry` is the configured plugin entry after layer merging and `policy` is this value.
    /// Without `_default`, this method returns true even for an explicitly disabled entry;
    /// the first operand still disables it. With `_default`, an explicit entry setting
    /// overrides that default in this policy predicate.
    #[must_use]
    pub fn allows_plugin(&self, plugin_id: &str) -> bool {
        let Some(default) = &self.default else {
            return true;
        };
        self.plugins
            .get(plugin_id)
            .and_then(|plugin| plugin.enabled)
            .unwrap_or(default.enabled)
    }
}

/// Explicit activation and MCP policy settings for one plugin.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PluginConfig {
    /// Explicit activation override. Omission inherits the installed-plugin default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Policy overlays for servers contributed by the plugin; transports belong to its manifest.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_servers: HashMap<String, PluginMcpServerConfig>,
}

/// Host policy for a plugin-provided MCP server, excluding transport configuration.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PluginMcpServerConfig {
    /// Whether the server may initialize after the policy is applied by a loader.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Presence of a retired EMA overlay, which a loader must preserve as a server denial.
    #[serde(
        default,
        rename = "ema_auth",
        deserialize_with = "unsupported_plugin_ema_auth",
        skip_serializing_if = "std::ops::Not::not"
    )]
    #[schemars(skip)]
    pub has_unsupported_ema_auth: bool,
    /// Default approval mode unless a tool override exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_approval_mode: Option<AppToolApproval>,
    /// Explicit tool allowlist; an empty list differs from omission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// Explicit tool denylist, applied after the allowlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
    /// Per-tool approval and output settings.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tools: HashMap<String, McpServerToolConfig>,
}

impl Default for PluginMcpServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            has_unsupported_ema_auth: false,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            tools: HashMap::new(),
        }
    }
}

fn unsupported_plugin_ema_auth<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| true)
}

/// Marketplace origin and the last successfully activated revision supplied by configuration.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MarketplaceConfig {
    /// Last successful add or refresh time.
    #[serde(default)]
    pub last_updated: Option<String>,
    /// Git revision last successfully activated.
    #[serde(default)]
    pub last_revision: Option<String>,
    /// Source kind used to install this marketplace.
    #[serde(default)]
    pub source_type: Option<MarketplaceSourceType>,
    /// Source location used when the marketplace was added.
    #[serde(default)]
    pub source: Option<String>,
    /// Git reference to check out for a Git source.
    #[serde(default, rename = "ref")]
    pub ref_name: Option<String>,
    /// Sparse checkout paths for a Git source.
    #[serde(default)]
    pub sparse_paths: Option<Vec<String>>,
}

/// Source kind for the marketplace itself, distinct from a plugin package's source kind.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MarketplaceSourceType {
    /// A Git repository containing a marketplace catalog.
    Git,
    /// A local marketplace directory.
    Local,
}

fn default_enabled() -> bool {
    true
}
