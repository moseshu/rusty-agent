// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#[cfg(test)]
pub(crate) fn network_proxy_state_for_policy(
    mut config: crate::config::NetworkProxyConfig,
) -> NetworkProxyState {
    let brokerage_created_default_allowlist =
        config.credential_broker && !config.enabled && config.allowed_domains().is_none();
    config.enabled = true;
    if brokerage_created_default_allowlist {
        config.set_allowed_domains(vec!["*".to_string()]);
    }
    let state = ConfigState {
        executor_os: crate::Platform::native(),
        allow_set: crate::policy::compile_allowlist(&config.allowed_domains().unwrap_or_default())
            .unwrap(),
        blocked: VecDeque::new(),
        blocked_total: 0,
        brokerage_created_default_allowlist,
        constraints: NetworkProxyConstraints::default(),
        deny_set: crate::policy::compile_denylist(&config.denied_domains().unwrap_or_default())
            .unwrap(),
        mitm: None,
        mitm_hooks: crate::mitm_hook::compile_mitm_hooks(&config).unwrap(),
        config,
    };

    NetworkProxyState::with_reloader(state, Arc::new(NoopReloader))
}

#[cfg(test)]
struct NoopReloader;

#[cfg(test)]
impl ConfigReloader for NoopReloader {
    fn source_label(&self) -> String {
        "test config state".to_string()
    }

    fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
        Box::pin(async { Ok(None) })
    }

    fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
        Box::pin(async { Err(anyhow::anyhow!("force reload is not supported in tests")) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::NetworkProxyConfig;
    use crate::policy::compile_allowlist;
    use crate::policy::compile_denylist;
    use crate::state::NetworkProxyConstraints;
    use crate::state::build_config_state;
    use crate::state::validate_policy_against_constraints;
    use pretty_assertions::assert_eq;

    #[derive(Clone)]
    struct StaticReloader {
        state: ConfigState,
    }

    impl ConfigReloader for StaticReloader {
        fn source_label(&self) -> String {
            "static test reloader".to_string()
        }

        fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
            let state = self.state.clone();
            Box::pin(async move { Ok(Some(state)) })
        }

        fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
            let state = self.state.clone();
            Box::pin(async move { Ok(state) })
        }
    }

    fn strings(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_string()).collect()
    }

    fn network_settings(allowed_domains: &[&str], denied_domains: &[&str]) -> NetworkProxyConfig {
        let mut network = NetworkProxyConfig::default();
        if !allowed_domains.is_empty() {
            network.set_allowed_domains(strings(allowed_domains));
        }
        if !denied_domains.is_empty() {
            network.set_denied_domains(strings(denied_domains));
        }
        network
    }

    fn network_settings_with_unix_sockets(
        allowed_domains: &[&str],
        denied_domains: &[&str],
        unix_sockets: &[String],
    ) -> NetworkProxyConfig {
        let mut network = network_settings(allowed_domains, denied_domains);
        if !unix_sockets.is_empty() {
            network.set_allow_unix_sockets(unix_sockets.to_vec());
        }
        network
    }

    #[tokio::test]
    async fn reload_applies_credential_broker_enablement_changes() {
        let config = NetworkProxyConfig {
            enabled: true,
            ..NetworkProxyConfig::default()
        };
        let initial_state = build_config_state(
            config.clone(),
            NetworkProxyConstraints::default(),
            crate::Platform::native(),
        )
        .unwrap();
        let mut reloaded_config = config;
        reloaded_config.set_credential_broker_enabled(/*enabled*/ true);
        let reloaded_state = build_config_state(
            reloaded_config,
            NetworkProxyConstraints::default(),
            crate::Platform::native(),
        )
        .unwrap();
        let state = NetworkProxyState::with_reloader(
            initial_state,
            Arc::new(StaticReloader {
                state: reloaded_state,
            }),
        );

        state
            .force_reload()
            .await
            .expect("enable credential broker");
        let mut env = HashMap::from([("OPENAI_API_KEY".to_string(), "sk-real".to_string())]);
        state.virtualize_child_credentials(&mut env);
        assert_ne!(env["OPENAI_API_KEY"], "sk-real");
    }

    #[test]
    fn managed_disabled_proxy_does_not_virtualize_credentials() {
        let mut config = NetworkProxyConfig::default();
        config.set_credential_broker_enabled(/*enabled*/ true);
        let config_state = build_config_state(
            config,
            NetworkProxyConstraints {
                enabled: Some(false),
                ..NetworkProxyConstraints::default()
            },
            crate::Platform::native(),
        )
        .expect("managed-disabled credential broker should fail open");
        let state = NetworkProxyState::with_reloader(config_state, Arc::new(NoopReloader));
        let mut env = HashMap::from([("OPENAI_API_KEY".to_string(), "sk-real".to_string())]);

        state.virtualize_child_credentials(&mut env);

        assert_eq!(env["OPENAI_API_KEY"], "sk-real");
    }

    #[tokio::test]
    async fn host_blocked_denied_wins_over_allowed() {
        let state =
            network_proxy_state_for_policy(network_settings(&["example.com"], &["example.com"]));

        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
    }

    #[tokio::test]
    async fn host_blocked_requires_allowlist_match() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            // Use a public IP literal to avoid relying on ambient DNS behavior (some networks
            // resolve unknown hostnames to private IPs, which would trigger `not_allowed_local`).
            state.host_blocked("8.8.8.8", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowed)
        );
        for enabled in [false, true] {
            let mut config = NetworkProxyConfig {
                enabled,
                ..NetworkProxyConfig::default()
            };
            config.set_credential_broker_enabled(/*enabled*/ true);
            let config_state = build_config_state(
                config,
                NetworkProxyConstraints::default(),
                crate::Platform::native(),
            )
            .expect("valid credential-broker configuration");
            assert_eq!(config_state.brokerage_created_default_allowlist, !enabled);
            let state = NetworkProxyState::with_reloader(config_state, Arc::new(NoopReloader));

            assert_eq!(
                state.host_blocked("8.8.8.8", /*port*/ 80).await.unwrap(),
                if enabled {
                    HostBlockDecision::Blocked(HostBlockReason::NotAllowed)
                } else {
                    HostBlockDecision::Allowed
                }
            );
            assert_eq!(
                state.host_blocked("127.0.0.1", /*port*/ 80).await.unwrap(),
                HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
            );
        }
    }

    #[tokio::test]
    async fn add_allowed_domain_removes_matching_deny_entry() {
        let state = network_proxy_state_for_policy(network_settings(&[], &["example.com"]));

        state.add_allowed_domain("ExAmPlE.CoM").await.unwrap();

        let (allowed, denied) = state.current_patterns().await.unwrap();
        assert_eq!(allowed, vec!["example.com".to_string()]);
        assert!(denied.is_empty());
        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn add_denied_domain_removes_matching_allow_entry() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        state.add_denied_domain("EXAMPLE.COM").await.unwrap();

        let (allowed, denied) = state.current_patterns().await.unwrap();
        assert!(allowed.is_empty());
        assert_eq!(denied, vec!["example.com".to_string()]);
        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
    }

    #[tokio::test]
    async fn add_denied_domain_forces_block_with_global_wildcard_allowlist() {
        let state = network_proxy_state_for_policy(network_settings(&["*"], &[]));

        assert_eq!(
            // Use a public IP literal to avoid relying on ambient DNS behavior.
            state.host_blocked("8.8.8.8", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Allowed
        );

        state.add_denied_domain("8.8.8.8").await.unwrap();

        let (allowed, denied) = state.current_patterns().await.unwrap();
        assert_eq!(allowed, vec!["*".to_string()]);
        assert_eq!(denied, vec!["8.8.8.8".to_string()]);
        assert_eq!(
            state.host_blocked("8.8.8.8", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
    }

    #[tokio::test]
    async fn add_allowed_domain_succeeds_when_managed_baseline_allows_expansion() {
        let mut config = network_settings(&["managed.example.com"], &[]);
        config.enabled = true;
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["managed.example.com".to_string()]),
            allowlist_expansion_enabled: Some(true),
            ..NetworkProxyConstraints::default()
        };
        let state = NetworkProxyState::with_reloader(
            build_config_state(config, constraints, crate::Platform::native()).unwrap(),
            Arc::new(NoopReloader),
        );

        state.add_allowed_domain("user.example.com").await.unwrap();

        let (allowed, denied) = state.current_patterns().await.unwrap();
        assert_eq!(
            allowed,
            vec![
                "managed.example.com".to_string(),
                "user.example.com".to_string()
            ]
        );
        assert!(denied.is_empty());
    }

    #[tokio::test]
    async fn add_allowed_domain_rejects_expansion_when_managed_baseline_is_fixed() {
        let mut config = network_settings(&["managed.example.com"], &[]);
        config.enabled = true;
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["managed.example.com".to_string()]),
            allowlist_expansion_enabled: Some(false),
            ..NetworkProxyConstraints::default()
        };
        let state = NetworkProxyState::with_reloader(
            build_config_state(config, constraints, crate::Platform::native()).unwrap(),
            Arc::new(NoopReloader),
        );

        let err = state
            .add_allowed_domain("user.example.com")
            .await
            .expect_err("managed baseline should reject allowlist expansion");

        assert!(
            format!("{err:#}").contains("network.allowed_domains constrained by managed config"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn add_denied_domain_rejects_expansion_when_managed_baseline_is_fixed() {
        let mut config = network_settings(&[], &["managed.example.com"]);
        config.enabled = true;
        let constraints = NetworkProxyConstraints {
            denied_domains: Some(vec!["managed.example.com".to_string()]),
            denylist_expansion_enabled: Some(false),
            ..NetworkProxyConstraints::default()
        };
        let state = NetworkProxyState::with_reloader(
            build_config_state(config, constraints, crate::Platform::native()).unwrap(),
            Arc::new(NoopReloader),
        );

        let err = state
            .add_denied_domain("user.example.com")
            .await
            .expect_err("managed baseline should reject denylist expansion");

        assert!(
            format!("{err:#}").contains("network.denied_domains constrained by managed config"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn blocked_snapshot_does_not_consume_entries() {
        let state = network_proxy_state_for_policy(NetworkProxyConfig::default());

        state
            .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                host: "google.com".to_string(),
                reason: "not_allowed".to_string(),
                client: None,
                method: Some("GET".to_string()),
                mode: None,
                protocol: "http".to_string(),
                decision: Some("ask".to_string()),
                source: Some("decider".to_string()),
                port: Some(80),
            }))
            .await
            .expect("entry should be recorded");

        let snapshot = state
            .blocked_snapshot()
            .await
            .expect("snapshot should succeed");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].host, "google.com");
        assert_eq!(snapshot[0].decision.as_deref(), Some("ask"));

        let drained = state
            .drain_blocked()
            .await
            .expect("drain should include snapshot entry");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].host, snapshot[0].host);
        assert_eq!(drained[0].reason, snapshot[0].reason);
        assert_eq!(drained[0].decision, snapshot[0].decision);
        assert_eq!(drained[0].source, snapshot[0].source);
        assert_eq!(drained[0].port, snapshot[0].port);
    }

    #[tokio::test]
    async fn drain_blocked_returns_buffered_window() {
        let state = network_proxy_state_for_policy(NetworkProxyConfig::default());

        for idx in 0..(MAX_BLOCKED_EVENTS + 5) {
            state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: format!("example{idx}.com"),
                    reason: "not_allowed".to_string(),
                    client: None,
                    method: Some("GET".to_string()),
                    mode: None,
                    protocol: "http".to_string(),
                    decision: Some("ask".to_string()),
                    source: Some("decider".to_string()),
                    port: Some(80),
                }))
                .await
                .expect("entry should be recorded");
        }

        let blocked = state.drain_blocked().await.expect("drain should succeed");
        assert_eq!(blocked.len(), MAX_BLOCKED_EVENTS);
        assert_eq!(blocked[0].host, "example5.com");
    }

    #[test]
    fn blocked_request_violation_log_line_serializes_payload() {
        let entry = BlockedRequest {
            host: "google.com".to_string(),
            reason: "not_allowed".to_string(),
            client: Some("127.0.0.1".to_string()),
            method: Some("GET".to_string()),
            mode: Some(NetworkMode::Full),
            protocol: "http".to_string(),
            execution_id: None,
            decision: Some("ask".to_string()),
            source: Some("decider".to_string()),
            port: Some(80),
            timestamp: 1_735_689_600,
        };

        assert_eq!(
            blocked_request_violation_log_line(&entry),
            r#"CODEX_NETWORK_POLICY_VIOLATION {"host":"google.com","reason":"not_allowed","client":"127.0.0.1","method":"GET","mode":"full","protocol":"http","decision":"ask","source":"decider","port":80,"timestamp":1735689600}"#
        );
    }

    #[tokio::test]
    async fn host_blocked_subdomain_wildcards_exclude_apex() {
        let state = network_proxy_state_for_policy(network_settings(&["*.openai.com"], &[]));

        assert_eq!(
            state
                .host_blocked("api.openai.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            state.host_blocked("openai.com", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowed)
        );
    }

    #[tokio::test]
    async fn host_blocked_global_wildcard_allowlist_allows_public_hosts_except_denylist() {
        let state = network_proxy_state_for_policy(network_settings(&["*"], &["evil.example"]));

        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            state
                .host_blocked("api.openai.com", /*port*/ 443)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            state
                .host_blocked("evil.example", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_loopback_when_local_binding_disabled() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        assert_eq!(
            state.host_blocked("127.0.0.1", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
        assert_eq!(
            state.host_blocked("localhost", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_blocked_allows_loopback_when_explicitly_allowlisted_and_local_binding_disabled() {
        let state = network_proxy_state_for_policy(network_settings(&["localhost"], &[]));

        assert_eq!(
            state.host_blocked("localhost", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn host_blocked_allows_private_ip_literal_when_explicitly_allowlisted() {
        let state = network_proxy_state_for_policy(network_settings(&["10.0.0.1"], &[]));

        assert_eq!(
            state.host_blocked("10.0.0.1", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_scoped_ipv6_literal_when_not_allowlisted() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        assert_eq!(
            state
                .host_blocked("fe80::1%lo0", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_blocked_allows_scoped_ipv6_literal_when_explicitly_allowlisted() {
        let state = network_proxy_state_for_policy(network_settings(&["fe80::1"], &[]));

        assert_eq!(
            state
                .host_blocked("fe80::1%lo0", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn host_blocked_requires_exact_scoped_ipv6_allowlist_match() {
        let state = network_proxy_state_for_policy(NetworkProxyConfig {
            allow_local_binding: Some(true),
            ..network_settings(&["fe80::1%eth0"], &[])
        });

        assert_eq!(
            state
                .host_blocked("fe80::1%eth0", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            state
                .host_blocked("fe80::1%eth1", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowed)
        );
    }

    #[tokio::test]
    async fn host_blocked_denies_scoped_ipv6_literal_before_local_binding() {
        let state = network_proxy_state_for_policy(NetworkProxyConfig {
            allow_local_binding: Some(true),
            ..network_settings(&["*"], &["fd00::1"])
        });

        for host in ["fd00::1%eth0", "[fd00::1%eth0]", "[fd00::1%25eth0]"] {
            assert_eq!(
                state.host_blocked(host, /*port*/ 80).await.unwrap(),
                HostBlockDecision::Blocked(HostBlockReason::Denied),
                "host should be denied after normalization: {host}"
            );
        }
    }

    #[tokio::test]
    async fn host_blocked_requires_exact_scoped_ipv6_denylist_match() {
        let state = network_proxy_state_for_policy(NetworkProxyConfig {
            allow_local_binding: Some(true),
            ..network_settings(&["*"], &["fd00::1%eth0"])
        });

        assert_eq!(
            state
                .host_blocked("fd00::1%eth0", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
        assert_eq!(
            state
                .host_blocked("fd00::1%eth1", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_private_ip_literals_when_local_binding_disabled() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        assert_eq!(
            state.host_blocked("10.0.0.1", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_loopback_when_allowlist_empty() {
        let state = network_proxy_state_for_policy(NetworkProxyConfig::default());

        assert_eq!(
            state.host_blocked("127.0.0.1", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_allowlisted_hostname_when_dns_lookup_fails() {
        let mut network = NetworkProxyConfig::default();
        network.set_allowed_domains(vec!["does-not-resolve.invalid".to_string()]);
        let state = network_proxy_state_for_policy(network);

        assert_eq!(
            state
                .host_blocked("does-not-resolve.invalid", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_resolves_to_non_public_ip_blocks_on_dns_lookup_timeout() {
        let blocked = host_resolves_to_non_public_ip(
            "slow.example",
            /*port*/ 80,
            Duration::from_millis(1),
            |_host, _port| async {
                std::future::pending::<std::io::Result<Vec<SocketAddr>>>().await
            },
        )
        .await;

        assert!(blocked);
    }

    #[tokio::test]
    async fn host_resolves_to_non_public_ip_blocks_on_dns_lookup_error() {
        let blocked = host_resolves_to_non_public_ip(
            "error.example",
            /*port*/ 80,
            Duration::from_millis(10),
            |_host, _port| async {
                Err::<Vec<SocketAddr>, std::io::Error>(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "forced failure",
                ))
            },
        )
        .await;

        assert!(blocked);
    }

    #[tokio::test]
    async fn host_resolves_to_non_public_ip_blocks_private_resolution() {
        let blocked = host_resolves_to_non_public_ip(
            "local.example",
            /*port*/ 80,
            Duration::from_millis(10),
            |_host, _port| async { Ok(vec!["127.0.0.1:80".parse().unwrap()]) },
        )
        .await;

        assert!(blocked);
    }

    #[tokio::test]
    async fn host_resolves_to_non_public_ip_allows_public_resolution() {
        let blocked = host_resolves_to_non_public_ip(
            "public.example",
            /*port*/ 80,
            Duration::from_millis(10),
            |_host, _port| async { Ok(vec!["8.8.8.8:80".parse().unwrap()]) },
        )
        .await;

        assert!(!blocked);
    }

    #[test]
    fn validate_policy_against_constraints_disallows_widening_allowed_domains() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["example.com".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let mut config = network_settings(&["example.com", "evil.com"], &[]);
        config.enabled = true;

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_allows_expanding_allowed_domains_when_enabled() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["example.com".to_string()]),
            allowlist_expansion_enabled: Some(true),
            ..NetworkProxyConstraints::default()
        };

        let mut config = network_settings(&["example.com", "api.openai.com"], &[]);
        config.enabled = true;

        assert!(validate_policy_against_constraints(&config, &constraints).is_ok());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_widening_mode() {
        let constraints = NetworkProxyConstraints {
            mode: Some(NetworkMode::Limited),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Full,
            ..NetworkProxyConfig::default()
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_allows_narrowing_wildcard_allowlist() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["*.example.com".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let mut config = network_settings(&["api.example.com"], &[]);
        config.enabled = true;

        assert!(validate_policy_against_constraints(&config, &constraints).is_ok());
    }

    #[test]
    fn validate_policy_against_constraints_rejects_widening_wildcard_allowlist() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["*.example.com".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let mut config = network_settings(&["**.example.com"], &[]);
        config.enabled = true;

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_rejects_global_wildcard_in_managed_allowlist() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["*".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let mut config = network_settings(&["api.example.com"], &[]);
        config.enabled = true;

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_rejects_bracketed_global_wildcard_in_managed_allowlist()
    {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["[*]".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let mut config = network_settings(&["api.example.com"], &[]);
        config.enabled = true;

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_rejects_double_wildcard_bracketed_global_wildcard_in_managed_allowlist()
     {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["**.[*]".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let mut config = network_settings(&["api.example.com"], &[]);
        config.enabled = true;

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_requires_managed_denied_domains_entries() {
        let constraints = NetworkProxyConstraints {
            denied_domains: Some(vec!["evil.com".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            enabled: true,
            ..NetworkProxyConfig::default()
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_expanding_denied_domains_when_fixed() {
        let constraints = NetworkProxyConstraints {
            denied_domains: Some(vec!["evil.com".to_string()]),
            denylist_expansion_enabled: Some(false),
            ..NetworkProxyConstraints::default()
        };

        let mut config = network_settings(&[], &["evil.com", "more-evil.com"]);
        config.enabled = true;

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_enabling_when_managed_disabled() {
        let constraints = NetworkProxyConstraints {
            enabled: Some(false),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            enabled: true,
            ..NetworkProxyConfig::default()
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_allow_local_binding_when_managed_disabled() {
        let constraints = NetworkProxyConstraints {
            allow_local_binding: Some(false),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            enabled: true,
            allow_local_binding: Some(true),
            ..NetworkProxyConfig::default()
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_allow_all_unix_sockets_without_managed_opt_in()
    {
        let constraints = NetworkProxyConstraints {
            dangerously_allow_all_unix_sockets: Some(false),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            enabled: true,
            dangerously_allow_all_unix_sockets: Some(true),
            ..NetworkProxyConfig::default()
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_allow_all_unix_sockets_when_allowlist_is_managed()
     {
        let constraints = NetworkProxyConstraints {
            allow_unix_sockets: Some(vec!["/tmp/allowed.sock".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            enabled: true,
            dangerously_allow_all_unix_sockets: Some(true),
            ..NetworkProxyConfig::default()
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_allows_allow_all_unix_sockets_with_managed_opt_in() {
        let constraints = NetworkProxyConstraints {
            dangerously_allow_all_unix_sockets: Some(true),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            enabled: true,
            dangerously_allow_all_unix_sockets: Some(true),
            ..NetworkProxyConfig::default()
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_ok());
    }

    #[test]
    fn validate_policy_against_constraints_allows_allow_all_unix_sockets_when_unmanaged() {
        let constraints = NetworkProxyConstraints::default();

        let config = NetworkProxyConfig {
            enabled: true,
            dangerously_allow_all_unix_sockets: Some(true),
            ..NetworkProxyConfig::default()
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_ok());
    }

    #[test]
    fn compile_globset_is_case_insensitive() {
        let patterns = vec!["ExAmPle.CoM".to_string()];
        let set = compile_denylist(&patterns).unwrap();
        assert!(set.is_match("example.com"));
        assert!(set.is_match("EXAMPLE.COM"));
    }

    #[test]
    fn compile_globset_excludes_apex_for_subdomain_patterns() {
        let patterns = vec!["*.openai.com".to_string()];
        let set = compile_denylist(&patterns).unwrap();
        assert!(set.is_match("api.openai.com"));
        assert!(!set.is_match("openai.com"));
        assert!(!set.is_match("evilopenai.com"));
    }

    #[test]
    fn compile_globset_includes_apex_for_double_wildcard_patterns() {
        let patterns = vec!["**.openai.com".to_string()];
        let set = compile_denylist(&patterns).unwrap();
        assert!(set.is_match("openai.com"));
        assert!(set.is_match("api.openai.com"));
        assert!(!set.is_match("evilopenai.com"));
    }

    #[test]
    fn compile_globset_rejects_global_wildcard() {
        let patterns = vec!["*".to_string()];
        assert!(compile_denylist(&patterns).is_err());
    }

    #[test]
    fn compile_globset_allows_global_wildcard_when_enabled() {
        let patterns = vec!["*".to_string()];
        let set = compile_allowlist(&patterns).unwrap();
        assert!(set.is_match("example.com"));
        assert!(set.is_match("api.openai.com"));
        assert!(set.is_match("localhost"));
    }

    #[test]
    fn compile_globset_rejects_bracketed_global_wildcard() {
        let patterns = vec!["[*]".to_string()];
        assert!(compile_denylist(&patterns).is_err());
    }

    #[test]
    fn compile_globset_rejects_double_wildcard_bracketed_global_wildcard() {
        let patterns = vec!["**.[*]".to_string()];
        assert!(compile_denylist(&patterns).is_err());
    }

    #[test]
    fn compile_globset_dedupes_patterns_without_changing_behavior() {
        let patterns = vec!["example.com".to_string(), "example.com".to_string()];
        let set = compile_denylist(&patterns).unwrap();
        assert!(set.is_match("example.com"));
        assert!(set.is_match("EXAMPLE.COM"));
        assert!(!set.is_match("not-example.com"));
    }

    #[test]
    fn compile_globset_rejects_invalid_patterns() {
        let patterns = vec!["[".to_string()];
        assert!(compile_denylist(&patterns).is_err());
    }

    #[test]
    fn build_config_state_allows_global_wildcard_allowed_domains() {
        let mut config = network_settings(&["*"], &[]);
        config.enabled = true;

        assert!(
            build_config_state(
                config,
                NetworkProxyConstraints::default(),
                crate::Platform::native()
            )
            .is_ok()
        );
    }

    #[test]
    fn build_config_state_allows_bracketed_global_wildcard_allowed_domains() {
        let mut config = network_settings(&["[*]"], &[]);
        config.enabled = true;

        assert!(
            build_config_state(
                config,
                NetworkProxyConstraints::default(),
                crate::Platform::native()
            )
            .is_ok()
        );
    }

    #[test]
    fn build_config_state_rejects_global_wildcard_denied_domains() {
        let mut config = network_settings(&["example.com"], &["*"]);
        config.enabled = true;

        assert!(
            build_config_state(
                config,
                NetworkProxyConstraints::default(),
                crate::Platform::native()
            )
            .is_err()
        );
    }

    #[test]
    fn build_config_state_rejects_bracketed_global_wildcard_denied_domains() {
        let mut config = network_settings(&["example.com"], &["[*]"]);
        config.enabled = true;

        assert!(
            build_config_state(
                config,
                NetworkProxyConstraints::default(),
                crate::Platform::native()
            )
            .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unix_socket_allowlist_is_respected_on_macos() {
        let socket_path = "/tmp/example.sock".to_string();
        let state = network_proxy_state_for_policy(network_settings_with_unix_sockets(
            &["example.com"],
            &[],
            std::slice::from_ref(&socket_path),
        ));

        assert!(state.is_unix_socket_allowed(&socket_path).await.unwrap());
        assert!(
            !state
                .is_unix_socket_allowed("/tmp/not-allowed.sock")
                .await
                .unwrap()
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unix_socket_allowlist_resolves_symlinks() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;

        let temp_dir = tempdir().unwrap();
        let dir = temp_dir.path();

        let real = dir.join("real.sock");
        let link = dir.join("link.sock");

        // The allowlist mechanism is path-based; for test purposes we don't need an actual unix
        // domain socket. Any filesystem entry works for canonicalization.
        std::fs::write(&real, b"not a socket").unwrap();
        symlink(&real, &link).unwrap();

        let real_s = real.to_str().unwrap().to_string();
        let link_s = link.to_str().unwrap().to_string();

        let state = network_proxy_state_for_policy(network_settings_with_unix_sockets(
            &["example.com"],
            &[],
            std::slice::from_ref(&real_s),
        ));

        assert!(state.is_unix_socket_allowed(&link_s).await.unwrap());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unix_socket_allow_all_flag_bypasses_allowlist() {
        let state = network_proxy_state_for_policy({
            let mut network = network_settings(&["example.com"], &[]);
            network.dangerously_allow_all_unix_sockets = Some(true);
            network
        });

        assert!(state.is_unix_socket_allowed("/tmp/any.sock").await.unwrap());
        assert!(!state.is_unix_socket_allowed("relative.sock").await.unwrap());
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn unix_socket_allowlist_is_rejected_on_non_macos() {
        let socket_path = "/tmp/example.sock".to_string();
        let state = network_proxy_state_for_policy({
            let mut network = network_settings_with_unix_sockets(
                &["example.com"],
                &[],
                std::slice::from_ref(&socket_path),
            );
            network.dangerously_allow_all_unix_sockets = Some(true);
            network
        });

        assert!(!state.is_unix_socket_allowed(&socket_path).await.unwrap());
    }
}

#[cfg(test)]
#[path = "../upstream/host_policy_tests.rs"]
mod host_policy_tests;
