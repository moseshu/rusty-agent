// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Runtime registrations built from the resolved product MCP catalog.
//!
//! The source connection set launches every enabled catalog server with its transport, timeouts
//! and tool filter, and reuses a live client when the server's connection identity is unchanged.
//! This module builds the same inputs for [`super::CodingMcpRuntime::replace_mcp_servers`]. The
//! identity keeps the source's non-account fields: auth mode, credential policy, transport,
//! environment, plugin root, the stdio fallback cwd and the values of referenced environment
//! variables. It is stored as a digest so secret values never live in the registry.
//!
//! Stored OAuth credentials, enterprise-managed authorization, HTTP header helpers and
//! executor-owned servers need runtime pieces that are not part of this product registry yet;
//! such servers are reported as unavailable rather than started without them.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ra_config::{McpServerAuth, McpServerConfig as ProductServerConfig, McpServerTransportConfig};
use ra_core::error::{Error, Result};
use ra_mcp::{
    McpCredentialPolicy, McpServerManagerOptions, McpServerSource, ResolvedMcpCatalog,
    ResolvedMcpServer,
    client::{
        McpClientOptions,
        http::{McpHttpParams, McpServerStreamableHttp},
        stdio::{McpServerStdio, McpStdioParams},
    },
    filter::ToolFilter,
};
use sha2::{Digest, Sha256};

use super::McpServerConfig;

/// Source default for a server's startup handshake.
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Source default for one tool call.
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// Certificate-bundle variables the source forwards to local stdio servers as absolute paths.
const CUSTOM_CA_ENV_KEYS: [&str; 11] = [
    "CODEX_CA_CERTIFICATE",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
    "CARGO_HTTP_CAINFO",
    "PIP_CERT",
    "BUNDLE_SSL_CA_CERT",
    "npm_config_cafile",
    "NPM_CONFIG_CAFILE",
];

/// The registrations to install and the catalog servers that could not be built.
pub struct ProductMcpServers {
    /// Runtime registrations in catalog name order.
    pub servers: Vec<McpServerConfig>,
    /// Servers that stay out of the registry, with the reason.
    pub unavailable: Vec<(String, String)>,
}

/// Builds runtime registrations for every winning catalog server.
///
/// `local_process_cwd` is the source's fallback cwd for local stdio servers that omit `cwd`.
#[must_use]
pub fn product_mcp_servers(
    catalog: &ResolvedMcpCatalog,
    local_process_cwd: &Path,
    http_client_factory: &ra_http::HttpClientFactory,
) -> ProductMcpServers {
    let mut servers = Vec::new();
    let mut unavailable = Vec::new();
    for (name, server) in catalog.servers() {
        match product_mcp_server(name, server, local_process_cwd, http_client_factory) {
            Ok(config) => servers.push(config),
            Err(error) => unavailable.push((name.to_owned(), error.to_string())),
        }
    }
    ProductMcpServers {
        servers,
        unavailable,
    }
}

fn product_mcp_server(
    name: &str,
    server: &ResolvedMcpServer,
    local_process_cwd: &Path,
    http_client_factory: &ra_http::HttpClientFactory,
) -> Result<McpServerConfig> {
    let config = server.config();
    if !config.is_local_environment() {
        return Err(Error::config(format!(
            "MCP server `{name}` belongs to execution environment `{}`, which this registry does not connect",
            config.environment_id
        )));
    }
    if matches!(config.auth, McpServerAuth::EmaAuth) {
        return Err(Error::config(format!(
            "MCP server `{name}` requires enterprise-managed authorization, which this registry does not provide"
        )));
    }
    let startup_timeout = config
        .startup_timeout_sec
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT);
    let tool_timeout = config.tool_timeout_sec.unwrap_or(DEFAULT_TOOL_TIMEOUT);
    let filter = tool_filter(config);
    // The registry applies the filter per run, so it can change without reconnecting.
    let options = McpClientOptions::default().with_tool_timeout(tool_timeout);
    let runtime_server: Arc<dyn super::McpRuntimeServer> = match &config.transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            env_vars,
            cwd,
        } => {
            if let Some(remote) = env_vars.iter().find(|var| var.is_remote_source()) {
                return Err(Error::config(format!(
                    "env_vars entry `{}` uses source `remote`, which requires remote MCP stdio",
                    remote.name()
                )));
            }
            let mut params = McpStdioParams::new(command.clone())
                .with_args(args.iter().cloned())
                .with_env(stdio_env(env.as_ref(), env_vars)?);
            params = match cwd {
                Some(cwd) => params.with_cwd(PathBuf::from(cwd.as_str())),
                None => params.with_cwd(local_process_cwd.to_path_buf()),
            };
            Arc::new(
                McpServerStdio::new(params)
                    .with_name(name)
                    .with_options(options),
            )
        }
        McpServerTransportConfig::StreamableHttp {
            url,
            bearer_token_env_var,
            http_headers,
            env_http_headers,
            http_headers_helper,
        } => {
            if http_headers_helper.is_some() {
                return Err(Error::config(format!(
                    "MCP server `{name}` uses an HTTP header helper, which this registry does not run"
                )));
            }
            let mut params = McpHttpParams::new(url.clone())
                .with_route_factory(http_client_factory.clone())
                .with_headers(http_headers_with_env(
                    http_headers.as_ref(),
                    env_http_headers.as_ref(),
                ))
                // The source bounds connection setup by the startup timeout alone.
                .with_timeout(startup_timeout);
            if let Some(env_var) = bearer_token_env_var {
                params = params.with_bearer_token_env_var(env_var.clone());
            }
            Arc::new(
                McpServerStreamableHttp::new(params)
                    .with_name(name)
                    .with_options(options),
            )
        }
    };
    let lifecycle =
        McpServerManagerOptions::default().with_connect_timeout(Some(startup_timeout))?;
    Ok(McpServerConfig::new(runtime_server)
        .with_enabled(config.enabled)
        .with_filter(filter)
        .with_lifecycle_options(lifecycle)
        .with_connection_identity(connection_identity(server, local_process_cwd)))
}

fn tool_filter(config: &ProductServerConfig) -> ToolFilter {
    let enabled = config
        .enabled_tools
        .as_ref()
        .map(|tools| tools.iter().cloned().collect::<HashSet<_>>());
    let disabled = config
        .disabled_tools
        .iter()
        .flatten()
        .cloned()
        .collect::<HashSet<_>>();
    ToolFilter::new(enabled, disabled)
}

/// Variables explicitly forwarded by the config, absolute CA bundle paths, then literal values;
/// the stdio client adds its core allowlist underneath.
fn stdio_env(
    env: Option<&std::collections::HashMap<String, String>>,
    env_vars: &[ra_config::McpServerEnvVar],
) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for var in env_vars
        .iter()
        .map(ra_config::McpServerEnvVar::name)
        .filter(|name| !ra_config::shell_environment::is_non_inheritable_env_var(name))
    {
        if let Some(value) = std::env::var_os(var) {
            values.insert(var.to_owned(), value.to_string_lossy().into_owned());
        }
    }
    for name in CUSTOM_CA_ENV_KEYS {
        let Some(value) = std::env::var_os(name) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let value = std::path::absolute(value).map_err(|error| {
            Error::config("failed to resolve a CA bundle path").with_source(error)
        })?;
        if cfg!(windows) {
            values.retain(|key: &String, _| !key.eq_ignore_ascii_case(name));
        }
        values.insert(name.to_owned(), value.to_string_lossy().into_owned());
    }
    for (name, value) in env.into_iter().flatten() {
        if cfg!(windows)
            || CUSTOM_CA_ENV_KEYS
                .iter()
                .any(|ca_name| ca_name.eq_ignore_ascii_case(name))
        {
            values.retain(|key, _| !key.eq_ignore_ascii_case(name));
        }
        values.insert(name.clone(), value.clone());
    }
    values.retain(|name, _| !ra_config::shell_environment::is_non_inheritable_env_var(name));
    Ok(values)
}

/// Static headers, then non-empty values read from the named environment variables.
fn http_headers_with_env(
    http_headers: Option<&std::collections::HashMap<String, String>>,
    env_http_headers: Option<&std::collections::HashMap<String, String>>,
) -> BTreeMap<String, String> {
    let mut headers = http_headers
        .into_iter()
        .flatten()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    for (name, env_var) in env_http_headers.into_iter().flatten() {
        if let Ok(value) = std::env::var(env_var)
            && !value.trim().is_empty()
        {
            headers.insert(name.clone(), value);
        }
    }
    headers
}

/// A digest of the fields whose change requires a new connection in the source.
fn connection_identity(server: &ResolvedMcpServer, local_process_cwd: &Path) -> String {
    let config = server.config();
    let host_plugin_root = match server.source() {
        McpServerSource::Plugin(plugin) => plugin.host_root().map(ToString::to_string),
        McpServerSource::SelectedPlugin(_)
        | McpServerSource::Config
        | McpServerSource::Compatibility { .. }
        | McpServerSource::Extension { .. } => None,
    };
    let local_stdio_fallback_cwd = matches!(
        config.transport,
        McpServerTransportConfig::Stdio { cwd: None, .. }
            | McpServerTransportConfig::StreamableHttp {
                http_headers_helper: Some(_),
                ..
            }
    )
    .then(|| local_process_cwd.to_string_lossy().into_owned());
    let referenced_environment_variables = match server.credential_policy() {
        McpCredentialPolicy::HostFallbackAllowed => referenced_environment_variables(config),
        McpCredentialPolicy::ExecutorOnly => Vec::new(),
    };
    let identity = serde_json::json!({
        "auth": config.auth,
        "credential_policy": format!("{:?}", server.credential_policy()),
        "transport": config.transport,
        "environment_id": config.environment_id,
        "host_plugin_root": host_plugin_root,
        "local_stdio_fallback_cwd": local_stdio_fallback_cwd,
        "referenced_environment_variables": referenced_environment_variables,
        "agent_plugin": server.source().is_agent_plugin(),
    });
    let digest = Sha256::digest(identity.to_string().as_bytes());
    digest
        .iter()
        .fold(String::with_capacity(digest.len() * 2), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn referenced_environment_variables(config: &ProductServerConfig) -> Vec<(String, Option<String>)> {
    let mut names = match &config.transport {
        McpServerTransportConfig::Stdio { env_vars, .. } => env_vars
            .iter()
            .filter(|env_var| !env_var.is_remote_source())
            .map(|env_var| env_var.name().to_string())
            .collect::<Vec<_>>(),
        McpServerTransportConfig::StreamableHttp {
            bearer_token_env_var,
            env_http_headers,
            ..
        } => bearer_token_env_var
            .iter()
            .filter(|name| config.is_local_environment() || std::env::var_os(name).is_some())
            .chain(env_http_headers.iter().flat_map(|headers| headers.values()))
            .cloned()
            .collect(),
    };
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let value =
                std::env::var_os(&name).map(|value: OsString| value.to_string_lossy().into_owned());
            (name, value)
        })
        .collect()
}
