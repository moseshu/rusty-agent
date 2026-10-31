// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
// Upstream declares this constant for `any(target_os = "macos", test)`; the product copy
// keeps the macOS half and the tests supply the rest.
#[cfg(all(test, not(target_os = "macos")))]
const GIT_SSH_COMMAND_ENV_KEY: &str = "GIT_SSH_COMMAND";

#[cfg(test)]
#[path = "../upstream/proxy/managed_routing_tests.rs"]
mod managed_routing_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalBindingPolicy::DefaultFalse;
    use crate::LocalBindingPolicy::RequireTrue;
    use crate::config::NetworkProxyConfig;
    use crate::state::network_proxy_state_for_policy;
    use pretty_assertions::assert_eq;
    use std::net::IpAddr;
    use std::net::Ipv4Addr;
    use std::path::Path;

    #[cfg(target_os = "windows")]
    static WINDOWS_INGRESS_TEST_LOCK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

    fn https_request(host: &str) -> crate::NetworkPolicyRequest {
        crate::NetworkPolicyRequest::new(crate::NetworkPolicyRequestArgs {
            protocol: crate::NetworkProtocol::HttpsConnect,
            host: host.to_string(),
            port: 443,
            environment_id: None,
            client_addr: None,
            method: None,
            command: None,
            exec_policy_hint: None,
        })
    }

    #[tokio::test]
    async fn wait_fails_fast_when_either_listener_fails() {
        for failing_listener in ["http", "socks"] {
            let http_fails = failing_listener == "http";
            let mut listeners = ProxyListeners::new();
            listeners.spawn(move |_guard| async move {
                if http_fails {
                    anyhow::bail!("http proxy listener failed");
                }
                std::future::pending::<Result<()>>().await
            });
            listeners.spawn(move |_guard| async move {
                if !http_fails {
                    anyhow::bail!("socks proxy listener failed");
                }
                std::future::pending::<Result<()>>().await
            });
            let handle = NetworkProxyHandle {
                runtime: Some(ProxyRuntime::Listeners(listeners)),
                environment_proxies: Some(Arc::new(Mutex::new(HashMap::new()))),
                #[cfg(target_os = "windows")]
                windows_active_route: None,
            };

            let error = tokio::time::timeout(std::time::Duration::from_secs(1), handle.wait())
                .await
                .expect("proxy wait should not hang when a listener fails")
                .unwrap_err();

            assert_eq!(
                error.to_string(),
                format!("{failing_listener} proxy listener failed")
            );
        }
    }

    #[tokio::test]
    async fn proxy_startup_ignores_macos_unix_socket_permissions_on_windows() -> Result<()> {
        let unix_sockets = crate::config::NetworkUnixSocketPermissions {
            entries: [
                (
                    "/tmp/allowed.sock".to_string(),
                    crate::config::NetworkUnixSocketPermission::Allow,
                ),
                (
                    "/tmp/denied.sock".to_string(),
                    crate::config::NetworkUnixSocketPermission::Deny,
                ),
            ]
            .into_iter()
            .collect(),
        };
        let config = NetworkProxyConfig {
            enabled: true,
            proxy_url: "http://0.0.0.0:3128".to_string(),
            socks_url: "http://0.0.0.0:8081".to_string(),
            dangerously_allow_non_loopback_proxy: true,
            dangerously_allow_all_unix_sockets: Some(true),
            unix_sockets: Some(unix_sockets.clone()),
            ..NetworkProxyConfig::default()
        };
        let state = Arc::new(network_proxy_state_for_policy(config.clone()));

        let (result, events) = crate::network_policy::test_support::capture_events(|| async {
            NetworkProxy::builder()
                .state(state)
                .managed_by_codex(/*managed_by_codex*/ false)
                .build()
                .await
        })
        .await;
        let proxy = result?;
        let expected_bind_host = if cfg!(target_os = "windows") {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        };

        assert_eq!(
            (proxy.http_addr, proxy.socks_addr),
            (
                format!("{expected_bind_host}:3128").parse::<SocketAddr>()?,
                format!("{expected_bind_host}:8081").parse::<SocketAddr>()?,
            )
        );
        let replacement = crate::state::build_config_state(
            config.clone(),
            Default::default(),
            crate::Platform::native(),
        )?;
        proxy.replace_config_state(replacement).await?;
        let expected_unix_sockets = if cfg!(target_os = "windows") {
            Vec::new()
        } else {
            vec!["/tmp/allowed.sock".to_string()]
        };
        assert_eq!(
            proxy.allow_unix_sockets().as_ref(),
            expected_unix_sockets.as_slice()
        );
        assert_eq!(
            proxy.dangerously_allow_all_unix_sockets(),
            !cfg!(target_os = "windows")
        );
        assert_eq!(proxy.current_cfg().await?, config);

        let remote = proxy.remote_launch_config(DefaultFalse).await?;
        assert_eq!(
            (
                remote.proxy.unix_sockets,
                remote.proxy.dangerously_allow_all_unix_sockets,
            ),
            (Some(unix_sockets), true)
        );
        let emitted_unix_socket_warning = events.iter().any(|event| {
            event.field("message").is_some_and(|message| {
                message.contains("unix socket proxying is enabled")
                    || message.contains("allowUnixSockets")
            })
        });
        assert_eq!(emitted_unix_socket_warning, !cfg!(target_os = "windows"));

        Ok(())
    }

    #[tokio::test]
    async fn remote_policy_decider_rechecks_live_policy_and_restores_attribution() -> Result<()> {
        let captured = Arc::new(Mutex::new(None));
        let captured_request = Arc::clone(&captured);
        let mut config = NetworkProxyConfig {
            allow_local_binding: Some(true),
            ..NetworkProxyConfig::default()
        };
        config.set_allowed_domains(vec!["controller.example".to_string()]);
        let mut owner_config = NetworkProxyConfig::default();
        owner_config.set_allowed_domains(vec!["owner.example".to_string()]);
        let fallback_policy_decider: Arc<dyn NetworkPolicyDecider> =
            Arc::new(move |request: crate::NetworkPolicyRequest| {
                let captured = Arc::clone(&captured_request);
                async move {
                    *captured
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some((request.host, request.environment_id, request.execution_id));
                    crate::NetworkDecision::Allow
                }
            });
        let proxy = NetworkProxy::builder()
            .state(Arc::new(network_proxy_state_for_policy(config)))
            .managed_by_codex(/*managed_by_codex*/ false)
            .build()
            .await?;
        let scoped = proxy.for_execution(
            "remote",
            "execution-1",
            "token-1".to_string(),
            Some(crate::EnvironmentNetworkPolicy::from_config(
                &owner_config,
                /*managed_allowed_domains_only*/ false,
            )),
            Some(Arc::clone(&fallback_policy_decider)),
        )?;
        let decider = scoped
            .remote_policy_decider(/*allow_local_binding*/ true)
            .expect("execution-scoped proxy should expose its policy decider");
        let mut request = https_request("controller.example");
        request.environment_id = Some("forged-environment".to_string());
        request.execution_id = Some("forged-execution".to_string());

        assert_eq!(decider.decide(request).await, crate::NetworkDecision::Allow);
        assert_eq!(
            *captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some((
                "controller.example".to_string(),
                Some("remote".to_string()),
                Some("execution-1".to_string())
            ))
        );

        scoped.add_denied_domain("denied.example").await?;
        assert_eq!(
            decider.decide(https_request("denied.example")).await,
            crate::NetworkDecision::deny_with_source(
                crate::reasons::REASON_DENIED,
                crate::NetworkDecisionSource::BaselinePolicy,
            )
        );
        assert_eq!(
            *captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some((
                "controller.example".to_string(),
                Some("remote".to_string()),
                Some("execution-1".to_string())
            ))
        );

        owner_config.set_denied_domains(vec!["denied.example".to_string()]);
        assert_eq!(
            scoped
                .remote_launch_config(DefaultFalse)
                .await?
                .proxy
                .domains,
            owner_config.domains
        );
        let strict_scoped = proxy.for_execution(
            "owner-environment",
            "execution-3",
            "token-3".to_string(),
            Some(crate::EnvironmentNetworkPolicy::from_config(
                &owner_config,
                /*managed_allowed_domains_only*/ true,
            )),
            Some(fallback_policy_decider),
        )?;
        assert!(
            strict_scoped
                .remote_policy_decider(/*allow_local_binding*/ true)
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn remote_policy_decider_stops_with_execution_scope() -> Result<()> {
        let decision_started = Arc::new(tokio::sync::Notify::new());
        let decision_started_for_decider = Arc::clone(&decision_started);
        let config = NetworkProxyConfig {
            allow_local_binding: Some(true),
            ..NetworkProxyConfig::default()
        };
        let proxy = NetworkProxy::builder()
            .state(Arc::new(network_proxy_state_for_policy(config)))
            .policy_decider(move |_request: crate::NetworkPolicyRequest| {
                let decision_started = Arc::clone(&decision_started_for_decider);
                async move {
                    decision_started.notify_one();
                    std::future::pending::<crate::NetworkDecision>().await
                }
            })
            .managed_by_codex(/*managed_by_codex*/ false)
            .build()
            .await?;
        let scoped = proxy.for_execution(
            "remote",
            "execution-1",
            "token-1".to_string(),
            /*environment_policy*/ None,
            /*fallback_policy_decider*/ None,
        )?;
        let decider = scoped
            .remote_policy_decider(/*allow_local_binding*/ true)
            .expect("execution-scoped proxy should expose its policy decider");
        let request = https_request("pending.example");
        let decision = tokio::spawn(async move { decider.decide(request).await });

        decision_started.notified().await;
        drop(scoped);

        let decision = tokio::time::timeout(std::time::Duration::from_secs(1), decision).await??;
        assert_eq!(
            decision,
            crate::NetworkDecision::deny(crate::reasons::REASON_NOT_ALLOWED)
        );
        Ok(())
    }

    #[tokio::test]
    async fn managed_proxy_builder_uses_loopback_ports() {
        #[cfg(target_os = "windows")]
        let _permit = WINDOWS_INGRESS_TEST_LOCK.acquire().await.unwrap();
        let http_listener = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let http_addr = http_listener.local_addr().unwrap();
        let socks_listener = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let socks_addr = socks_listener.local_addr().unwrap();
        drop(http_listener);
        drop(socks_listener);

        let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
            enabled: true,
            proxy_url: format!("http://{http_addr}"),
            socks_url: format!("http://{socks_addr}"),
            ..NetworkProxyConfig::default()
        }));
        let proxy = match NetworkProxy::builder().state(state).build().await {
            Ok(proxy) => proxy,
            Err(err) => {
                if err
                    .chain()
                    .any(|cause| cause.to_string().contains("Operation not permitted"))
                {
                    return;
                }
                panic!("failed to build managed proxy: {err:#}");
            }
        };

        assert!(proxy.http_addr.ip().is_loopback());
        assert!(proxy.socks_addr.ip().is_loopback());
        #[cfg(target_os = "windows")]
        {
            assert_eq!(proxy.http_addr, http_addr);
            assert_eq!(proxy.socks_addr, socks_addr);
            assert_eq!(proxy.network_proxy_restricting_sid(None), None);
            let (result, events) =
                crate::network_policy::test_support::capture_events(|| async { proxy.run().await })
                    .await;
            let handle = result.expect("start stable ingress route");
            assert!(!events.iter().any(|event| {
                event
                    .field("message")
                    .is_some_and(|message| message.contains("allowUnixSockets"))
            }));
            let second_state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
                enabled: true,
                proxy_url: format!("http://{http_addr}"),
                socks_url: format!("http://{socks_addr}"),
                ..NetworkProxyConfig::default()
            }));
            let second = NetworkProxy::builder()
                .state(second_state)
                .build()
                .await
                .expect("second proxy should share the stable ingress");
            let second_handle = second
                .run()
                .await
                .expect("start second stable ingress route");
            assert_eq!(second.http_addr, proxy.http_addr);
            assert_eq!(second.socks_addr, proxy.socks_addr);
            assert_ne!(
                second.network_proxy_restricting_sid(None),
                proxy.network_proxy_restricting_sid(None)
            );
            let differently_configured =
                Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
                    enabled: true,
                    proxy_url: "http://127.0.0.1:1".to_string(),
                    socks_url: "http://127.0.0.1:2".to_string(),
                    allow_local_binding: Some(true),
                    ..NetworkProxyConfig::default()
                }));
            let third = NetworkProxy::builder()
                .state(differently_configured)
                .build()
                .await
                .expect("different route config should share the stable ingress");
            let third_handle = third
                .run()
                .await
                .expect("start differently configured stable route");
            assert_eq!(third.http_addr, proxy.http_addr);
            assert_eq!(third.socks_addr, proxy.socks_addr);
            assert!(third.allow_local_binding());
            let replacement = crate::state::build_config_state(
                NetworkProxyConfig {
                    enabled: true,
                    proxy_url: format!("http://{http_addr}"),
                    socks_url: format!("http://{socks_addr}"),
                    allow_local_binding: Some(true),
                    ..NetworkProxyConfig::default()
                },
                Default::default(),
                crate::Platform::native(),
            )
            .expect("replacement config state");
            proxy
                .replace_config_state(replacement)
                .await
                .expect("live route should accept a local-binding policy change");
            assert!(proxy.allow_local_binding());
            third_handle
                .shutdown()
                .await
                .expect("stop differently configured stable route");
            second_handle
                .shutdown()
                .await
                .expect("stop second stable ingress route");
            handle.shutdown().await.expect("stop stable ingress route");
            assert_eq!(proxy.network_proxy_restricting_sid(None), None);
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert_ne!(proxy.http_addr.port(), 0);
            assert_ne!(proxy.socks_addr.port(), 0);
        }
    }

    #[tokio::test]
    async fn non_codex_managed_proxy_builder_uses_configured_ports() {
        let settings = NetworkProxyConfig {
            proxy_url: "http://127.0.0.1:43128".to_string(),
            socks_url: "http://127.0.0.1:48081".to_string(),
            ..NetworkProxyConfig::default()
        };
        let state = Arc::new(network_proxy_state_for_policy(settings));
        let proxy = NetworkProxy::builder()
            .state(state)
            .managed_by_codex(/*managed_by_codex*/ false)
            .build()
            .await
            .unwrap();

        assert_eq!(
            proxy.http_addr,
            "127.0.0.1:43128".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            proxy.socks_addr,
            "127.0.0.1:48081".parse::<SocketAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn prepare_for_optional_environment_preserves_effective_unix_socket_permissions()
    -> Result<()> {
        let mut config = NetworkProxyConfig {
            enabled: true,
            proxy_url: "http://127.0.0.1:43128".to_string(),
            socks_url: "http://127.0.0.1:48081".to_string(),
            allow_local_binding: Some(true),
            unix_sockets: Some(crate::config::NetworkUnixSocketPermissions {
                entries: [
                    (
                        "/tmp/allowed.sock".to_string(),
                        crate::config::NetworkUnixSocketPermission::Allow,
                    ),
                    (
                        "/tmp/denied.sock".to_string(),
                        crate::config::NetworkUnixSocketPermission::Deny,
                    ),
                ]
                .into_iter()
                .collect(),
            }),
            ..NetworkProxyConfig::default()
        };
        let proxy = NetworkProxy::builder()
            .state(Arc::new(network_proxy_state_for_policy(config.clone())))
            .managed_by_codex(/*managed_by_codex*/ false)
            .build()
            .await?;

        for allow_all in [false, true] {
            config.dangerously_allow_all_unix_sockets = Some(allow_all);
            let replacement = crate::state::build_config_state(
                config.clone(),
                Default::default(),
                crate::Platform::native(),
            )?;
            proxy.replace_config_state(replacement).await?;
            let prepared = proxy
                .prepare_for_optional_environment(HashMap::new(), /*environment_id*/ None)?;
            assert_eq!(
                prepared.sandbox_context,
                ManagedNetworkSandboxContext {
                    loopback_ports: vec![43128, 48081],
                    allow_local_binding: true,
                    allow_unix_sockets: if cfg!(target_os = "windows") {
                        Vec::new()
                    } else {
                        vec!["/tmp/allowed.sock".to_string()]
                    },
                    dangerously_allow_all_unix_sockets: !cfg!(target_os = "windows") && allow_all,
                }
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn prepare_for_environment_keeps_env_and_sandbox_ports_in_sync() -> Result<()> {
        #[cfg(target_os = "windows")]
        let _permit = WINDOWS_INGRESS_TEST_LOCK.acquire().await.unwrap();
        let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
            enabled: true,
            ..NetworkProxyConfig::default()
        }));
        let proxy = NetworkProxy::builder().state(state).build().await?;
        let handle = proxy.run().await?;

        let base_env = HashMap::from([("PRESERVED".to_string(), "value".to_string())]);
        let local = proxy.prepare_for_optional_environment(base_env.clone(), Some("local"))?;
        let remote = proxy.prepare_for_remote_environment(HashMap::new(), "remote")?;

        assert_eq!(
            local.env.get("PRESERVED").map(String::as_str),
            Some("value")
        );
        #[cfg(target_os = "windows")]
        {
            assert_eq!(
                local.env.get("HTTP_PROXY"),
                Some(&format!("http://{}", proxy.http_addr()))
            );
            assert_ne!(local.env.get("HTTP_PROXY"), remote.env.get("HTTP_PROXY"));
            assert!(proxy.network_proxy_restricting_sid(Some("local")).is_some());
            assert_eq!(proxy.network_proxy_restricting_sid(Some("remote")), None);
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert_ne!(local.env.get("HTTP_PROXY"), remote.env.get("HTTP_PROXY"));
            assert_ne!(
                local.env.get("HTTP_PROXY"),
                Some(&format!("http://{}", proxy.http_addr()))
            );
            assert_ne!(
                remote.env.get("HTTP_PROXY"),
                Some(&format!("http://{}", proxy.http_addr()))
            );
        }
        for prepared in [&local, &remote] {
            let http_port = prepared
                .env
                .get("HTTP_PROXY")
                .and_then(|value| value.strip_prefix("http://"))
                .and_then(|value| value.parse::<SocketAddr>().ok())
                .map(|addr| addr.port())
                .expect("managed HTTP proxy address");
            let socks_port = prepared
                .env
                .get("ALL_PROXY")
                .and_then(|value| value.strip_prefix("socks5h://"))
                .and_then(|value| value.parse::<SocketAddr>().ok())
                .map(|addr| addr.port())
                .expect("managed SOCKS proxy address");
            let mut expected_ports = vec![http_port, socks_port];
            expected_ports.sort_unstable();
            expected_ports.dedup();
            assert_eq!(
                prepared.sandbox_context,
                ManagedNetworkSandboxContext {
                    loopback_ports: expected_ports,
                    allow_local_binding: false,
                    ..ManagedNetworkSandboxContext::default()
                }
            );
        }
        let mut legacy_env = base_env;
        proxy.apply_to_env_for_environment(&mut legacy_env, "local")?;
        assert_eq!(legacy_env, local.env);

        handle.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn private_credential_context_prepares_spawn_ready_env() -> Result<()> {
        #[cfg(target_os = "windows")]
        let _permit = WINDOWS_INGRESS_TEST_LOCK.acquire().await.unwrap();
        let mut config = NetworkProxyConfig {
            enabled: true,
            ..NetworkProxyConfig::default()
        };
        config.set_credential_broker_enabled(/*enabled*/ true);
        let state = Arc::new(network_proxy_state_for_policy(config));
        let proxy = NetworkProxy::builder()
            .state(Arc::clone(&state))
            .build()
            .await?;
        let handle = proxy.run().await?;
        let real = "ghp-private-context-test";
        for previous in [None, Some(""), Some("visible.example")] {
            let mut env = HashMap::from([("GH_ENTERPRISE_TOKEN".to_string(), real.to_string())]);
            if let Some(previous) = previous {
                env.insert("GH_HOST".to_string(), previous.to_string());
            }
            let context = crate::CredentialBrokerContext::from(HashMap::from([(
                "GH_HOST".to_string(),
                "private.enterprise.example".to_string(),
            )]));
            let prepared =
                context.prepare_child_environment(&proxy, env, Some("snapshot-context"))?;
            assert_eq!(prepared.env.get("GH_HOST").map(String::as_str), previous);
            let dummy = &prepared.env["GH_ENTERPRISE_TOKEN"];
            assert_ne!(dummy, real);
            for (host, expected) in [
                ("private.enterprise.example", real),
                ("visible.example", dummy.as_str()),
            ] {
                let mut headers = rama_http::HeaderMap::new();
                headers.insert(
                    rama_http::header::AUTHORIZATION,
                    format!("Bearer {dummy}").parse()?,
                );
                state
                    .for_environment_id(Some("snapshot-context"))
                    .inject_request_credentials(host, &mut headers);
                assert_eq!(
                    headers[rama_http::header::AUTHORIZATION],
                    format!("Bearer {expected}")
                );
            }
        }
        handle.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn remote_launch_config_carries_execution_scope() -> Result<()> {
        #[cfg(target_os = "windows")]
        let _permit = WINDOWS_INGRESS_TEST_LOCK.acquire().await.unwrap();
        let mut config = NetworkProxyConfig {
            dangerously_allow_plaintext_credential_injection: true,
            dangerously_allow_all_unix_sockets: Some(false),
            allow_local_binding: Some(false),
            ..NetworkProxyConfig::default()
        };
        config.set_allow_unix_sockets(Vec::new());
        config.set_credential_broker_enabled(/*enabled*/ true);
        config.configure_credential_broker_environment(&HashMap::from([(
            "GH_HOST".to_string(),
            "local-github.example".to_string(),
        )]));
        let state = Arc::new(network_proxy_state_for_policy(config));
        let proxy = match NetworkProxy::builder().state(state).build().await {
            Ok(proxy) => proxy,
            Err(err) => {
                if err
                    .chain()
                    .any(|cause| cause.to_string().contains("Operation not permitted"))
                {
                    return Ok(());
                }
                return Err(err);
            }
        };

        let scoped = proxy.for_execution(
            "remote-env",
            "execution-1",
            "token-1".to_string(),
            /*environment_policy*/ None,
            /*fallback_policy_decider*/ None,
        )?;
        let launch = scoped.remote_launch_config(RequireTrue).await?;
        let prepared = scoped.prepare_for_optional_environment(
            HashMap::from([(
                PROXY_ATTRIBUTION_TOKEN_ENV_KEY.to_string(),
                "foreign-token".to_string(),
            )]),
            /*environment_id*/ None,
        )?;

        assert_eq!(launch.environment_id.as_deref(), Some("remote-env"));
        assert_eq!(launch.execution_id.as_deref(), Some("execution-1"));
        assert!(!launch.proxy.enabled);

        let mut owner_config = NetworkProxyConfig::default();
        owner_config.set_allowed_domains(vec!["owner.example".to_string()]);
        let owner_scoped = proxy.for_execution(
            "remote-env",
            "execution-2",
            "token-2".to_string(),
            Some(crate::EnvironmentNetworkPolicy::from_config(
                &owner_config,
                /*managed_allowed_domains_only*/ false,
            )),
            /*fallback_policy_decider*/ None,
        )?;
        assert!(
            owner_scoped
                .remote_launch_config(DefaultFalse)
                .await
                .is_err()
        );
        for (enabled, mode, allowed_domain, proxy_url) in [
            (false, config::NetworkMode::Full, Some("*"), None),
            (false, config::NetworkMode::Full, Some("example.com"), None),
            (
                false,
                config::NetworkMode::Full,
                None,
                Some("http://127.0.0.1:8123"),
            ),
            (true, config::NetworkMode::Full, None, None),
            (true, config::NetworkMode::Limited, None, None),
        ] {
            let mut config = NetworkProxyConfig {
                enabled,
                mode,
                ..NetworkProxyConfig::default()
            };
            if let Some(domain) = allowed_domain {
                config.set_allowed_domains(vec![domain.to_string()]);
            }
            if let Some(proxy_url) = proxy_url {
                config.proxy_url = proxy_url.to_string();
            }
            config.set_credential_broker_enabled(/*enabled*/ true);
            let proxy = NetworkProxy::builder()
                .state(Arc::new(network_proxy_state_for_policy(config)))
                .build()
                .await?;
            let launch = proxy.remote_launch_config(DefaultFalse).await?;
            assert!(launch.proxy.enabled);
            assert_eq!(launch.proxy.mode, mode);
        }
        assert_eq!(
            prepared
                .env
                .get(PROXY_ATTRIBUTION_TOKEN_ENV_KEY)
                .map(String::as_str),
            Some("token-1")
        );
        Ok(())
    }

    #[tokio::test]
    async fn managed_proxy_builder_lazily_upgrades_disabled_socks() {
        #[cfg(target_os = "windows")]
        let _permit = WINDOWS_INGRESS_TEST_LOCK.acquire().await.unwrap();
        let http_listener = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let http_addr = http_listener.local_addr().unwrap();
        drop(http_listener);
        let occupied_socks = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let socks_addr = occupied_socks.local_addr().unwrap();
        let settings = NetworkProxyConfig {
            enabled: true,
            enable_socks5: false,
            proxy_url: format!("http://{http_addr}"),
            socks_url: format!("http://{socks_addr}"),
            ..NetworkProxyConfig::default()
        };
        let state = Arc::new(network_proxy_state_for_policy(settings));
        let proxy = match NetworkProxy::builder().state(state).build().await {
            Ok(proxy) => proxy,
            Err(err) => {
                if err
                    .chain()
                    .any(|cause| cause.to_string().contains("Operation not permitted"))
                {
                    return;
                }
                panic!("failed to build managed proxy: {err:#}");
            }
        };

        assert!(proxy.http_addr.ip().is_loopback());
        assert_ne!(proxy.http_addr.port(), 0);
        assert_eq!(proxy.socks_addr, socks_addr);
        #[cfg(target_os = "windows")]
        {
            assert_eq!(proxy.http_addr, http_addr);
            assert!(proxy.reserved_listeners.is_none());
            assert!(proxy.windows_runtime.is_some());
            assert_eq!(proxy.network_proxy_restricting_sid(None), None);
            let handle = proxy.run().await.expect("start HTTP-only stable route");
            assert!(proxy.network_proxy_restricting_sid(None).is_some());
            let prepared_before_upgrade = proxy
                .prepare_for_optional_environment(
                    HashMap::from([(
                        WINDOWS_SANDBOX_PROXY_PORTS_ENV_KEY.to_string(),
                        "1,2".to_string(),
                    )]),
                    None,
                )
                .expect("prepare stable Windows proxy");
            assert_eq!(
                prepared_before_upgrade.sandbox_context.loopback_ports,
                vec![proxy.http_addr.port()]
            );
            assert_eq!(
                prepared_before_upgrade
                    .env
                    .get(WINDOWS_SANDBOX_PROXY_PORTS_ENV_KEY),
                Some(&proxy.http_addr.port().to_string())
            );
            assert_eq!(
                prepared_before_upgrade.env.get("ALL_PROXY"),
                Some(&format!("http://{}", proxy.http_addr))
            );
            let environment_id = "cached-before-socks-upgrade";
            let environment_before_upgrade = proxy
                .prepare_for_optional_environment(HashMap::new(), Some(environment_id))
                .expect("prepare cached HTTP-only environment");
            assert_eq!(
                environment_before_upgrade.sandbox_context.loopback_ports,
                vec![proxy.http_addr.port()]
            );

            let requested_socks =
                StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
            let requested_socks_addr = requested_socks.local_addr().unwrap();
            assert_ne!(requested_socks_addr, socks_addr);
            let socks_state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
                enabled: true,
                enable_socks5: true,
                proxy_url: format!("http://{http_addr}"),
                socks_url: format!("socks5://{requested_socks_addr}"),
                ..NetworkProxyConfig::default()
            }));
            let socks_proxy = NetworkProxy::builder()
                .state(socks_state)
                .build()
                .await
                .expect("upgrade stable ingress to SOCKS5");
            let actual_socks_addr = socks_proxy.socks_addr();
            assert_eq!(socks_proxy.http_addr(), proxy.http_addr());
            assert!(actual_socks_addr.ip().is_loopback());
            assert_ne!(actual_socks_addr, requested_socks_addr);
            assert!(WINDOWS_MANAGED_SOCKS_PROXY_PORTS.contains(&actual_socks_addr.port()));
            assert_eq!(proxy.socks_addr(), actual_socks_addr);
            let socks_handle = socks_proxy
                .run()
                .await
                .expect("start SOCKS-enabled stable route");

            let mut expected_ports = vec![proxy.http_addr.port(), actual_socks_addr.port()];
            expected_ports.sort_unstable();
            let prepared_after_upgrade = proxy
                .prepare_for_optional_environment(HashMap::new(), None)
                .expect("re-prepare HTTP-only route after SOCKS5 upgrade");
            let environment_after_upgrade = proxy
                .prepare_for_optional_environment(HashMap::new(), Some(environment_id))
                .expect("re-prepare cached environment after SOCKS5 upgrade");
            for prepared in [&prepared_after_upgrade, &environment_after_upgrade] {
                assert_eq!(prepared.sandbox_context.loopback_ports, expected_ports);
                assert_eq!(
                    prepared.env.get(WINDOWS_SANDBOX_PROXY_PORTS_ENV_KEY),
                    Some(
                        &expected_ports
                            .iter()
                            .map(u16::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                );
                assert_eq!(
                    prepared.env.get("ALL_PROXY"),
                    Some(&format!("http://{}", proxy.http_addr))
                );
            }
            let socks_prepared = socks_proxy
                .prepare_for_optional_environment(HashMap::new(), None)
                .expect("prepare SOCKS-enabled route");
            assert_eq!(
                socks_prepared.sandbox_context.loopback_ports,
                expected_ports
            );
            assert_eq!(
                socks_prepared.env.get("ALL_PROXY"),
                Some(&format!("socks5h://{actual_socks_addr}"))
            );
            let remote = proxy
                .prepare_for_remote_environment(
                    HashMap::from([(
                        WINDOWS_SANDBOX_PROXY_PORTS_ENV_KEY.to_string(),
                        "1,2".to_string(),
                    )]),
                    "remote",
                )
                .expect("prepare HTTP-only trusted bridge proxy");
            assert_eq!(remote.sandbox_context.loopback_ports.len(), 1);
            assert_eq!(remote.env.get(WINDOWS_SANDBOX_PROXY_PORTS_ENV_KEY), None);
            assert_eq!(proxy.network_proxy_restricting_sid(Some("remote")), None);
            socks_handle
                .shutdown()
                .await
                .expect("stop SOCKS-enabled stable route");
            handle
                .shutdown()
                .await
                .expect("stop HTTP-only stable route");
            assert_eq!(proxy.network_proxy_restricting_sid(None), None);
        }
        #[cfg(not(target_os = "windows"))]
        assert!(
            proxy
                .reserved_listeners
                .as_ref()
                .expect("managed builder should reserve listeners")
                .take_socks()
                .is_none()
        );
        drop(proxy);
        drop(occupied_socks);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_managed_loopback_addr_clamps_non_loopback_inputs() {
        assert_eq!(
            windows_managed_loopback_addr("0.0.0.0:3128".parse::<SocketAddr>().unwrap()),
            "127.0.0.1:3128".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            windows_managed_loopback_addr("[::]:8081".parse::<SocketAddr>().unwrap()),
            "127.0.0.1:8081".parse::<SocketAddr>().unwrap()
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reserve_windows_managed_listeners_falls_back_when_http_port_is_busy() {
        let occupied = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let busy_port = occupied.local_addr().unwrap().port();

        let reserved = reserve_windows_managed_listeners(
            SocketAddr::from(([127, 0, 0, 1], busy_port)),
            SocketAddr::from(([127, 0, 0, 1], 48081)),
            /*reserve_socks_listener*/ false,
        )
        .unwrap();

        assert!(reserved.socks_listener.is_none());
        assert!(
            reserved
                .http_listener
                .local_addr()
                .unwrap()
                .ip()
                .is_loopback()
        );
        let fallback_port = reserved.http_listener.local_addr().unwrap().port();
        assert_ne!(fallback_port, busy_port);
        assert!(WINDOWS_MANAGED_HTTP_PROXY_PORTS.contains(&fallback_port));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reserve_windows_managed_listeners_preserves_http_when_socks_port_is_busy() {
        let http_listener = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let http_addr = http_listener.local_addr().unwrap();
        drop(http_listener);
        let occupied_socks = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();

        let reserved = reserve_windows_managed_listeners(
            http_addr,
            occupied_socks.local_addr().unwrap(),
            /*reserve_socks_listener*/ true,
        )
        .unwrap();

        assert_eq!(reserved.http_listener.local_addr().unwrap(), http_addr);
        assert!(
            WINDOWS_MANAGED_SOCKS_PROXY_PORTS.contains(
                &reserved
                    .socks_listener
                    .unwrap()
                    .local_addr()
                    .unwrap()
                    .port()
            )
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reserve_windows_managed_listener_uses_ephemeral_port_when_preferred_ports_are_busy() {
        let occupied = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let occupied_addr = occupied.local_addr().unwrap();
        let occupied_port = occupied_addr.port();

        let listener =
            reserve_windows_managed_listener(occupied_addr, occupied_port..=occupied_port, "HTTP")
                .unwrap();

        let fallback_addr = listener.local_addr().unwrap();
        assert!(fallback_addr.ip().is_loopback());
        assert_ne!(fallback_addr.port(), occupied_port);
    }

    #[test]
    fn proxy_url_env_value_resolves_lowercase_aliases() {
        let mut env = HashMap::new();
        env.insert(
            "http_proxy".to_string(),
            "http://127.0.0.1:3128".to_string(),
        );

        assert_eq!(
            proxy_url_env_value(&env, "HTTP_PROXY"),
            Some("http://127.0.0.1:3128")
        );
    }

    #[test]
    fn has_proxy_url_env_vars_detects_lowercase_aliases() {
        let mut env = HashMap::new();
        env.insert(
            "all_proxy".to_string(),
            "socks5h://127.0.0.1:8081".to_string(),
        );

        assert_eq!(has_proxy_url_env_vars(&env), true);
    }

    #[test]
    fn has_proxy_url_env_vars_detects_websocket_proxy_keys() {
        let mut env = HashMap::new();
        env.insert("wss_proxy".to_string(), "http://127.0.0.1:3128".to_string());

        assert_eq!(has_proxy_url_env_vars(&env), true);
    }

    #[test]
    fn apply_proxy_env_overrides_sets_common_tool_vars() {
        let mut env = HashMap::new();
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
            /*mitm_ca_trust_bundle*/ None,
        );

        assert_eq!(
            env.get("HTTP_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("WS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("WSS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("npm_config_proxy"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("ALL_PROXY"),
            Some(&"socks5h://127.0.0.1:8081".to_string())
        );
        assert_eq!(
            env.get("FTP_PROXY"),
            Some(&"socks5h://127.0.0.1:8081".to_string())
        );
        assert_eq!(env.get("NO_PROXY"), Some(&String::new()));
        assert_eq!(env.get(PROXY_ACTIVE_ENV_KEY), Some(&"1".to_string()));
        assert_eq!(env.get(ALLOW_LOCAL_BINDING_ENV_KEY), Some(&"0".to_string()));
        assert_eq!(
            env.get(ELECTRON_GET_USE_PROXY_ENV_KEY),
            Some(&"true".to_string())
        );
        assert_eq!(env.get(NODE_USE_ENV_PROXY_ENV_KEY), Some(&"1".to_string()));
        #[cfg(target_os = "macos")]
        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(
                &"CODEX_PROXY_GIT_SSH_COMMAND=1 ssh -o ProxyCommand='nc -X 5 -x 127.0.0.1:8081 %h %p'"
                    .to_string()
            )
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(env.get(GIT_SSH_COMMAND_ENV_KEY), None);
    }

    #[test]
    fn apply_proxy_env_overrides_keeps_local_targets_direct_when_local_binding_enabled() {
        let mut env = HashMap::new();
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ true,
            /*mitm_ca_trust_bundle*/ None,
        );

        assert_eq!(
            env.get("NO_PROXY"),
            Some(&DEFAULT_NO_PROXY_VALUE.to_string())
        );
    }

    #[test]
    fn apply_proxy_env_overrides_sets_only_expected_env_keys() {
        let mut env = HashMap::new();
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
            /*mitm_ca_trust_bundle*/ None,
        );

        for key in env.keys() {
            let is_managed_git_ssh_key =
                cfg!(target_os = "macos") && key == GIT_SSH_COMMAND_ENV_KEY;
            assert!(
                PROXY_ENV_KEYS.contains(&key.as_str()) || is_managed_git_ssh_key,
                "proxy env writer set unexpected key: {key}"
            );
        }
    }

    #[test]
    fn apply_proxy_env_overrides_sets_mitm_ca_trust_bundle_vars() {
        let mut env = HashMap::new();
        let mitm_ca_trust_bundle_path = Path::new("/tmp/codex-proxy/ca-bundle.pem");
        let mitm_ca_trust_bundle = crate::certs::ManagedMitmCaTrustBundle {
            path: mitm_ca_trust_bundle_path.to_path_buf(),
            startup_env_values: HashMap::new(),
        };
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
            Some(&mitm_ca_trust_bundle),
        );

        for key in crate::certs::CUSTOM_CA_ENV_KEYS {
            assert_eq!(
                env.get(key),
                Some(&mitm_ca_trust_bundle_path.display().to_string())
            );
        }
    }

    #[test]
    fn apply_proxy_env_overrides_preserves_command_scoped_mitm_ca_override() {
        let command_ca_bundle_path = "/tmp/command-ca.pem".to_string();
        let mut env = HashMap::from([(
            "REQUESTS_CA_BUNDLE".to_string(),
            command_ca_bundle_path.clone(),
        )]);
        let mitm_ca_trust_bundle_path = Path::new("/tmp/codex-proxy/ca-bundle.pem");
        let mitm_ca_trust_bundle = crate::certs::ManagedMitmCaTrustBundle {
            path: mitm_ca_trust_bundle_path.to_path_buf(),
            startup_env_values: HashMap::new(),
        };

        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
            Some(&mitm_ca_trust_bundle),
        );

        assert_eq!(env.get("REQUESTS_CA_BUNDLE"), Some(&command_ca_bundle_path));
        assert_eq!(
            env.get("SSL_CERT_FILE"),
            Some(&mitm_ca_trust_bundle_path.display().to_string())
        );
    }

    #[test]
    fn apply_proxy_env_overrides_uses_http_for_all_proxy_without_socks() {
        let mut env = HashMap::new();
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ false,
            /*allow_local_binding*/ true,
            /*mitm_ca_trust_bundle*/ None,
        );

        assert_eq!(
            env.get("ALL_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(env.get(ALLOW_LOCAL_BINDING_ENV_KEY), Some(&"1".to_string()));
    }

    #[test]
    fn apply_proxy_env_overrides_uses_plain_http_proxy_url() {
        let mut env = HashMap::new();
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
            /*mitm_ca_trust_bundle*/ None,
        );

        assert_eq!(
            env.get("HTTP_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("HTTPS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("WS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("WSS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("ALL_PROXY"),
            Some(&"socks5h://127.0.0.1:8081".to_string())
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(
                &"CODEX_PROXY_GIT_SSH_COMMAND=1 ssh -o ProxyCommand='nc -X 5 -x 127.0.0.1:8081 %h %p'"
                    .to_string()
            )
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(env.get(GIT_SSH_COMMAND_ENV_KEY), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apply_proxy_env_overrides_preserves_existing_git_ssh_command() {
        let mut env = HashMap::new();
        env.insert(
            GIT_SSH_COMMAND_ENV_KEY.to_string(),
            "ssh -o ProxyCommand='tsh proxy ssh --cluster=dev %r@%h:%p'".to_string(),
        );
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
            /*mitm_ca_trust_bundle*/ None,
        );

        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(&"ssh -o ProxyCommand='tsh proxy ssh --cluster=dev %r@%h:%p'".to_string())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apply_proxy_env_overrides_preserves_unmarked_git_ssh_command_with_proxy_shape() {
        let mut env = HashMap::new();
        env.insert(
            GIT_SSH_COMMAND_ENV_KEY.to_string(),
            "ssh -o ProxyCommand='nc -X 5 -x 127.0.0.1:8081 %h %p'".to_string(),
        );
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 48081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
            /*mitm_ca_trust_bundle*/ None,
        );

        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(&"ssh -o ProxyCommand='nc -X 5 -x 127.0.0.1:8081 %h %p'".to_string())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apply_proxy_env_overrides_refreshes_previous_codex_proxy_git_ssh_command() {
        let mut env = HashMap::new();
        env.insert(
            GIT_SSH_COMMAND_ENV_KEY.to_string(),
            codex_proxy_git_ssh_command(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081)),
        );

        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 43128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 48081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
            /*mitm_ca_trust_bundle*/ None,
        );

        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(&codex_proxy_git_ssh_command(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                48081,
            )))
        );
    }
}
