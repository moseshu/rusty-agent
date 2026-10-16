// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Streamable HTTP with explicit headers and bearer-token environment configuration.

use super::{McpClient, McpClientOptions, safe_error};
use ra_core::error::{Result, ToolErrorKind};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use std::collections::BTreeMap;

/// Remote transport configuration. Secrets are deliberately absent from Debug output.
#[derive(Clone)]
pub struct McpHttpParams {
    url: String,
    headers: BTreeMap<String, String>,
    bearer_token_env_var: Option<String>,
}
impl McpHttpParams {
    /// Selects an MCP endpoint.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: BTreeMap::new(),
            bearer_token_env_var: None,
        }
    }
    /// Supplies static headers.
    #[must_use]
    pub fn with_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.headers = headers;
        self
    }
    /// Loads the token at connection time; this overrides a static Authorization header.
    #[must_use]
    pub fn with_bearer_token_env_var(mut self, name: impl Into<String>) -> Self {
        self.bearer_token_env_var = Some(name.into());
        self
    }
}
impl McpClient {
    /// Initializes a Streamable HTTP session using the SDK's session handling and shutdown.
    pub async fn connect_http(
        server_name: impl Into<String>,
        params: McpHttpParams,
        options: McpClientOptions,
    ) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in params.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                safe_error(ToolErrorKind::InvalidInput, "invalid MCP HTTP header name")
            })?;
            let mut value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
                safe_error(ToolErrorKind::InvalidInput, "invalid MCP HTTP header value")
            })?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        if let Some(name) = params.bearer_token_env_var {
            let token = std::env::var(name)
                .ok()
                .filter(|token| !token.trim().is_empty())
                .ok_or_else(|| {
                    safe_error(
                        ToolErrorKind::InvalidInput,
                        "MCP bearer token environment variable is missing or empty",
                    )
                })?;
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| safe_error(ToolErrorKind::InvalidInput, "invalid MCP bearer token"))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|_| {
                safe_error(
                    ToolErrorKind::ExecutionFailed,
                    "MCP HTTP client construction failed",
                )
            })?;
        let transport = StreamableHttpClientTransport::with_client(
            client,
            StreamableHttpClientTransportConfig::with_uri(params.url),
        );
        Self::connect(server_name, transport, options).await
    }
}
