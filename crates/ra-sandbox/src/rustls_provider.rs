// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Process-wide TLS initialization at the Docker adapter boundary.

use std::sync::Once;

/// Installs Docker's ring default once, preserving a provider installed by the host.
pub(crate) fn ensure_rustls_crypto_provider() {
    static RUSTLS_PROVIDER_INIT: Once = Once::new();
    RUSTLS_PROVIDER_INIT.call_once(|| {
        // Docker retains its existing ring backend. The MCP HTTP adapter follows Codex's
        // aws-lc-rs selection; both adapters preserve the first process-wide installation.
        // install_default leaves an existing provider intact, including a concurrent install.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
