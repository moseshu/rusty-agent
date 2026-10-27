// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! MCP server configuration types.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::time::Duration;

use super::RedactedString;
use super::ToolExposureSurface;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ra_paths::LegacyAppPathString;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as SerdeError;

use super::McpEmaRegistration;
use super::RequirementSource;

/// Effective MCP environment id when config omits `environment_id`.
pub const DEFAULT_MCP_SERVER_ENVIRONMENT_ID: &str = "local";

/// Approval policy carried by product MCP configuration.
///
/// This is distinct from the Agents SDK's server-owned `require_approval` contract.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppToolApproval {
    /// Apply the product's automatic approval policy.
    #[default]
    Auto,
    /// Request approval for every tool call.
    Prompt,
    /// Request approval for tools that write.
    Writes,
    /// Approve without requesting additional approval from this policy.
    Approve,
}

impl AppToolApproval {
    /// Requires approval whenever either policy could require it.
    ///
    /// `Auto` and `Writes` are incomparable; their conservative intersection is `Prompt`.
    #[must_use]
    pub fn restrict_to(self, requested: Self) -> Self {
        match (self, requested) {
            (Self::Prompt, _)
            | (_, Self::Prompt)
            | (Self::Auto, Self::Writes)
            | (Self::Writes, Self::Auto) => Self::Prompt,
            (Self::Approve, mode) | (mode, Self::Approve) => mode,
            (Self::Auto, Self::Auto) => Self::Auto,
            (Self::Writes, Self::Writes) => Self::Writes,
        }
    }
}

/// Human-readable reason a configured MCP server was disabled after requirements
/// were applied.
///
/// `Display` is intentionally implemented for CLI/TUI status output; avoid
/// relying on `Debug` because enum variant syntax is not part of the user-facing
/// message contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerDisabledReason {
    /// The server is disabled, but there is no more specific user-facing reason.
    Unknown,
    /// The server was disabled by config requirements from the given source.
    Requirements {
        /// The authority that disabled this server.
        source: RequirementSource,
    },
}

impl fmt::Display for McpServerDisabledReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpServerDisabledReason::Unknown => write!(f, "unknown"),
            McpServerDisabledReason::Requirements { source } => {
                write!(f, "requirements ({source})")
            }
        }
    }
}

/// Per-tool settings for a single product MCP server tool.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct McpServerToolConfig {
    /// Approval mode for this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<AppToolApproval>,
    /// Output token budget before the standard serialization allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_limit: Option<NonZeroUsize>,
}

impl McpServerToolConfig {
    /// Applies the stricter explicit output budget without changing approval policy.
    pub fn restrict_output_token_limit(&mut self, limit: Option<NonZeroUsize>) {
        self.output_token_limit = self.output_token_limit.into_iter().chain(limit).min();
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
/// An environment variable forwarded from local or remote execution context.
pub enum McpServerEnvVar {
    /// A legacy variable name with the default local source.
    Name(String),
    /// A variable name paired with an explicit source selector.
    Config {
        /// The environment variable name.
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// The optional `local` or `remote` source selector.
        source: Option<String>,
    },
}

impl McpServerEnvVar {
    /// Returns the environment variable name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            McpServerEnvVar::Name(name) | McpServerEnvVar::Config { name, .. } => name,
        }
    }

    /// Returns the explicit source selector, if any.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        match self {
            McpServerEnvVar::Name(_) => None,
            McpServerEnvVar::Config { source, .. } => source.as_deref(),
        }
    }

    /// Whether this variable is read from the remote executor environment.
    #[must_use]
    pub fn is_remote_source(&self) -> bool {
        self.source() == Some("remote")
    }

    /// Rejects source selectors other than `local` or `remote`.
    pub fn validate_source(&self) -> Result<(), String> {
        match self.source() {
            None | Some("local" | "remote") => Ok(()),
            Some(source) => Err(format!(
                "unsupported env_vars source `{source}`; expected `local` or `remote`"
            )),
        }
    }
}

impl From<String> for McpServerEnvVar {
    fn from(value: String) -> Self {
        Self::Name(value)
    }
}

impl From<&str> for McpServerEnvVar {
    fn from(value: &str) -> Self {
        Self::Name(value.to_string())
    }
}

impl AsRef<str> for McpServerEnvVar {
    fn as_ref(&self) -> &str {
        self.name()
    }
}

/// Client settings for MCP OAuth login or enterprise token exchange.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct McpServerOAuthConfig {
    /// Explicit OAuth client identifier to present during authorization and token exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// OAuth client secret used for token exchange with a pre-registered client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<RedactedString>,

    /// Registered callback URL associated with this OAuth client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,

    /// Fixed callback port that takes precedence over the product's global OAuth callback port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,

    /// Expected resource authorization server issuer for EMA token exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_server_issuer: Option<String>,

    /// Host-resolved authorization; never accepted from a server or plugin declaration.
    #[serde(skip)]
    #[schemars(skip)]
    pub ema_registration: Option<McpEmaRegistration>,
}

/// Authentication flow for an HTTP MCP server. Explicit credentials take
/// precedence for OAuth; EMA rejects alternate credentials and fallback.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpServerAuth {
    /// Use stored MCP OAuth credentials when available. Starting an OAuth login
    /// is a separate operation.
    #[default]
    #[serde(rename = "oauth")]
    OAuth,
    /// Exchange an enterprise `IdP` refresh token for resource-specific authorization.
    /// Alternate credentials and ordinary OAuth fallback are not permitted.
    #[serde(rename = "ema_auth")]
    EmaAuth,
}

/// Readiness needed before startup can expose this server's tools to the model.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpStartupReadiness {
    /// Use the live connection to determine startup readiness.
    #[default]
    Connection,
    /// Allow a valid cached tool catalog while the live connection starts.
    /// Tool execution still requires the current connection.
    Catalog,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

#[derive(Serialize, Debug, Clone, PartialEq)]
/// Validated product MCP configuration, independent of live SDK server ownership.
pub struct McpServerConfig {
    #[serde(flatten)]
    /// The validated stdio or streamable HTTP transport.
    pub transport: McpServerTransportConfig,

    /// Authentication flow, including an explicit no-fallback EMA mode.
    #[serde(default, skip_serializing_if = "is_default")]
    pub auth: McpServerAuth,

    /// Effective environment id for where the product should start this MCP server.
    pub environment_id: String,

    /// When `false`, the product skips initializing this MCP server.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// When `true`, the coding executor exits with an error if this MCP server fails to initialize.
    /// With `startup_readiness = "catalog"`, a valid cached catalog can satisfy startup;
    /// connection failures are then reported when a tool is invoked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,

    /// Whether startup requires a live connection or can use a valid cached tool catalog.
    #[serde(default, skip_serializing_if = "is_default")]
    pub startup_readiness: McpStartupReadiness,

    /// When `true`, every tool from this server is advertised as safe for parallel tool calls.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub supports_parallel_tool_calls: bool,

    /// UTF-8 byte threshold for compacting each ordinary MCP tool input schema. Defaults to 5,000 bytes.
    /// Code Mode also uses an explicitly configured limit when rendering each tool's input type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input_schema_max_bytes: Option<NonZeroUsize>,

    /// Model-facing surfaces from which this server's tools must be omitted.
    /// `None` leaves lower-priority configuration unchanged; an empty list clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omit_tools_from: Option<Vec<ToolExposureSurface>>,

    /// Reason this server was disabled after applying requirements.
    #[serde(skip)]
    pub disabled_reason: Option<McpServerDisabledReason>,

    /// Startup timeout in seconds for initializing MCP server & initially listing tools.
    #[serde(
        default,
        with = "option_duration_secs",
        skip_serializing_if = "Option::is_none"
    )]
    pub startup_timeout_sec: Option<Duration>,

    /// Default timeout for MCP tool calls initiated via this server.
    #[serde(default, with = "option_duration_secs")]
    pub tool_timeout_sec: Option<Duration>,

    /// Approval mode for tools in this server unless a tool override exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_approval_mode: Option<AppToolApproval>,

    /// Explicit allow-list of tools exposed from this server. When set, only these tools will be registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,

    /// Explicit deny-list of tools. These tools will be removed after applying `enabled_tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,

    /// Optional scopes requested during MCP login or EMA token exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,

    /// Optional client settings for MCP login or EMA token exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpServerOAuthConfig>,

    /// Optional resource parameter for MCP login or EMA token exchange (RFC 8707).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_resource: Option<String>,

    /// Per-tool settings keyed by tool name.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tools: HashMap<String, McpServerToolConfig>,
}

impl McpServerConfig {
    /// Whether this server belongs to the host-owned `local` environment.
    #[must_use]
    pub fn is_local_environment(&self) -> bool {
        self.environment_id == DEFAULT_MCP_SERVER_ENVIRONMENT_ID
    }

    /// Keeps local OAuth credentials compatible while reserving managed credential namespaces.
    #[must_use]
    pub fn oauth_credential_name<'a>(&self, server_name: &'a str) -> Cow<'a, str> {
        if self.is_local_environment() {
            if server_name.starts_with("executor:")
                || server_name.starts_with("local:")
                || server_name.starts_with("ema-idp:")
            {
                Cow::Owned(format!("local:{server_name}"))
            } else {
                Cow::Borrowed(server_name)
            }
        } else {
            let environment = URL_SAFE_NO_PAD.encode(self.environment_id.as_bytes());
            let server = URL_SAFE_NO_PAD.encode(server_name.as_bytes());
            Cow::Owned(format!("executor:{environment}:{server}"))
        }
    }

    /// Returns the explicitly configured MCP OAuth client identifier.
    #[must_use]
    pub fn oauth_client_id(&self) -> Option<&str> {
        self.oauth
            .as_ref()
            .and_then(|oauth| oauth.client_id.as_deref())
    }

    /// Returns the configured secret without changing its debug redaction.
    #[must_use]
    pub fn oauth_client_secret(&self) -> Option<&str> {
        self.oauth
            .as_ref()
            .and_then(|oauth| oauth.client_secret.as_ref())
            .map(|secret| secret.as_str())
    }

    /// Selects the per-server callback port before the global fallback.
    pub fn oauth_callback_port(&self, global_callback_port: Option<u16>) -> Option<u16> {
        let callback_port = self.oauth.as_ref().and_then(|oauth| oauth.callback_port);
        if let Some(callback_port) = callback_port {
            tracing::info!(
                callback_port,
                ?global_callback_port,
                "using plugin-specific MCP OAuth callback port instead of the global callback port"
            );
        }
        callback_port.or(global_callback_port)
    }
}

/// Raw MCP config shape used for deserialization and supported-field JSON
/// Schema generation.
///
/// Fields that are accepted only to produce targeted validation errors should
/// be skipped in the generated schema.
///
/// Keep `TryFrom<RawMcpServerConfig> for McpServerConfig` exhaustively
/// destructuring this struct so new TOML fields cannot be added here without
/// updating the validation/mapping logic that produces [`McpServerConfig`].
#[derive(Deserialize, Clone, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RawMcpServerConfig {
    // stdio
    /// The executable for a stdio server.
    pub command: Option<String>,
    #[serde(default)]
    /// Arguments supplied to the stdio executable.
    pub args: Option<Vec<String>>,
    #[serde(default)]
    /// Literal environment values supplied to the stdio process.
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    /// Environment variables forwarded from a selected execution context.
    pub env_vars: Option<Vec<McpServerEnvVar>>,
    #[serde(default)]
    /// Legacy path text preserved for resolution in the owning environment.
    pub cwd: Option<LegacyAppPathString>,
    /// Literal HTTP request headers.
    pub http_headers: Option<HashMap<String, String>>,
    #[serde(default)]
    /// HTTP headers populated from named environment variables.
    pub env_http_headers: Option<HashMap<String, String>>,

    // streamable_http
    /// The streamable HTTP endpoint.
    pub url: Option<String>,
    #[schemars(skip)]
    /// Rejected legacy inline bearer credentials, retained for a targeted error.
    pub bearer_token: Option<String>,
    /// The environment variable holding the HTTP bearer token.
    pub bearer_token_env_var: Option<String>,
    /// A local-only command producing dynamic HTTP headers as JSON.
    pub http_headers_helper: Option<String>,

    // shared
    #[serde(default)]
    /// The owning environment, defaulting to `local`.
    pub environment_id: Option<String>,
    #[serde(default)]
    /// The explicit MCP authentication flow.
    pub auth: Option<McpServerAuth>,
    #[serde(default)]
    /// Startup timeout in seconds, taking precedence over the legacy millisecond field.
    pub startup_timeout_sec: Option<f64>,
    #[serde(default)]
    /// Legacy startup timeout in milliseconds.
    pub startup_timeout_ms: Option<u64>,
    #[serde(default, with = "option_duration_secs")]
    #[schemars(with = "Option<f64>")]
    /// Default timeout for tool calls.
    pub tool_timeout_sec: Option<Duration>,
    #[serde(default)]
    /// Whether the server should initialize, defaulting to true.
    pub enabled: Option<bool>,
    #[serde(default)]
    /// Whether a startup failure fails the coding executor.
    pub required: Option<bool>,
    /// Whether startup requires a live connection or can use a valid cached tool catalog.
    #[serde(default)]
    /// The readiness condition required to expose tools.
    pub startup_readiness: Option<McpStartupReadiness>,
    #[serde(default)]
    /// Whether all server tools may run in parallel.
    pub supports_parallel_tool_calls: Option<bool>,
    /// UTF-8 byte threshold for compacting each ordinary MCP tool input schema. Defaults to 5,000 bytes.
    /// Code Mode also uses an explicitly configured limit when rendering each tool's input type.
    /// Larger limits preserve more parameter descriptions.
    #[serde(default)]
    #[schemars(range(min = 1))]
    /// Positive byte budget for model-facing input schemas.
    pub tool_input_schema_max_bytes: Option<NonZeroUsize>,
    #[serde(default)]
    /// Model-facing surfaces that must omit the server tools.
    pub omit_tools_from: Option<Vec<ToolExposureSurface>>,
    #[serde(default)]
    /// Default tool approval mode for this server.
    pub default_tools_approval_mode: Option<AppToolApproval>,
    #[serde(default)]
    /// An optional tool allow-list.
    pub enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    /// An optional tool deny-list.
    pub disabled_tools: Option<Vec<String>>,
    #[serde(default)]
    /// Scopes requested during MCP authorization.
    pub scopes: Option<Vec<String>>,
    #[serde(default)]
    /// Explicit MCP OAuth client settings.
    pub oauth: Option<McpServerOAuthConfig>,
    #[serde(default)]
    /// An optional resource authorization parameter.
    pub oauth_resource: Option<String>,
    /// Legacy display-name field accepted for backward compatibility.
    #[serde(default, rename = "name")]
    /// A legacy display name accepted without affecting registration identity.
    #[allow(
        clippy::pub_underscore_fields,
        reason = "Preserve the upstream raw compatibility field."
    )]
    pub _name: Option<String>,
    #[serde(default)]
    /// Per-tool approval and output budget overrides.
    pub tools: Option<HashMap<String, McpServerToolConfig>>,
}

impl TryFrom<RawMcpServerConfig> for McpServerConfig {
    type Error = String;

    #[allow(
        clippy::too_many_lines,
        clippy::items_after_statements,
        reason = "Keep exhaustive upstream transport validation together."
    )]
    fn try_from(raw: RawMcpServerConfig) -> Result<Self, Self::Error> {
        let RawMcpServerConfig {
            command,
            args,
            env,
            env_vars,
            cwd,
            http_headers,
            env_http_headers,
            url,
            bearer_token,
            bearer_token_env_var,
            http_headers_helper,
            environment_id,
            auth,
            startup_timeout_sec,
            startup_timeout_ms,
            tool_timeout_sec,
            enabled,
            required,
            startup_readiness,
            supports_parallel_tool_calls,
            tool_input_schema_max_bytes,
            omit_tools_from,
            default_tools_approval_mode,
            enabled_tools,
            disabled_tools,
            scopes,
            oauth,
            oauth_resource,
            _name: _,
            tools,
        } = raw;

        let startup_timeout_sec = match (startup_timeout_sec, startup_timeout_ms) {
            (Some(sec), _) => {
                Some(Duration::try_from_secs_f64(sec).map_err(|err| err.to_string())?)
            }
            (None, Some(ms)) => Some(Duration::from_millis(ms)),
            (None, None) => None,
        };

        fn throw_if_set<T>(transport: &str, field: &str, value: Option<&T>) -> Result<(), String> {
            if value.is_none() {
                return Ok(());
            }
            Err(format!("{field} is not supported for {transport}"))
        }

        let transport = if let Some(command) = command {
            throw_if_set("stdio", "url", url.as_ref())?;
            throw_if_set(
                "stdio",
                "bearer_token_env_var",
                bearer_token_env_var.as_ref(),
            )?;
            throw_if_set("stdio", "bearer_token", bearer_token.as_ref())?;
            throw_if_set("stdio", "http_headers_helper", http_headers_helper.as_ref())?;
            throw_if_set("stdio", "http_headers", http_headers.as_ref())?;
            throw_if_set("stdio", "env_http_headers", env_http_headers.as_ref())?;
            throw_if_set("stdio", "oauth", oauth.as_ref())?;
            throw_if_set("stdio", "oauth_resource", oauth_resource.as_ref())?;
            throw_if_set("stdio", "auth", auth.as_ref())?;
            let env_vars = env_vars.unwrap_or_default();
            for env_var in &env_vars {
                env_var.validate_source()?;
            }
            McpServerTransportConfig::Stdio {
                command,
                args: args.unwrap_or_default(),
                env,
                env_vars,
                cwd,
            }
        } else if let Some(url) = url {
            throw_if_set("streamable_http", "args", args.as_ref())?;
            throw_if_set("streamable_http", "env", env.as_ref())?;
            throw_if_set("streamable_http", "env_vars", env_vars.as_ref())?;
            throw_if_set("streamable_http", "cwd", cwd.as_ref())?;
            throw_if_set("streamable_http", "bearer_token", bearer_token.as_ref())?;
            if http_headers_helper
                .as_deref()
                .is_some_and(|command| command.trim().is_empty())
            {
                return Err("http_headers_helper must not be empty".to_string());
            }
            if environment_id
                .as_deref()
                .is_some_and(|environment_id| environment_id != DEFAULT_MCP_SERVER_ENVIRONMENT_ID)
                && http_headers_helper.is_some()
            {
                return Err(
                    "http_headers_helper is only supported for local MCP servers".to_string(),
                );
            }
            McpServerTransportConfig::StreamableHttp {
                url,
                bearer_token_env_var,
                http_headers,
                env_http_headers,
                http_headers_helper,
            }
        } else {
            return Err("invalid transport".to_string());
        };

        let environment_id =
            environment_id.unwrap_or_else(|| DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string());
        let auth = auth.unwrap_or_default();
        if let Some(oauth) = &oauth
            && let Some(client_secret) = &oauth.client_secret
        {
            if client_secret.trim().is_empty() {
                return Err("oauth.client_secret must not be empty".to_string());
            }
            if oauth
                .client_id
                .as_deref()
                .is_none_or(|client_id| client_id.trim().is_empty())
            {
                return Err("oauth.client_secret requires oauth.client_id".to_string());
            }
        }
        if !matches!(auth, McpServerAuth::EmaAuth)
            && oauth
                .as_ref()
                .is_some_and(|oauth| oauth.authorization_server_issuer.is_some())
        {
            return Err(
                "oauth.authorization_server_issuer requires auth = \"ema_auth\"".to_string(),
            );
        }

        Ok(Self {
            transport,
            auth,
            environment_id,
            startup_timeout_sec,
            tool_timeout_sec,
            enabled: enabled.unwrap_or_else(default_enabled),
            required: required.unwrap_or_default(),
            startup_readiness: startup_readiness.unwrap_or_default(),
            supports_parallel_tool_calls: supports_parallel_tool_calls.unwrap_or_default(),
            tool_input_schema_max_bytes,
            omit_tools_from,
            disabled_reason: None,
            default_tools_approval_mode,
            enabled_tools,
            disabled_tools,
            scopes,
            oauth,
            oauth_resource,
            tools: tools.unwrap_or_default(),
        })
    }
}

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        RawMcpServerConfig::deserialize(deserializer)?
            .try_into()
            .map_err(SerdeError::custom)
    }
}

const fn default_enabled() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(untagged, deny_unknown_fields, rename_all = "snake_case")]
/// Transport-specific MCP configuration with incompatible fields rejected.
pub enum McpServerTransportConfig {
    /// <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#stdio>
    Stdio {
        /// The executable for a stdio server.
        command: String,
        #[serde(default)]
        /// Arguments supplied to the stdio executable.
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Literal environment values supplied to the stdio process.
        env: Option<HashMap<String, String>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        /// Environment variables forwarded from a selected execution context.
        env_vars: Vec<McpServerEnvVar>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Legacy path text preserved for resolution in the owning environment.
        cwd: Option<LegacyAppPathString>,
    },
    /// <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#streamable-http>
    StreamableHttp {
        /// The streamable HTTP endpoint.
        url: String,
        /// Name of the environment variable to read for an HTTP bearer token.
        /// When set, requests will include the token via `Authorization: Bearer <token>`.
        /// The actual secret value must be provided via the environment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_token_env_var: Option<String>,
        /// Additional HTTP headers to include in requests to this server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_headers: Option<HashMap<String, String>>,
        /// HTTP headers where the value is sourced from an environment variable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env_http_headers: Option<HashMap<String, String>>,
        /// Local-only shell command that prints a JSON object of dynamic HTTP headers.
        /// The command may be visible to local process inspection; do not embed credentials.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_headers_helper: Option<String>,
    },
}

mod option_duration_secs {
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use std::time::Duration;

    #[allow(
        clippy::ref_option,
        reason = "Serde with-modules receive a reference to the field."
    )]
    pub(super) fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(duration) => serializer.serialize_some(&duration.as_secs_f64()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = Option::<f64>::deserialize(deserializer)?;
        secs.map(|secs| Duration::try_from_secs_f64(secs).map_err(serde::de::Error::custom))
            .transpose()
    }
}
