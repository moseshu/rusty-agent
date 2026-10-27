// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Host-resolved enterprise authorization records and MCP transport validation.

use super::{McpServerConfig, McpServerTransportConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
/// The enterprise identity provider selected by trusted host configuration.
pub struct McpServerIdpOAuthConfig {
    /// Issuer used for enterprise OAuth discovery and identity validation.
    pub issuer: String,
    /// Public OAuth client registered with that enterprise `IdP`.
    pub client_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
/// A complete enterprise authorization profile selected as one trusted record.
pub struct McpEnterpriseManagedAuthConfig {
    /// Shared enterprise authorization, independent of application account credentials.
    pub idp: McpServerIdpOAuthConfig,
}

/// Immutable authorization selected after trusted policy and catalog resolution.
/// This cannot be deserialized from a server or plugin declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEmaRegistration {
    idp: McpServerIdpOAuthConfig,
    server_url: String,
    resource: Option<String>,
    client_id: Option<String>,
    authorization_server_issuer: Option<String>,
    scopes: Vec<String>,
}

impl McpEmaRegistration {
    /// Returns the trusted identity provider.
    #[must_use]
    pub fn idp(&self) -> &McpServerIdpOAuthConfig {
        &self.idp
    }

    /// Returns the authorized MCP resource endpoint.
    #[must_use]
    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    /// Returns the explicitly selected OAuth resource parameter.
    #[must_use]
    pub fn resource(&self) -> Option<&str> {
        self.resource.as_deref()
    }

    /// Returns the registered OAuth resource client identifier.
    #[must_use]
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    /// Returns the expected resource authorization issuer.
    #[must_use]
    pub fn authorization_server_issuer(&self) -> Option<&str> {
        self.authorization_server_issuer.as_deref()
    }

    /// Returns the scopes bound to this enterprise registration.
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }
}

impl McpServerConfig {
    /// Returns the identity provider of the host-resolved registration.
    pub fn oauth_idp(&self) -> Option<&McpServerIdpOAuthConfig> {
        self.ema_registration().map(McpEmaRegistration::idp)
    }

    /// Returns immutable host-resolved authorization, when present.
    #[must_use]
    pub fn ema_registration(&self) -> Option<&McpEmaRegistration> {
        self.oauth.as_ref()?.ema_registration.as_ref()
    }

    /// Called once on a materialized catalog after configuration provenance is checked.
    pub fn resolve_ema_registration(
        &mut self,
        idp: &McpServerIdpOAuthConfig,
    ) -> Result<(), &'static str> {
        if let Some(oauth) = &mut self.oauth {
            oauth.ema_registration = None;
        }
        if !matches!(self.auth, super::McpServerAuth::EmaAuth) {
            return Err("enterprise registration requires ema_auth");
        }
        self.validate_ema_auth_transport()?;
        let McpServerTransportConfig::StreamableHttp { url, .. } = &self.transport else {
            unreachable!("EMA transport was validated");
        };
        let oauth = self.oauth.get_or_insert_default();
        oauth.ema_registration = Some(McpEmaRegistration {
            idp: idp.clone(),
            server_url: url.clone(),
            resource: self.oauth_resource.clone(),
            client_id: oauth.client_id.clone(),
            authorization_server_issuer: oauth.authorization_server_issuer.clone(),
            scopes: self.scopes.clone().unwrap_or_default(),
        });
        Ok(())
    }

    /// EMA never falls back to an unrelated bearer or executor-owned credential.
    pub fn validate_ema_auth_transport(&self) -> Result<(), &'static str> {
        if !self.is_local_environment() {
            return Err("ema_auth requires a host-owned MCP connection");
        }
        if self.oauth_client_secret().is_some() {
            return Err("ema_auth cannot be combined with oauth.client_secret");
        }
        let McpServerTransportConfig::StreamableHttp {
            bearer_token_env_var,
            http_headers,
            env_http_headers,
            http_headers_helper,
            ..
        } = &self.transport
        else {
            return Err("ema_auth requires streamable HTTP");
        };
        if bearer_token_env_var.is_some()
            || http_headers
                .as_ref()
                .is_some_and(|headers| !headers.is_empty())
            || env_http_headers
                .as_ref()
                .is_some_and(|headers| !headers.is_empty())
            || http_headers_helper.is_some()
        {
            return Err("ema_auth cannot be combined with alternate HTTP credentials");
        }
        Ok(())
    }
}
