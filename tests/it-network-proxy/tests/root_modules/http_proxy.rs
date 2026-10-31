// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#[cfg(test)]
mod tests {
    use super::*;

    use crate::CredentialProviderConfig;
    use crate::config::NetworkMode;
    use crate::config::NetworkProxyConfig;
    use crate::runtime::network_proxy_state_for_policy;
    use pretty_assertions::assert_eq;
    use rama_http::Method;
    use rama_http::Request;
    use std::collections::BTreeMap;
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::net::TcpListener as StdTcpListener;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener as TokioTcpListener;
    use tokio::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn http_connect_accept_blocks_in_limited_mode() {
        let policy = {
            let mut policy = NetworkProxyConfig::default();
            policy.set_allowed_domains(vec!["example.com".to_string()]);
            policy
        };
        let state = Arc::new(network_proxy_state_for_policy(policy));
        state.set_network_mode(NetworkMode::Limited).await.unwrap();

        let mut req = Request::builder()
            .method(Method::CONNECT)
            .uri("https://example.com:443")
            .header("host", "example.com:443")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(state);

        let response = http_connect_accept(
            /*policy_decider*/ None, /*environment_id*/ None, req,
        )
        .await
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get("x-proxy-error").unwrap(),
            "blocked-by-mitm-required"
        );
    }

    #[tokio::test]
    async fn http_connect_accept_allows_allowlisted_host_in_full_mode() {
        let policy = {
            let mut policy = NetworkProxyConfig {
                allow_local_binding: Some(true),
                ..NetworkProxyConfig::default()
            };
            policy.set_allowed_domains(vec!["example.com".to_string()]);
            policy
        };
        let state = Arc::new(network_proxy_state_for_policy(policy));

        let mut req = Request::builder()
            .method(Method::CONNECT)
            .uri("https://example.com:443")
            .header("host", "example.com:443")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(state);

        let (response, _request) = http_connect_accept(
            /*policy_decider*/ None, /*environment_id*/ None, req,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn http_connect_accept_passes_environment_id_to_decider() {
        let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig::default()));
        let seen_environment_id = Arc::new(Mutex::new(None));
        let decider: Arc<dyn NetworkPolicyDecider> = Arc::new({
            let seen_environment_id = seen_environment_id.clone();
            move |request: NetworkPolicyRequest| {
                *seen_environment_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = request.environment_id;
                async { NetworkDecision::Allow }
            }
        });

        let mut req = Request::builder()
            .method(Method::CONNECT)
            .uri("https://example.com:443")
            .header("host", "example.com:443")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(state);

        let (response, _request) =
            http_connect_accept(Some(decider), Some("remote".to_string()), req)
                .await
                .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            seen_environment_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_deref(),
            Some("remote")
        );
    }

    #[tokio::test]
    async fn http_connect_accept_defers_brokered_host_mitm_until_protocol_detection() {
        let mut policy = NetworkProxyConfig {
            credential_broker: true,
            mitm: true,
            ..NetworkProxyConfig::default()
        };
        policy.set_allowed_domains(vec!["github.com".to_string()]);
        let state = Arc::new(network_proxy_state_for_policy(policy));
        let mut env = HashMap::from([("GH_TOKEN".to_string(), "ghp-real".to_string())]);
        state.virtualize_child_credentials(&mut env);

        let mut req = Request::builder()
            .method(Method::CONNECT)
            .uri("https://github.com:22")
            .header("host", "github.com:22")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(state);

        let (response, request) = http_connect_accept(
            /*policy_decider*/ None, /*environment_id*/ None, req,
        )
        .await
        .expect("brokered credentials should defer MITM until protocol detection");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            request.extensions().get::<ConnectMitmMode>().copied(),
            Some(ConnectMitmMode::DetectProtocol(
                crate::brokered_tunnel::BrokeredProtocols {
                    tls: true,
                    http: false
                }
            ))
        );
    }

    #[tokio::test]
    async fn plaintext_credential_injection_requires_explicit_opt_in() {
        let real_token = "ghp-real";
        for enabled in [false, true] {
            let state = network_proxy_state_for_policy(NetworkProxyConfig {
                credential_broker: true,
                dangerously_allow_plaintext_credential_injection: enabled,
                mitm: true,
                ..NetworkProxyConfig::default()
            });
            let mut env = HashMap::from([("GH_TOKEN".to_string(), real_token.to_string())]);
            state.virtualize_child_credentials(&mut env);
            let dummy = env.get("GH_TOKEN").expect("dummy GitHub token");
            let mut req = Request::builder()
                .uri("http://api.github.com/")
                .header(header::AUTHORIZATION, format!("Bearer {dummy}"))
                .body(Body::empty())
                .unwrap();
            let context = RequestContext::try_from(&req).unwrap();

            for destination in forward_request_credential_destinations(&state, &context, &req)
                .await
                .unwrap()
            {
                state.inject_request_credentials(&destination, req.headers_mut());
            }

            let expected = if enabled { real_token } else { dummy };
            assert_eq!(
                req.headers()[header::AUTHORIZATION],
                format!("Bearer {expected}")
            );
        }
    }

    #[tokio::test]
    async fn configured_forward_requests_match_destination_scheme_and_port() {
        let real_token = "provider_abcdefghijklmnopqrstuvwx";
        for (host, port, prefix, scope) in [
            ("localhost", 8080, ""),
            ("127.0.0.1", 8081, ""),
            ("api.provider.example", 443, "https://"),
            ("api.provider.example", 8443, "https://"),
        ]
        .into_iter()
        .flat_map(|(host, port, prefix)| ["/v1", "/"].map(move |scope| (host, port, prefix, scope)))
        {
            let state = network_proxy_state_for_policy(NetworkProxyConfig {
                credential_broker: true,
                dangerously_allow_plaintext_credential_injection: true,
                credential_providers: BTreeMap::from([(
                    "custom".to_string(),
                    CredentialProviderConfig {
                        env: vec!["PROVIDER_TOKEN".to_string()],
                        patterns: vec!["^provider_[a-z]{24}$".to_string()],
                        url_prefix_from_env: Some("PROVIDER_URL".to_string()),
                        ..CredentialProviderConfig::default()
                    },
                )]),
                ..NetworkProxyConfig::default()
            });
            let mut env = HashMap::from([
                ("PROVIDER_TOKEN".to_string(), real_token.to_string()),
                (
                    "PROVIDER_URL".to_string(),
                    format!("{prefix}{host}:{port}{scope}"),
                ),
            ]);
            state.virtualize_child_credentials(&mut env);
            let dummy = env.get("PROVIDER_TOKEN").expect("dummy provider token");
            assert_ne!(dummy, real_token);
            assert_eq!(
                state.host_mitm_requirement(host, port).await.unwrap(),
                HostMitmRequirement::Credential(crate::brokered_tunnel::BrokeredProtocols {
                    tls: !prefix.is_empty(),
                    http: prefix.is_empty()
                })
            );
            assert_eq!(
                state.host_mitm_requirement(host, port + 1).await.unwrap(),
                HostMitmRequirement::None
            );
            assert_eq!(
                state
                    .for_environment_id(Some("other"))
                    .host_mitm_requirement(host, port)
                    .await
                    .unwrap(),
                HostMitmRequirement::None
            );
            for (uri, inject) in [
                (format!("http://{host}:{port}/v1/models"), prefix.is_empty()),
                (format!("http://{host}:{}/v1/models", port + 1), false),
                (format!("https://{host}:{}/v1/models", port + 1), false),
                (
                    format!("https://{host}:{port}/v1/models"),
                    !prefix.is_empty(),
                ),
                ("/v1/models".to_string(), prefix.is_empty()),
                (
                    format!("https://{host}:{port}/other"),
                    !prefix.is_empty() && scope == "/",
                ),
                ("*".to_string(), prefix.is_empty() && scope == "/"),
            ] {
                let request_uri: rama_http::Uri = uri.parse().unwrap();
                let host_header = request_uri
                    .authority()
                    .map(|authority| authority.as_str().to_string())
                    .unwrap_or_else(|| format!("{host}:{port}"));
                let mut req = Request::builder()
                    .method(if uri == "*" {
                        Method::OPTIONS
                    } else {
                        Method::GET
                    })
                    .uri(request_uri)
                    .header(header::HOST, host_header)
                    .header(header::AUTHORIZATION, format!("Bearer {dummy}"))
                    .body(Body::empty())
                    .unwrap();
                let context = RequestContext::try_from(&req).unwrap();

                for destination in forward_request_credential_destinations(&state, &context, &req)
                    .await
                    .unwrap()
                {
                    state.inject_request_credentials(&destination, req.headers_mut());
                }

                let expected = if inject { real_token } else { dummy };
                assert_eq!(
                    req.headers()[header::AUTHORIZATION],
                    format!("Bearer {expected}"),
                    "request: {uri}, configured scope: {scope}"
                );
                assert_eq!(req.uri().to_string(), uri);
            }
        }
    }

    #[tokio::test]
    async fn http_plain_proxy_blocks_brokered_provider_alias_before_credential_injection() {
        let mut network = NetworkProxyConfig {
            allow_local_binding: Some(true),
            credential_broker: true,
            dangerously_allow_plaintext_credential_injection: true,
            mitm: true,
            mitm_hooks: vec![crate::mitm_hook::MitmHookConfig {
                host: "api.github.com".to_string(),
                matcher: crate::mitm_hook::MitmHookMatchConfig {
                    methods: vec!["POST".to_string()],
                    path_prefixes: vec!["/repos/openai/".to_string()],
                    ..crate::mitm_hook::MitmHookMatchConfig::default()
                },
                actions: crate::mitm_hook::MitmHookActionsConfig::default(),
            }],
            ..NetworkProxyConfig::default()
        };
        network.set_allowed_domains(vec!["api.github.com".to_string(), "github.com".to_string()]);
        let state = Arc::new(network_proxy_state_for_policy(network));
        let mut env = HashMap::from([("GH_TOKEN".to_string(), "ghp-real".to_string())]);
        state.virtualize_child_credentials(&mut env);

        for scheme in ["http", "https"] {
            let mut req = Request::builder()
                .method(Method::GET)
                .uri(format!("{scheme}://github.com/repos/openai/codex/issues"))
                .header(header::HOST, "github.com")
                .header(header::AUTHORIZATION, format!("Bearer {}", env["GH_TOKEN"]))
                .body(Body::empty())
                .expect("absolute-form provider-alias request should build");
            req.extensions_mut().insert(state.clone());

            let response = http_plain_proxy(
                /*policy_decider*/ None, /*environment_id*/ None, req,
            )
            .await
            .expect("provider-alias policy should return a response");

            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(
                response.headers().get("x-proxy-error").unwrap(),
                "blocked-by-mitm-hook"
            );
        }

        let blocked = state.drain_blocked().await.unwrap();
        assert_eq!(blocked.len(), 2);
        assert!(blocked.iter().all(|request| {
            request.host == "github.com"
                && request.reason == REASON_MITM_HOOK_DENIED
                && request.protocol == "http"
                && request.source.as_deref() == Some("mode_guard")
        }));
        assert_eq!(blocked[0].port, Some(80));
        assert_eq!(blocked[1].port, Some(443));
    }

    #[tokio::test]
    async fn http_connect_accept_blocks_hooked_host_in_full_mode_without_mitm_state() {
        let mut policy = NetworkProxyConfig {
            mitm: true,
            mitm_hooks: vec![crate::mitm_hook::MitmHookConfig {
                host: "api.github.com".to_string(),
                matcher: crate::mitm_hook::MitmHookMatchConfig {
                    methods: vec!["POST".to_string()],
                    path_prefixes: vec!["/repos/openai/".to_string()],
                    ..crate::mitm_hook::MitmHookMatchConfig::default()
                },
                actions: crate::mitm_hook::MitmHookActionsConfig::default(),
            }],
            ..Default::default()
        };
        policy.set_allowed_domains(vec!["api.github.com".to_string()]);
        let state = Arc::new(network_proxy_state_for_policy(policy));

        let mut req = Request::builder()
            .method(Method::CONNECT)
            .uri("https://api.github.com:8443")
            .header("host", "api.github.com:8443")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(state);

        let response = http_connect_accept(
            /*policy_decider*/ None, /*environment_id*/ None, req,
        )
        .await
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get("x-proxy-error").unwrap(),
            "blocked-by-mitm-required"
        );
    }

    #[tokio::test]
    async fn brokered_connect_forwards_server_first_opaque_protocol_without_mitm() {
        let server_banner = b"SSH-2.0-server\r\n";
        let target_listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("target listener should bind");
        let target_addr = target_listener
            .local_addr()
            .expect("target listener should expose local addr");
        let target_task = tokio::spawn(async move {
            let (mut stream, _) = target_listener
                .accept()
                .await
                .expect("target listener should accept");
            stream
                .write_all(server_banner)
                .await
                .expect("target should write opaque server bytes");
        });

        let state = Arc::new(network_proxy_state_for_policy({
            let mut network = NetworkProxyConfig {
                credential_broker: true,
                mitm: true,
                ..NetworkProxyConfig::default()
            };
            network.set_allowed_domains(vec!["127.0.0.1".to_string()]);
            network.allow_local_binding = Some(true);
            network
        }));
        let mut env = HashMap::from([
            ("GH_HOST".to_string(), "127.0.0.1".to_string()),
            (
                "GH_ENTERPRISE_TOKEN".to_string(),
                "ghp-enterprise-real".to_string(),
            ),
        ]);
        state.virtualize_child_credentials(&mut env);
        let listener =
            StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("proxy listener should bind");
        let proxy_addr = listener
            .local_addr()
            .expect("proxy listener should expose local addr");
        let lifecycle = crate::connection_lifecycle::ConnectionLifecycle::new();
        let proxy_task = tokio::spawn(run_http_proxy_with_std_listener(
            state,
            listener,
            /*policy_decider*/ None,
            /*environment_id*/ None,
            lifecycle.guard(),
        ));

        let mut stream = tokio::net::TcpStream::connect(proxy_addr)
            .await
            .expect("client should connect to proxy");
        let request = format!(
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n",
            port = target_addr.port()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("client should write CONNECT request");

        let mut buf = [0_u8; 256];
        let bytes_read = timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .expect("proxy should respond before timeout")
            .expect("client should read proxy response");
        let response = String::from_utf8_lossy(&buf[..bytes_read]);
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "unexpected proxy response: {response:?}"
        );

        let mut buf = vec![0_u8; server_banner.len()];
        timeout(Duration::from_secs(2), stream.read_exact(&mut buf))
            .await
            .expect("opaque server bytes should arrive before timeout")
            .expect("client should read opaque server bytes");
        assert_eq!(buf, server_banner);

        drop(stream);
        proxy_task.abort();
        let _ = proxy_task.await;
        target_task.await.expect("target task should finish");
    }

    #[tokio::test]
    async fn http_proxy_blocks_absolute_form_https_for_hooked_host() {
        let target_listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("target listener should bind");
        let target_addr = target_listener
            .local_addr()
            .expect("target listener should expose local addr");
        let target_task = tokio::spawn(async move {
            timeout(Duration::from_secs(1), target_listener.accept())
                .await
                .is_ok()
        });

        let state = Arc::new(network_proxy_state_for_policy({
            let mut network = NetworkProxyConfig {
                allow_local_binding: Some(true),
                mitm: true,
                mitm_hooks: vec![crate::mitm_hook::MitmHookConfig {
                    host: "127.0.0.1".to_string(),
                    matcher: crate::mitm_hook::MitmHookMatchConfig {
                        methods: vec!["GET".to_string()],
                        path_prefixes: vec!["/repos/openai/ALLOWED".to_string()],
                        ..crate::mitm_hook::MitmHookMatchConfig::default()
                    },
                    actions: crate::mitm_hook::MitmHookActionsConfig::default(),
                }],
                ..NetworkProxyConfig::default()
            };
            network.set_allowed_domains(vec!["127.0.0.1".to_string()]);
            network
        }));
        let listener =
            StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("proxy listener should bind");
        let proxy_addr = listener
            .local_addr()
            .expect("proxy listener should expose local addr");
        let lifecycle = crate::connection_lifecycle::ConnectionLifecycle::new();
        let proxy_task = tokio::spawn(run_http_proxy_with_std_listener(
            state.clone(),
            listener,
            /*policy_decider*/ None,
            /*environment_id*/ None,
            lifecycle.guard(),
        ));

        let mut stream = tokio::net::TcpStream::connect(proxy_addr)
            .await
            .expect("client should connect to proxy");
        let request = format!(
            "GET https://127.0.0.1:{port}/repos/openai/UNAUTHORIZED HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
            port = target_addr.port()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("client should write absolute-form HTTPS request");

        let mut buf = [0_u8; 512];
        let bytes_read = timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .expect("proxy should respond before timeout")
            .expect("client should read proxy response");
        let response = String::from_utf8_lossy(&buf[..bytes_read]);
        assert!(
            response.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "unexpected proxy response: {response:?}"
        );
        assert!(response.contains("x-proxy-error: blocked-by-mitm-required\r\n"));
        assert!(
            !target_task.await.expect("target task should finish"),
            "blocked request must not reach upstream"
        );

        let blocked = state.drain_blocked().await.unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].reason, REASON_MITM_REQUIRED);

        drop(stream);
        proxy_task.abort();
        let _ = proxy_task.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_plain_proxy_blocks_unix_socket_when_method_not_allowed() {
        let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig::default()));
        state
            .set_network_mode(NetworkMode::Limited)
            .await
            .expect("network mode should update");

        let mut req = Request::builder()
            .method(Method::POST)
            .uri("http://example.com")
            .header("x-unix-socket", "/tmp/test.sock")
            .body(Body::empty())
            .expect("request should build");
        req.extensions_mut().insert(state);

        let response = http_plain_proxy(
            /*policy_decider*/ None, /*environment_id*/ None, req,
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get("x-proxy-error").unwrap(),
            "blocked-by-method-policy"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_plain_proxy_rejects_unix_socket_when_not_allowlisted() {
        let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig::default()));

        let mut req = Request::builder()
            .method(Method::GET)
            .uri("http://example.com")
            .header("x-unix-socket", "/tmp/test.sock")
            .body(Body::empty())
            .expect("request should build");
        req.extensions_mut().insert(state);

        let response = http_plain_proxy(
            /*policy_decider*/ None, /*environment_id*/ None, req,
        )
        .await
        .unwrap();

        if cfg!(target_os = "macos") {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(
                response.headers().get("x-proxy-error").unwrap(),
                "blocked-by-allowlist"
            );
        } else {
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "current_thread")]
    async fn http_plain_proxy_attempts_allowed_unix_socket_proxy() {
        let state = Arc::new(network_proxy_state_for_policy({
            let mut network = NetworkProxyConfig::default();
            network.set_allow_unix_sockets(vec!["/tmp/test.sock".to_string()]);
            network
        }));

        let mut req = Request::builder()
            .method(Method::GET)
            .uri("http://example.com")
            .header("x-unix-socket", "/tmp/test.sock")
            .body(Body::empty())
            .expect("request should build");
        req.extensions_mut().insert(state);

        let response = http_plain_proxy(
            /*policy_decider*/ None, /*environment_id*/ None, req,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn http_connect_accept_denies_denylisted_host() {
        let policy = {
            let mut policy = NetworkProxyConfig::default();
            policy.set_allowed_domains(vec!["**.openai.com".to_string()]);
            policy.set_denied_domains(vec!["api.openai.com".to_string()]);
            policy
        };
        let state = Arc::new(network_proxy_state_for_policy(policy));

        let mut req = Request::builder()
            .method(Method::CONNECT)
            .uri("https://api.openai.com:443")
            .header("host", "api.openai.com:443")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(state);

        let response = http_connect_accept(
            /*policy_decider*/ None, /*environment_id*/ None, req,
        )
        .await
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get("x-proxy-error").unwrap(),
            "blocked-by-denylist"
        );
    }

    #[tokio::test]
    async fn private_ip_upstream_routing_preserves_destination_policy() {
        for (allowlisted, denied, expected_error) in [
            (true, true, "blocked-by-denylist"),
            (false, false, "blocked-by-allowlist"),
        ] {
            let mut policy = NetworkProxyConfig {
                allow_local_binding: Some(true),
                ..NetworkProxyConfig::default()
            };
            if allowlisted {
                policy.set_allowed_domains(vec!["100.68.58.50".to_string()]);
            }
            if denied {
                policy.set_denied_domains(vec!["100.68.58.50".to_string()]);
            }
            let mut state = network_proxy_state_for_policy(policy);
            state.set_proxy_private_ips_via_upstream(/*enabled*/ true);
            let state = Arc::new(state);
            for method in [Method::GET, Method::CONNECT] {
                let mut req = Request::builder()
                    .method(method.clone())
                    .uri("http://100.68.58.50:80/api/items")
                    .header("host", "100.68.58.50:80")
                    .body(Body::empty())
                    .unwrap();
                req.extensions_mut().insert(state.clone());
                let response = if method == Method::CONNECT {
                    http_connect_accept(
                        /*policy_decider*/ None, /*environment_id*/ None, req,
                    )
                    .await
                    .unwrap_err()
                } else {
                    http_plain_proxy(
                        /*policy_decider*/ None, /*environment_id*/ None, req,
                    )
                    .await
                    .unwrap()
                };
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
                assert_eq!(
                    response.headers().get("x-proxy-error").unwrap(),
                    expected_error
                );
            }
        }
    }

    #[tokio::test]
    async fn http_plain_proxy_rejects_absolute_uri_host_header_mismatch() {
        let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig::default()));
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("http://raw.githubusercontent.com/openai/codex/main/README.md")
            .header(header::HOST, "api.github.com")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(state);

        let response = http_plain_proxy(
            /*policy_decider*/ None, /*environment_id*/ None, req,
        )
        .await;
        assert_eq!(response.unwrap().status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_absolute_form_host_header_allows_matching_default_port() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("http://example.com/")
            .header("host", "example.com")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            validate_absolute_form_host_header(&req, &RequestContext::try_from(&req).unwrap(),),
            Ok(())
        );
    }

    #[test]
    fn validate_absolute_form_host_header_rejects_mismatched_host() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("http://raw.githubusercontent.com/")
            .header("host", "api.github.com")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            validate_absolute_form_host_header(&req, &RequestContext::try_from(&req).unwrap(),),
            Err("Host header does not match request target")
        );
    }

    #[test]
    fn validate_absolute_form_host_header_rejects_missing_non_default_port() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("http://example.com:8080/")
            .header("host", "example.com")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            validate_absolute_form_host_header(&req, &RequestContext::try_from(&req).unwrap(),),
            Err("Host header does not match request target")
        );
    }

    #[test]
    fn remove_hop_by_hop_request_headers_keeps_forwarding_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("x-hop, keep-alive"),
        );
        headers.insert("x-hop", HeaderValue::from_static("1"));
        headers.insert(
            header::PROXY_AUTHORIZATION,
            HeaderValue::from_static("Basic abc"),
        );
        headers.insert(
            &header::X_FORWARDED_FOR,
            HeaderValue::from_static("127.0.0.1"),
        );
        headers.insert(header::HOST, HeaderValue::from_static("example.com"));

        remove_hop_by_hop_request_headers(&mut headers);

        assert_eq!(headers.get(header::CONNECTION), None);
        assert_eq!(headers.get("x-hop"), None);
        assert_eq!(headers.get(header::PROXY_AUTHORIZATION), None);
        assert_eq!(
            headers.get(&header::X_FORWARDED_FOR),
            Some(&HeaderValue::from_static("127.0.0.1"))
        );
        assert_eq!(
            headers.get(header::HOST),
            Some(&HeaderValue::from_static("example.com"))
        );
    }
}
