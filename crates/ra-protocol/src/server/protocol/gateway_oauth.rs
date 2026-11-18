// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Gateway OAuth sign-in wire types: Codex's `GatewayOAuth*` records in
//! `app-server-protocol/src/protocol/v2/account.rs`.
//!
//! A provider configured with `gateway_oauth` needs a second OAuth credential besides its primary
//! authentication. `account/gatewayOAuth/read` reports whether the effective provider uses one and
//! whether it is ready, `account/gatewayOAuth/login` runs browser authorization, and
//! `account/gatewayOAuth/cancel` stops the caller's login. `account/gatewayOAuth/changed` reports
//! readiness changes. None of these payloads carries a credential.

use serde::{Deserialize, Serialize};

/// The method of [`GatewayOAuthChangedNotification`].
pub const GATEWAY_OAUTH_CHANGED_METHOD: &str = "account/gatewayOAuth/changed";

/// Readiness and browser sign-in progress of a gateway credential: Codex's `GatewayOAuthStatus`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GatewayOAuthStatus {
    /// No usable credential is available.
    NotReady,
    /// A browser sign-in is in progress.
    Started,
    /// Saved credentials are locally usable; no gateway request has verified them.
    Succeeded,
    /// The last sign-in failed or was canceled.
    Failed,
}

/// A readiness change of the effective provider's gateway credential: Codex's
/// `GatewayOAuthChangedNotification`.
///
/// Its `Debug` output redacts the authorization URL, as Codex's does.
#[non_exhaustive]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayOAuthChangedNotification {
    /// Authorization handoff, sent only to the connection that started login.
    auth_url: Option<String>,
    provider_id: String,
    status: GatewayOAuthStatus,
    error: Option<String>,
}

impl GatewayOAuthChangedNotification {
    /// A change of `provider_id`'s gateway credential to `status`, with a safe failure message.
    #[must_use]
    pub fn new(
        provider_id: impl Into<String>,
        status: GatewayOAuthStatus,
        error: Option<String>,
    ) -> Self {
        Self {
            auth_url: None,
            provider_id: provider_id.into(),
            status,
            error,
        }
    }

    /// Carries the URL the initiating client opens to authorize the gateway.
    #[must_use]
    pub fn with_auth_url(mut self, auth_url: impl Into<String>) -> Self {
        self.auth_url = Some(auth_url.into());
        self
    }

    /// The authorization URL, present only on the initiating connection's `started` change.
    #[must_use]
    pub fn auth_url(&self) -> Option<&str> {
        self.auth_url.as_deref()
    }

    /// The effective provider whose credential changed.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// The new status.
    #[must_use]
    pub const fn status(&self) -> GatewayOAuthStatus {
        self.status
    }

    /// A safe description of a failure, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl std::fmt::Debug for GatewayOAuthChangedNotification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayOAuthChangedNotification")
            .field("auth_url", &self.auth_url.as_ref().map(|_| "[REDACTED]"))
            .field("provider_id", &self.provider_id)
            .field("status", &self.status)
            .field("error", &self.error)
            .finish()
    }
}

/// The answer to `account/gatewayOAuth/read`: Codex's `GatewayOAuthReadResponse`.
///
/// It describes the current effective gateway policy and credential readiness, and never contains
/// credentials.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayOAuthReadResponse {
    provider_id: String,
    provider_name: String,
    required: bool,
    status: Option<GatewayOAuthStatus>,
    error: Option<String>,
}

impl GatewayOAuthReadResponse {
    /// The effective provider, which does not use gateway OAuth.
    #[must_use]
    pub fn new(provider_id: impl Into<String>, provider_name: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            provider_name: provider_name.into(),
            required: false,
            status: None,
            error: None,
        }
    }

    /// Marks the provider as using gateway OAuth, with its credential's readiness.
    #[must_use]
    pub fn with_gateway_status(
        mut self,
        status: GatewayOAuthStatus,
        error: Option<String>,
    ) -> Self {
        self.required = true;
        self.status = Some(status);
        self.error = error;
        self
    }

    /// The effective provider's id.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// The effective provider's display name.
    #[must_use]
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// Whether the provider uses gateway OAuth, even when already signed in.
    #[must_use]
    pub const fn required(&self) -> bool {
        self.required
    }

    /// The credential's readiness; `None` when the provider does not use gateway OAuth.
    #[must_use]
    pub const fn status(&self) -> Option<GatewayOAuthStatus> {
        self.status
    }

    /// A safe description of the last failure, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// The answer to `account/gatewayOAuth/login` once the credential has been saved: Codex's
/// `GatewayOAuthLoginResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayOAuthLoginResponse {}

impl GatewayOAuthLoginResponse {
    /// The empty answer.
    #[must_use]
    pub const fn new() -> Self {
        Self {}
    }
}

/// The answer to `account/gatewayOAuth/cancel` once the caller's login has released its slot:
/// Codex's `GatewayOAuthCancelResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayOAuthCancelResponse {}

impl GatewayOAuthCancelResponse {
    /// The empty answer.
    #[must_use]
    pub const fn new() -> Self {
        Self {}
    }
}
