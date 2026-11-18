// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! OAuth protocol operations shared by independent credential managers.
//!
//! Callers supply an HTTP client with the application's proxy and CA policy. They retain
//! ownership of credential storage, refresh scheduling, account validation, and login UX.
//!
//! The module keeps Codex's whole protocol surface. JSON token encoding and unlimited error
//! bodies serve Codex's account refresh, which this product does not port; they stay so the
//! shared module and its tests match upstream, and only the gateway's form grants call it here.

#![cfg_attr(
    not(feature = "test-api"),
    allow(
        dead_code,
        unreachable_pub,
        unused_imports,
        reason = "Crate-private outside test-api; account-only protocol variants have no caller"
    )
)]

mod authorization;
mod client;
mod diagnostics;
mod error;
mod pkce;

pub use authorization::AuthorizationRequest;
pub use authorization::CallbackError;
pub use authorization::CallbackParameters;
pub use authorization::build_authorization_url;
pub use authorization::generate_state;
pub use client::AuthorizationCodeGrant;
pub use client::OAuthClient;
pub use client::RefreshTokenGrant;
pub use client::TokenEncoding;
pub use client::TokenEndpoint;
pub use diagnostics::sanitize_url_for_logging;
pub use error::ErrorBodyLimit;
pub use error::OAuthError;
pub use error::TokenErrorDetail;
pub use error::TokenRejection;
pub use pkce::PkceCodes;
pub use pkce::generate_pkce;
