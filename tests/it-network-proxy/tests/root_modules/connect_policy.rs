// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkProxyConfig;
    use crate::state::network_proxy_state_for_policy;
    use rama_net::address::HostWithPort;
    use std::net::Ipv4Addr;
    use tokio::net::TcpListener;

    #[tokio::test(flavor = "current_thread")]
    async fn direct_connector_rejects_non_public_target_when_local_binding_disabled() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local listener");
        let target = listener.local_addr().expect("local addr");
        let connector = TargetCheckedTcpConnector::new(Arc::new(network_proxy_state_for_policy(
            NetworkProxyConfig::default(),
        )));

        let request: rama_tcp::client::Request =
            rama_tcp::client::Request::new(HostWithPort::from(target));
        let err = Service::serve(&connector, request)
            .await
            .expect_err("local target should be rejected");

        assert!(
            format!("{err:?}").contains("network target rejected by policy"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_connector_allows_non_public_target_when_local_binding_enabled() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local listener");
        let target = listener.local_addr().expect("local addr");
        let connector = TargetCheckedTcpConnector::new(Arc::new(network_proxy_state_for_policy(
            NetworkProxyConfig {
                allow_local_binding: Some(true),
                ..NetworkProxyConfig::default()
            },
        )));

        let request: rama_tcp::client::Request =
            rama_tcp::client::Request::new(HostWithPort::from(target));
        let result = Service::serve(&connector, request).await;

        assert!(result.is_ok(), "local target should be allowed: {result:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_connector_allows_explicitly_allowlisted_non_public_target() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local listener");
        let target = listener.local_addr().expect("local addr");
        let mut config = NetworkProxyConfig::default();
        config.set_allowed_domains(vec![target.ip().to_string()]);
        let connector =
            TargetCheckedTcpConnector::new(Arc::new(network_proxy_state_for_policy(config)));

        let request: rama_tcp::client::Request =
            rama_tcp::client::Request::new(HostWithPort::from(target));
        let result = Service::serve(&connector, request).await;

        assert!(
            result.is_ok(),
            "explicitly allowlisted local target should be allowed: {result:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_connector_allows_explicitly_allowlisted_localhost_target() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local listener");
        let target = listener.local_addr().expect("local addr");
        let mut config = NetworkProxyConfig::default();
        config.set_allowed_domains(vec!["localhost".to_string()]);
        let connector =
            TargetCheckedTcpConnector::new(Arc::new(network_proxy_state_for_policy(config)));

        let request: rama_tcp::client::Request =
            rama_tcp::client::Request::new(HostWithPort::new(Host::LOCALHOST_NAME, target.port()));
        let result = Service::serve(&connector, request).await;

        assert!(
            result.is_ok(),
            "explicitly allowlisted localhost target should be allowed: {result:?}"
        );
    }

    #[test]
    fn resolved_private_address_does_not_match_allowlisted_hostname() {
        let host = Host::Name("example.com".parse().expect("valid domain"));

        assert!(!target_matches_non_public_addr(
            &host,
            Ipv4Addr::LOCALHOST.into()
        ));
    }
}
