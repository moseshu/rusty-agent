// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product provider descriptions and source validation; runtime adapters own provider creation.
#![allow(
    missing_docs,
    reason = "Retain the source provider schema and validation surface."
)]
use crate::{GatewayOAuthConfig, ModelProviderAuthInfo, ModelProviderCapabilities, RedactedString};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    num::NonZeroU64,
    path::{Component, Path},
    time::Duration,
};
const CHAT_WIRE_API_REMOVED_ERROR: &str = "`wire_api = \"chat\"` is no longer supported.\nHow to fix: set `wire_api = \"responses\"` in your provider config.\nMore info: https://github.com/openai/codex/discussions/7782";
const DEFAULT_AWS_CREDENTIAL_EXPORT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_AWS_AUTH_REFRESH_TIMEOUT_MS: u64 = 300_000;
/// Wire protocol that the provider speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    /// The Responses API exposed by `OpenAI` at `/v1/responses`.
    #[default]
    Responses,
}

impl fmt::Display for WireApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Responses => "responses",
        };
        f.write_str(value)
    }
}

impl<'de> Deserialize<'de> for WireApi {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "responses" => Ok(Self::Responses),
            "chat" => Err(serde::de::Error::custom(CHAT_WIRE_API_REMOVED_ERROR)),
            _ => Err(serde::de::Error::unknown_variant(&value, &["responses"])),
        }
    }
}

/// Serializable representation of a provider definition.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderInfo {
    /// Friendly display name.
    #[serde(default)]
    pub name: String,
    /// Base URL for the provider's OpenAI-compatible API.
    pub base_url: Option<String>,
    /// Optional full URL for a Codex-native model catalog. When unset, `OpenAI` discovery
    /// uses the Codex backend unless `base_url` overrides the inference endpoint.
    pub model_catalog_url: Option<RedactedString>,
    /// Environment variable that stores the user's API key for this provider.
    pub env_key: Option<String>,

    /// Optional instructions to help the user get a valid value for the
    /// variable and set it.
    pub env_key_instructions: Option<String>,
    /// Value to use with `Authorization: Bearer <token>` header. Use of this
    /// config is discouraged in favor of `env_key` for security reasons, but
    /// this may be necessary when using this programmatically.
    pub experimental_bearer_token: Option<RedactedString>,
    /// Command-backed bearer-token configuration for this provider.
    pub auth: Option<ModelProviderAuthInfo>,
    /// Secondary OAuth credentials required by the provider's gateway.
    pub gateway_oauth: Option<GatewayOAuthConfig>,
    /// AWS `SigV4` auth configuration for this provider.
    pub aws: Option<ModelProviderAwsAuthInfo>,
    /// Which wire protocol this provider expects.
    #[serde(default)]
    pub wire_api: WireApi,
    /// Optional API capability overrides for a custom Responses-compatible provider.
    /// Unspecified capabilities retain their existing provider defaults.
    pub capabilities: Option<ModelProviderCapabilities>,
    /// Optional query parameters to append to the base URL.
    pub query_params: Option<HashMap<String, RedactedString>>,
    /// Additional HTTP headers to include in requests to this provider where
    /// the (key, value) pairs are the header name and value.
    pub http_headers: Option<HashMap<String, RedactedString>>,
    /// Optional HTTP headers to include in requests to this provider where the
    /// (key, value) pairs are the header name and _environment variable_ whose
    /// value should be used. If the environment variable is not set, or the
    /// value is empty, the header will not be included in the request.
    pub env_http_headers: Option<HashMap<String, String>>,
    /// Maximum number of times to retry a failed HTTP request to this provider.
    pub request_max_retries: Option<u64>,
    /// Number of times to retry reconnecting a dropped streaming response before failing.
    pub stream_max_retries: Option<u64>,
    /// Idle timeout (in milliseconds) to wait for activity on a streaming response before treating
    /// the connection as lost.
    pub stream_idle_timeout_ms: Option<u64>,
    /// Maximum time (in milliseconds) to wait for a websocket connection attempt before treating
    /// it as failed.
    pub websocket_connect_timeout_ms: Option<u64>,
    /// Whether this provider supports the Responses API WebSocket transport.
    #[serde(default)]
    pub supports_websockets: bool,
    /// Whether this provider supports the standalone web-search endpoint.
    #[serde(default)]
    pub supports_standalone_web_search: bool,
}

/// AWS `SigV4` auth configuration for a model provider.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderAwsAuthInfo {
    /// AWS profile name to use. When unset, the AWS SDK default chain decides.
    pub profile: Option<String>,
    /// AWS region to use for provider-specific endpoints.
    pub region: Option<String>,
    /// Optional command whose exported credentials replace the AWS SDK credential chain.
    pub credential_export: Option<AwsCredentialExportConfig>,
    /// Optional command used to reauthenticate after a refreshable AWS auth failure.
    pub auth_refresh: Option<AwsAuthRefreshConfig>,
}

/// Command used to export AWS signing credentials for a model provider.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AwsCredentialExportConfig {
    /// Executable to invoke directly, without a shell.
    pub command: String,
    /// Arguments passed to the credential export command.
    #[serde(default)]
    pub args: Vec<RedactedString>,
    /// Maximum time to wait for the credential export command to complete.
    #[serde(default = "default_aws_credential_export_timeout_ms")]
    pub timeout_ms: NonZeroU64,
}

impl AwsCredentialExportConfig {
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.get())
    }
}

fn default_aws_credential_export_timeout_ms() -> NonZeroU64 {
    match NonZeroU64::new(DEFAULT_AWS_CREDENTIAL_EXPORT_TIMEOUT_MS) {
        Some(timeout_ms) => timeout_ms,
        None => panic!("AWS credential export timeout must be non-zero"),
    }
}

/// Command used to refresh AWS credentials for a model provider.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AwsAuthRefreshConfig {
    /// Executable to invoke directly, without a shell.
    pub command: String,
    /// Arguments passed to the refresh command.
    #[serde(default)]
    pub args: Vec<RedactedString>,
    /// Maximum time to wait for the refresh command to complete.
    #[serde(default = "default_aws_auth_refresh_timeout_ms")]
    pub timeout_ms: NonZeroU64,
}

impl AwsAuthRefreshConfig {
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.get())
    }
}

fn default_aws_auth_refresh_timeout_ms() -> NonZeroU64 {
    match NonZeroU64::new(DEFAULT_AWS_AUTH_REFRESH_TIMEOUT_MS) {
        Some(timeout_ms) => timeout_ms,
        None => panic!("AWS auth refresh timeout must be non-zero"),
    }
}

impl ModelProviderInfo {
    /// Checks that a configured Bedrock entry only customizes supported fields.
    /// Call this on the override before merging it with the built-in provider.
    pub fn validate_bedrock_override(&self) -> Result<(), String> {
        let unsupported_fields = Self {
            base_url: None,
            auth: None,
            aws: None,
            http_headers: None,
            ..self.clone()
        };
        if unsupported_fields != Self::default() {
            return Err("only supports changing \
`base_url`, `auth`, `http_headers`, `aws.profile`, `aws.region`, `aws.credential_export`, \
and `aws.auth_refresh`; \
other non-default provider fields are not supported"
                .to_string());
        }
        Ok(())
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if let Some(gateway) = &self.gateway_oauth {
            gateway.validate(self)?;
        }
        if let Some(aws) = self.aws.as_ref() {
            if self.supports_websockets {
                // AWS SigV4 signing is unavailable for WebSocket
                // upgrade requests before allowing AWS-authenticated providers
                // to enable Responses-over-WebSocket.
                return Err("provider aws cannot be combined with supports_websockets".to_string());
            }

            let mut conflicts = Vec::new();
            if self.env_key.is_some() {
                conflicts.push("env_key");
            }
            if self.experimental_bearer_token.is_some() {
                conflicts.push("experimental_bearer_token");
            }
            if self.auth.is_some() {
                conflicts.push("auth");
            }

            if !conflicts.is_empty() {
                return Err(format!(
                    "provider aws cannot be combined with {}",
                    conflicts.join(", ")
                ));
            }

            if let Some(credential_export) = aws.credential_export.as_ref() {
                if aws.profile.is_some() {
                    return Err(
                        "provider aws.credential_export cannot be combined with aws.profile"
                            .to_string(),
                    );
                }
                if credential_export.command.trim().is_empty() {
                    return Err(
                        "provider aws.credential_export.command must not be empty".to_string()
                    );
                }
                let command = Path::new(&credential_export.command);
                let mut components = command.components();
                let is_bare_command = matches!(
                    (components.next(), components.next()),
                    (Some(Component::Normal(name)), None) if name == command.as_os_str()
                );
                if !command.is_absolute() && !is_bare_command {
                    return Err(
                        "provider aws.credential_export.command must be an absolute path or a bare executable name"
                            .to_string(),
                    );
                }
            }

            if let Some(auth_refresh) = aws.auth_refresh.as_ref() {
                if auth_refresh.command.trim().is_empty() {
                    return Err("provider aws.auth_refresh.command must not be empty".to_string());
                }
                if auth_refresh.command != "aws" {
                    return Err("provider aws.auth_refresh.command must be `aws`".to_string());
                }
            }
        }

        let Some(auth) = self.auth.as_ref() else {
            return Ok(());
        };

        if auth.command.trim().is_empty() {
            return Err("provider auth.command must not be empty".to_string());
        }

        let mut conflicts = Vec::new();
        if self.env_key.is_some() {
            conflicts.push("env_key");
        }
        if self.experimental_bearer_token.is_some() {
            conflicts.push("experimental_bearer_token");
        }

        if conflicts.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "provider auth cannot be combined with {}",
                conflicts.join(", ")
            ))
        }
    }
}

impl ModelProviderInfo {
    /// Whether this is the managed Bedrock provider description.
    #[must_use]
    pub fn is_amazon_bedrock(&self) -> bool {
        self.name == "Amazon Bedrock" || self.name == "Amazon Bedrock Runtime"
    }
    /// Whether this is the managed Bedrock Runtime provider description.
    #[must_use]
    pub fn is_amazon_bedrock_runtime(&self) -> bool {
        self.name == "Amazon Bedrock Runtime"
    }
}

pub const AMAZON_BEDROCK_PROVIDER_ID: &str = "amazon-bedrock";

pub const AMAZON_BEDROCK_RUNTIME_PROVIDER_ID: &str = "amazon-bedrock-runtime";

pub const LEGACY_OLLAMA_CHAT_PROVIDER_ID: &str = "ollama-chat";

pub const LMSTUDIO_OSS_PROVIDER_ID: &str = "lmstudio";

pub const OLLAMA_CHAT_PROVIDER_REMOVED_ERROR: &str = "`ollama-chat` is no longer supported.\nHow to fix: replace `ollama-chat` with `ollama` in `model_provider`, `oss_provider`, or `--local-provider`.\nMore info: https://github.com/openai/codex/discussions/7782";

pub const OLLAMA_OSS_PROVIDER_ID: &str = "ollama";

pub const OPENAI_PROVIDER_ID: &str = "openai";

/// Managed residency restriction.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResidencyRequirement {
    /// United States residency.
    Us,
}
