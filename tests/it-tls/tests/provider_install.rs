//! A fresh process gets the aws-lc-rs provider, which verifies ECDSA P-521/SHA-512 signatures.
use rustls::SignatureScheme;
use rustls::crypto::CryptoProvider;

#[test]
fn installs_a_provider_that_verifies_p521_signatures_once() {
    assert!(CryptoProvider::get_default().is_none());

    ra_tls::ensure_rustls_crypto_provider();
    let installed = CryptoProvider::get_default().expect("a provider should be installed");
    assert!(
        installed
            .signature_verification_algorithms
            .supported_schemes()
            .contains(&SignatureScheme::ECDSA_NISTP521_SHA512)
    );

    ra_tls::ensure_rustls_crypto_provider();
    let again = CryptoProvider::get_default().expect("the provider should remain installed");
    assert!(std::sync::Arc::ptr_eq(installed, again));
}
