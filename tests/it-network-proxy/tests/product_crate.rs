//! The proxy as its users build and run it, through the product crate. The host's other cases run
//! upstream's private tests against a copy of the product sources (see `build.rs`); these check
//! that the crate those copies come from builds, starts and enforces its policy on its own.

use std::sync::Arc;

use ra_network_proxy::{
    ConfigReloader, ConfigReloaderFuture, ConfigState, NetworkProxy, NetworkProxyConfig,
    NetworkProxyConstraints, NetworkProxyState, Platform, build_config_state,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A configuration that never changes.
struct Fixed;

impl ConfigReloader for Fixed {
    fn source_label(&self) -> String {
        "fixed test configuration".to_owned()
    }

    fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
        Box::pin(async { Ok(None) })
    }

    fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
        Box::pin(async { anyhow::bail!("the fixed test configuration is not reloaded") })
    }
}

#[tokio::test]
async fn the_product_proxy_refuses_a_host_outside_its_allowlist() {
    let mut config = NetworkProxyConfig {
        enabled: true,
        ..NetworkProxyConfig::default()
    };
    config.set_allowed_domains(vec!["allowed.test".to_owned()]);
    let state = build_config_state(
        config,
        NetworkProxyConstraints::default(),
        Platform::native(),
    )
    .expect("the configuration compiles");
    let state = NetworkProxyState::with_reloader(state, Arc::new(Fixed));
    let proxy = NetworkProxy::builder()
        .state(Arc::new(state))
        .build()
        .await
        .expect("the proxy builds");
    let handle = proxy.run().await.expect("the proxy starts");
    let address = proxy.http_addr();
    assert!(address.ip().is_loopback(), "{address}");

    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("the proxy listens");
    stream
        .write_all(
            b"GET http://blocked.test/ HTTP/1.1\r\nHost: blocked.test\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    // Refused for the allowlist, not for anything else the policy checks.
    assert!(
        response.contains(r#""reason":"not_allowed","#),
        "{response}"
    );
    handle.shutdown().await.expect("the proxy stops");
}
