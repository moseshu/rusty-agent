//! Relay harness endpoints fail before registration or opening a direct JSON-RPC socket.
use ra_executor::{EnvironmentManager, ExecServerClient, RemoteExecServerConnectArgs};
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use std::time::Duration;
fn factory() -> HttpClientFactory {
    HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
}
#[tokio::test]
async fn harness_url_is_rejected_before_connection_and_environment_registration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    for query in ["role=harness", "token=value&role=harness&other=value"] {
        let url = format!("ws://{}/relay?{query}", listener.local_addr().unwrap());
        let manager = EnvironmentManager::without_environments(factory());
        let error = manager
            .upsert_environment("relay".into(), url.clone(), None)
            .unwrap_err();
        assert!(error.to_string().contains("relay framing"));
        assert!(manager.get_environment("relay").is_none());
        let error = ExecServerClient::connect_websocket(RemoteExecServerConnectArgs::new(
            url,
            "test".into(),
            factory(),
        ))
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("relay framing"));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn configured_harness_url_is_rejected_during_preparation() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("environments.toml"),
        "include_local=false\n[[environments]]\nid='relay'\nurl='wss://relay.invalid/?role=harness'\n",
    )
    .unwrap();
    let error = EnvironmentManager::prepare_from_codex_home(home.path())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("relay framing"));
}
