// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Provider installation and host ownership at the public Docker connection boundary.

use std::sync::Arc;

use ra_sandbox::docker::BollardDockerApi;
use rustls::crypto::CryptoProvider;

const PROVIDER_CASE: &str = "RA_DOCKER_RUSTLS_PROVIDER_CASE";

fn run_provider_case(case: &str) {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command
        .args(["--exact", "provider_child", "--ignored", "--nocapture"])
        .env(PROVIDER_CASE, case)
        .env_remove("DOCKER_TLS_VERIFY")
        .env(
            "DOCKER_CERT_PATH",
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/docker_tls"),
        );
    if matches!(
        case,
        "tls_tcp" | "tls_http" | "unix_tls_verify" | "npipe_tls_verify"
    ) {
        // Bollard treats a present, even empty, Unicode value as enabling TLS on TCP/HTTP.
        command.env("DOCKER_TLS_VERIFY", "");
    }
    let output = command.output().expect("run the provider case");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{stdout}");
}

#[test]
fn docker_tls_client_installs_provider_with_both_backends_enabled() {
    for case in ["install", "tls_tcp", "tls_http"] {
        run_provider_case(case);
    }
}

#[test]
fn docker_tls_client_preserves_preinstalled_provider() {
    run_provider_case("preinstalled");
}

#[test]
fn docker_plain_http_leaves_provider_unset() {
    for case in ["plain_tcp", "plain_http"] {
        run_provider_case(case);
    }
}

#[cfg(unix)]
#[test]
fn docker_unix_socket_leaves_provider_unset() {
    for case in ["unix", "unix_tls_verify"] {
        run_provider_case(case);
    }
}

#[cfg(windows)]
#[test]
fn docker_named_pipe_leaves_provider_unset() {
    for case in ["npipe", "npipe_tls_verify"] {
        run_provider_case(case);
    }
}

#[test]
#[ignore = "run in a fresh process by the provider tests"]
fn provider_child() {
    let Ok(case) = std::env::var(PROVIDER_CASE) else {
        return;
    };
    assert!(CryptoProvider::get_default().is_none());
    if case == "preinstalled" {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .expect("install the host's provider");
    }
    // Bollard checks that a Unix socket exists, but construction needs no Docker daemon.
    #[cfg(unix)]
    let directory = tempfile::tempdir_in("/tmp").expect("socket directory");
    #[cfg(unix)]
    let socket = directory.path().join("docker.sock");
    #[cfg(unix)]
    let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("Unix socket");
    #[cfg(unix)]
    let unix_host = format!("unix://{}", socket.display());
    let (host, uses_tls) = match case.as_str() {
        "install" | "preinstalled" => ("https://127.0.0.1:2376", true),
        "tls_tcp" => ("tcp://127.0.0.1:2376", true),
        "tls_http" => ("http://127.0.0.1:2376", true),
        "plain_tcp" => ("tcp://127.0.0.1:2375", false),
        "plain_http" => ("http://127.0.0.1:2375", false),
        #[cfg(unix)]
        "unix" | "unix_tls_verify" => (unix_host.as_str(), false),
        "npipe" | "npipe_tls_verify" => ("npipe:////./pipe/docker_engine", false),
        _ => panic!("unknown provider case: {case}"),
    };

    for _ in 0..2 {
        let expected = CryptoProvider::get_default().cloned();
        // Construction does not contact a daemon; the TLS attach test covers a real handshake.
        BollardDockerApi::connect_with_host(host).expect("a Docker client");
        if !uses_tls {
            assert!(CryptoProvider::get_default().is_none());
            continue;
        }
        let provider = CryptoProvider::get_default().expect("provider after client construction");
        if let Some(expected) = expected {
            assert!(Arc::ptr_eq(provider, &expected));
        } else {
            assert_eq!(
                provider
                    .signature_verification_algorithms
                    .supported_schemes(),
                rustls::crypto::ring::default_provider()
                    .signature_verification_algorithms
                    .supported_schemes()
            );
        }
    }
}
