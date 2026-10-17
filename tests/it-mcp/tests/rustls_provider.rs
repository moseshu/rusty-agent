// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Provider installation and host ownership, exercised through the public HTTP client factory.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use ra_mcp::client::{McpClient, McpClientOptions, http::McpHttpParams};
use rustls::crypto::CryptoProvider;

const PROVIDER_CASE: &str = "RA_MCP_RUSTLS_PROVIDER_CASE";

fn run_provider_case(case: &str) {
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "provider_child", "--ignored", "--nocapture"])
        .env(PROVIDER_CASE, case)
        .output()
        .expect("run the provider case");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{stdout}");
}

#[test]
fn http_client_installs_p521_provider_before_calling_custom_factory() {
    run_provider_case("install");
}

#[test]
fn http_client_preserves_preinstalled_provider() {
    run_provider_case("preinstalled");
}

#[tokio::test]
#[ignore = "run in a fresh process by the provider tests"]
async fn provider_child() {
    let Ok(case) = std::env::var(PROVIDER_CASE) else {
        return;
    };
    assert!(CryptoProvider::get_default().is_none());
    if case == "preinstalled" {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("install the host's provider");
    }

    for _ in 0..2 {
        let expected = CryptoProvider::get_default().cloned();
        let called = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&called);
        let params =
            McpHttpParams::new("https://localhost/mcp").with_http_client_factory(move |builder| {
                let provider = CryptoProvider::get_default().expect("provider before the factory");
                if let Some(expected) = &expected {
                    assert!(Arc::ptr_eq(provider, expected));
                } else {
                    assert_eq!(
                        provider
                            .signature_verification_algorithms
                            .supported_schemes(),
                        rustls::crypto::aws_lc_rs::default_provider()
                            .signature_verification_algorithms
                            .supported_schemes()
                    );
                    assert!(
                        provider
                            .signature_verification_algorithms
                            .supported_schemes()
                            .contains(&rustls::SignatureScheme::ECDSA_NISTP521_SHA512)
                    );
                }
                observed.store(true, Ordering::SeqCst);
                // Stop at the factory boundary without issuing a network request.
                builder.user_agent("\n").build()
            });
        assert!(
            McpClient::connect_http("provider", params, McpClientOptions::default())
                .await
                .is_err()
        );
        assert!(called.load(Ordering::SeqCst));
    }
}
