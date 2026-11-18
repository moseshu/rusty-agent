// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Provider OAuth for model gateways.
//!
//! [`GatewayAuthManager`] owns one gateway's OAuth credentials: it resolves a usable access token,
//! refreshes it, recovers after the gateway rejects it, and runs PKCE browser authorization through
//! a loopback callback. Credentials persist in the gateway namespace of the encrypted local
//! secrets store. Token grants run over the application's route-aware HTTP client with redirects
//! and request logging disabled.
//!
//! Codex's login crate also carries account sign-in for its own service. That machinery is not
//! part of this product; only the provider OAuth protocol and gateway credentials are ported.
//!
//! **Stability**: `Internal`. Consumed by the coding product's gateway OAuth integration.

mod gateway_auth;

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod oauth;
#[cfg(not(feature = "test-api"))]
mod oauth;

pub use gateway_auth::GatewayAuthConfig;
pub use gateway_auth::GatewayAuthError;
pub use gateway_auth::GatewayAuthManager;
pub use gateway_auth::GatewayAuthStatus;
pub use gateway_auth::GatewayAuthStatusChange;
pub use gateway_auth::GatewayLoginControl;
pub use gateway_auth::subscribe_gateway_auth_status;

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use gateway_auth::test_api;
