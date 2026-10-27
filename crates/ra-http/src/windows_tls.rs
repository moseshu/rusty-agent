// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Windows certificate validation for callers without a custom CA bundle.
//!
//! Unlike a snapshot of installed roots, Windows can retrieve missing trusted roots on demand.
//! Callers must retain the custom-CA configuration path when a bundle is configured.

use std::sync::Arc;

use crate::ensure_rustls_crypto_provider;
use rustls::ClientConfig;
use rustls_platform_verifier::ConfigVerifierExt;

/// Builds a TLS configuration that validates server certificates using Windows trust policy.
///
/// This does not load Codex custom CA settings; callers must check those before selecting it.
pub fn build_windows_platform_tls_config() -> Result<Arc<ClientConfig>, rustls::Error> {
    ensure_rustls_crypto_provider();
    ClientConfig::with_platform_verifier().map(Arc::new)
}
