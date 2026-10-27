//! A host that installed its own provider first keeps it; initialization stays best-effort.
use rustls::crypto::CryptoProvider;

#[test]
fn keeps_a_provider_installed_by_the_embedding_host() {
    let host_provider = rustls::crypto::ring::default_provider();
    let host_cipher_suites = host_provider.cipher_suites.len();
    host_provider
        .install_default()
        .expect("the host installs first in a fresh process");
    let installed = CryptoProvider::get_default().expect("the host provider is installed");

    ra_tls::ensure_rustls_crypto_provider();

    let after = CryptoProvider::get_default().expect("a provider remains installed");
    assert!(std::sync::Arc::ptr_eq(installed, after));
    assert_eq!(after.cipher_suites.len(), host_cipher_suites);
}
