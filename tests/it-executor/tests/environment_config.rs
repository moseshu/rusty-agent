//! Configured environment startup and parse-error privacy at the actual file boundary.
use ra_executor::EnvironmentManager;
use ra_http::{HttpClientFactory, OutboundProxyPolicy};

#[tokio::test]
async fn explicitly_disabled_configuration_does_not_materialize_local_environment() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("environments.toml"),
        "default = 'none'\ninclude_local = false\n",
    )
    .unwrap();
    let manager = EnvironmentManager::from_codex_home(
        home.path(),
        None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
    .await
    .unwrap();
    assert_eq!(manager.default_environment_id(), None);
    assert!(manager.try_local_environment().is_none());
    assert!(manager.default_environment_ids().is_empty());
}

#[tokio::test]
async fn local_environment_requires_configured_native_runtime_paths() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("environments.toml"),
        "include_local = true\n",
    )
    .unwrap();
    let prepared = EnvironmentManager::prepare_from_codex_home(home.path())
        .await
        .unwrap();
    let error = prepared
        .build(
            None,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("local environment requires configured runtime paths")
    );
}

#[tokio::test]
async fn malformed_config_diagnostic_does_not_echo_credential_source() {
    let home = tempfile::tempdir().unwrap();
    let contents = "include_local = false\n[[environments]]\nid = 'test'\nurl = 'wss://example.invalid'\nauth_bearer_token = 'credential-must-not-appear' trailing-invalid-content\n";
    std::fs::write(home.path().join("environments.toml"), contents).unwrap();
    let error = EnvironmentManager::prepare_from_codex_home(home.path())
        .await
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(error.contains("failed to parse environment config"));
    assert!(!error.contains("credential-must-not-appear"));
}
