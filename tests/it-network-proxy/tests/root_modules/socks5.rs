// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkMode;
    use crate::config::NetworkProxyConfig;
    use crate::mitm_hook::MitmHookConfig;
    use crate::mitm_hook::MitmHookMatchConfig;
    use crate::network_policy::test_support::POLICY_DECISION_EVENT_NAME;
    use crate::network_policy::test_support::capture_events;
    use crate::network_policy::test_support::find_event_by_name;
    use crate::runtime::ConfigReloader;
    use crate::runtime::ConfigReloaderFuture;
    use crate::runtime::ConfigState;
    use crate::state::NetworkProxyConstraints;
    use crate::state::build_config_state;
    use pretty_assertions::assert_eq;
    use rama_core::extensions::Extensions;
    use rama_core::extensions::ExtensionsMut;
    use rama_net::address::HostWithPort;
    use rama_net::address::SocketAddress;
    use rama_socks5::server::udp::RelayDirection;
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::Mutex;

    // Managed MITM CA files live under the shared test CODEX_HOME, so MITM-enabled config state
    // must be materialized one test at a time.
    static MITM_CONFIG_STATE_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Clone)]
    struct StaticReloader {
        state: ConfigState,
    }

    impl ConfigReloader for StaticReloader {
        fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
            Box::pin(async { Ok(None) })
        }

        fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
            Box::pin(async { Ok(self.state.clone()) })
        }

        fn source_label(&self) -> String {
            "static test reloader".to_string()
        }
    }

    fn state_for_settings(network: NetworkProxyConfig) -> Arc<NetworkProxyState> {
        let config = network;
        let _mitm_config_state_guard = config.mitm.then(|| MITM_CONFIG_STATE_LOCK.lock().unwrap());
        let state = build_config_state(
            config,
            NetworkProxyConstraints::default(),
            crate::Platform::native(),
        )
        .unwrap();
        let reloader = Arc::new(StaticReloader {
            state: state.clone(),
        });
        Arc::new(NetworkProxyState::with_reloader(state, reloader))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_emits_block_decision_for_proxy_disabled() {
        let state = state_for_settings(NetworkProxyConfig {
            enabled: false,
            mode: NetworkMode::Full,
            ..NetworkProxyConfig::default()
        });
        let mut request =
            TcpRequest::new(HostWithPort::try_from("example.com:443").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let (result, events) = capture_events(|| async {
            handle_socks5_tcp(
                request,
                TargetCheckedTcpConnector::new(state.clone()),
                /*policy_decider*/ None,
                /*environment_id*/ None,
            )
            .await
        })
        .await;
        assert!(result.is_err(), "proxy-disabled request should be denied");

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision event");
        assert_eq!(event.field("network.policy.scope"), Some("non_domain"));
        assert_eq!(event.field("network.policy.decision"), Some("deny"));
        assert_eq!(event.field("network.policy.source"), Some("proxy_state"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_PROXY_DISABLED)
        );
        assert_eq!(
            event.field("network.transport.protocol"),
            Some("socks5_tcp")
        );
        assert_eq!(event.field("server.address"), Some("example.com"));
        assert_eq!(event.field("server.port"), Some("443"));
        assert_eq!(event.field("http.request.method"), Some("none"));
        assert_eq!(event.field("client.address"), Some("unknown"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_uses_mitm_in_limited_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Limited,
            mitm: true,
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["example.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("example.com:443").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let result = handle_socks5_tcp(
            request,
            TargetCheckedTcpConnector::new(state),
            /*policy_decider*/ None,
            /*environment_id*/ None,
        )
        .await
        .expect("limited-mode HTTPS should use MITM");

        assert!(matches!(result.conn, Socks5TcpConnection::Mitm { .. }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_blocks_non_https_in_limited_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Limited,
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["example.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("example.com:80").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let (result, events) = capture_events(|| async {
            handle_socks5_tcp(
                request,
                TargetCheckedTcpConnector::new(state),
                /*policy_decider*/ None,
                /*environment_id*/ None,
            )
            .await
        })
        .await;
        assert!(
            result.is_err(),
            "limited-mode non-HTTPS SOCKS should be denied"
        );

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision event");
        assert_eq!(event.field("network.policy.scope"), Some("non_domain"));
        assert_eq!(event.field("network.policy.decision"), Some("deny"));
        assert_eq!(event.field("network.policy.source"), Some("mode_guard"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_METHOD_NOT_ALLOWED)
        );
        assert_eq!(
            event.field("network.transport.protocol"),
            Some("socks5_tcp")
        );
        assert_eq!(event.field("server.address"), Some("example.com"));
        assert_eq!(event.field("server.port"), Some("80"));
        assert_eq!(event.field("http.request.method"), Some("none"));
        assert_eq!(event.field("client.address"), Some("unknown"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_detects_tls_for_brokered_nonstandard_port_in_full_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Full,
            mitm: true,
            credential_broker: true,
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["api.openai.com".to_string()]);
        let state = state_for_settings(settings);
        let mut env = HashMap::from([("OPENAI_API_KEY".to_string(), "sk-real".to_string())]);
        state.virtualize_child_credentials(&mut env);
        let mut request = TcpRequest::new(
            HostWithPort::try_from("api.openai.com:8443").expect("valid authority"),
        );
        request.extensions_mut().insert(state.clone());

        let result = handle_socks5_tcp(
            request,
            TargetCheckedTcpConnector::new(state),
            /*policy_decider*/ None,
            /*environment_id*/ None,
        )
        .await
        .expect("brokered TLS should defer MITM until protocol detection");

        assert!(matches!(
            result.conn,
            Socks5TcpConnection::DetectProtocol { .. }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_blocks_limited_mode_without_mitm_state() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Limited,
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["example.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("example.com:443").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let err = handle_socks5_tcp(
            request,
            TargetCheckedTcpConnector::new(state),
            /*policy_decider*/ None,
            /*environment_id*/ None,
        )
        .await
        .expect_err("limited-mode HTTPS requires MITM");

        assert!(
            format!("{err:?}").contains("MITM required"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_uses_mitm_for_hooked_host_in_full_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Full,
            mitm: true,
            mitm_hooks: vec![MitmHookConfig {
                host: "api.github.com".to_string(),
                matcher: MitmHookMatchConfig {
                    methods: vec!["POST".to_string()],
                    path_prefixes: vec!["/repos/openai/".to_string()],
                    ..MitmHookMatchConfig::default()
                },
                ..MitmHookConfig::default()
            }],
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["api.github.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("api.github.com:443").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let result = handle_socks5_tcp(
            request,
            TargetCheckedTcpConnector::new(state),
            /*policy_decider*/ None,
            /*environment_id*/ None,
        )
        .await
        .expect("hooked HTTPS should use MITM");

        assert!(matches!(result.conn, Socks5TcpConnection::Mitm { .. }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_blocks_hooked_non_https_host_in_full_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Full,
            mitm: true,
            mitm_hooks: vec![MitmHookConfig {
                host: "api.github.com".to_string(),
                matcher: MitmHookMatchConfig {
                    methods: vec!["POST".to_string()],
                    path_prefixes: vec!["/repos/openai/".to_string()],
                    ..MitmHookMatchConfig::default()
                },
                ..MitmHookConfig::default()
            }],
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["api.github.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("api.github.com:80").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let err = handle_socks5_tcp(
            request,
            TargetCheckedTcpConnector::new(state),
            /*policy_decider*/ None,
            /*environment_id*/ None,
        )
        .await
        .expect_err("hooked non-HTTPS SOCKS should require MITM");

        assert!(
            format!("{err:?}").contains("MITM required"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inspect_socks5_udp_emits_block_decision_for_mode_guard_deny() {
        let state = state_for_settings(NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Limited,
            ..NetworkProxyConfig::default()
        });
        let request = RelayRequest {
            direction: RelayDirection::South,
            server_address: SocketAddress::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 53),
            payload: Default::default(),
            extensions: Extensions::new(),
        };

        let (result, events) = capture_events(|| async {
            inspect_socks5_udp(
                request, state, /*policy_decider*/ None, /*environment_id*/ None,
            )
            .await
        })
        .await;
        assert!(result.is_err(), "limited-mode UDP request should be denied");

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision event");
        assert_eq!(event.field("network.policy.scope"), Some("non_domain"));
        assert_eq!(event.field("network.policy.decision"), Some("deny"));
        assert_eq!(event.field("network.policy.source"), Some("mode_guard"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_METHOD_NOT_ALLOWED)
        );
        assert_eq!(
            event.field("network.transport.protocol"),
            Some("socks5_udp")
        );
        assert_eq!(event.field("server.address"), Some("93.184.216.34"));
        assert_eq!(event.field("server.port"), Some("53"));
        assert_eq!(event.field("http.request.method"), Some("none"));
        assert_eq!(event.field("client.address"), Some("unknown"));
    }
}
