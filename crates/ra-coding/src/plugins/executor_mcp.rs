// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! MCP declarations loaded through their bound executor filesystem.
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use super::PluginResourceLocator;
use super::ResolvedExecutorPlugin;
use super::ResolvedPlugin;
use super::ResolvedPluginLocation;
use super::model::PluginManifestMcpServers;
use ra_config::McpServerConfig;
use ra_executor::ExecutorFileSystem;
use ra_executor::ReadFileOptions;
use ra_mcp::plugin_config::parse_executor_plugin_mcp_config;
use ra_paths::PathUri;
use ra_paths::PathUriParseError;
use std::io;
use thiserror::Error;

const DEFAULT_MCP_CONFIG_FILE: &str = ".mcp.json";

/// Loads MCP declarations from resolved plugins.
#[derive(Clone, Copy, Debug, Default)]
pub struct PluginMcpProvider;

/// Failure to load a plugin's MCP declarations.
#[derive(Debug, Error)]
pub enum PluginMcpProviderError {
    #[error("failed to read MCP config for selected plugin `{plugin_id}` at `{path}`: {source}")]
    ReadConfig {
        plugin_id: String,
        path: PathUri,
        #[source]
        source: io::Error,
    },
    #[error(
        "failed to resolve MCP config path `{relative_path}` below selected plugin `{plugin_id}` at `{root}`: {source}"
    )]
    InvalidConfigPath {
        plugin_id: String,
        root: PathUri,
        relative_path: &'static str,
        #[source]
        source: PathUriParseError,
    },
    #[error("failed to parse MCP config for selected plugin `{plugin_id}` at `{path}`: {source}")]
    ParseConfig {
        plugin_id: String,
        path: PathUri,
        #[source]
        source: serde_json::Error,
    },
}

impl PluginMcpProvider {
    /// Returns MCP servers declared by `plugin`, bound to its environment.
    #[tracing::instrument(name = "mcp.plugin.servers.load", skip_all)]
    pub async fn load(
        &self,
        plugin: &ResolvedExecutorPlugin,
    ) -> Result<Vec<(String, McpServerConfig)>, PluginMcpProviderError> {
        let ResolvedPluginLocation::Environment { root, .. } = plugin.plugin().location();

        load_from_file_system(plugin.plugin(), root, plugin.file_system()).await
    }
}

async fn load_from_file_system(
    plugin: &ResolvedPlugin,
    plugin_root: &PathUri,
    file_system: &dyn ExecutorFileSystem,
) -> Result<Vec<(String, McpServerConfig)>, PluginMcpProviderError> {
    let ResolvedPluginLocation::Environment { environment_id, .. } = plugin.location();
    let plugin_id = plugin.selected_root_id();
    let (contents, config_path) = match plugin.manifest().paths.mcp_servers.as_ref() {
        Some(PluginManifestMcpServers::Path(PluginResourceLocator::Environment {
            path, ..
        })) => {
            (
                file_system
                    .read_file_text(path, ReadFileOptions::default(), /*sandbox*/ None)
                    .await
                    .map_err(|source| PluginMcpProviderError::ReadConfig {
                        plugin_id: plugin_id.to_string(),
                        path: path.clone(),
                        source,
                    })?,
                path.clone(),
            )
        }
        Some(PluginManifestMcpServers::Object(object_config)) => {
            let PluginResourceLocator::Environment { path, .. } = plugin.manifest_path();
            (object_config.clone(), path.clone())
        }
        None => {
            let config_path = plugin_root
                .join(DEFAULT_MCP_CONFIG_FILE)
                .map_err(|source| PluginMcpProviderError::InvalidConfigPath {
                    plugin_id: plugin_id.to_string(),
                    root: plugin_root.clone(),
                    relative_path: DEFAULT_MCP_CONFIG_FILE,
                    source,
                })?;
            let contents = match file_system
                .read_file_text(
                    &config_path,
                    ReadFileOptions::default(),
                    /*sandbox*/ None,
                )
                .await
            {
                Ok(contents) => contents,
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(source) => {
                    return Err(PluginMcpProviderError::ReadConfig {
                        plugin_id: plugin_id.to_string(),
                        path: config_path.clone(),
                        source,
                    });
                }
            };
            (contents, config_path)
        }
    };
    let parsed = parse_executor_plugin_mcp_config(plugin_root, &contents, environment_id).map_err(
        |source| PluginMcpProviderError::ParseConfig {
            plugin_id: plugin_id.to_string(),
            path: config_path,
            source,
        },
    )?;

    for error in parsed.errors {
        tracing::warn!(
            plugin = plugin_id,
            server = error.name,
            error = error.message,
            "ignoring invalid plugin MCP server"
        );
    }

    Ok(parsed.servers.into_iter().collect())
}

/// Original loader internals exposed only to integration tests.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use super::{
        ExecutorFileSystem, McpServerConfig, PathUri, PluginMcpProviderError, ResolvedPlugin,
    };
    pub const DEFAULT_MCP_CONFIG_FILE: &str = super::DEFAULT_MCP_CONFIG_FILE;
    pub async fn load_from_file_system(
        plugin: &ResolvedPlugin,
        root: &PathUri,
        file_system: &dyn ExecutorFileSystem,
    ) -> Result<Vec<(String, McpServerConfig)>, PluginMcpProviderError> {
        super::load_from_file_system(plugin, root, file_system).await
    }
}
