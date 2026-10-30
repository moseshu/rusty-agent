// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

    use super::*;

    use pretty_assertions::assert_eq;

    fn settings_with_unix_sockets(unix_sockets: &[&str]) -> NetworkProxyConfig {
        let mut settings = NetworkProxyConfig::default();
        if !unix_sockets.is_empty() {
            settings.set_allow_unix_sockets(
                unix_sockets
                    .iter()
                    .map(|path| (*path).to_string())
                    .collect(),
            );
        }
        settings
    }

    #[test]
    fn network_proxy_settings_default_matches_local_use_baseline() {
        assert_eq!(
            NetworkProxyConfig::default(),
            NetworkProxyConfig {
                enabled: false,
                proxy_url: "http://127.0.0.1:3128".to_string(),
                enable_socks5: true,
                socks_url: "http://127.0.0.1:8081".to_string(),
                enable_socks5_udp: true,
                allow_upstream_proxy: true,
                dangerously_allow_non_loopback_proxy: false,
                dangerously_allow_all_unix_sockets: None,
                mode: NetworkMode::Full,
                domains: None,
                unix_sockets: None,
                allow_local_binding: None,
                mitm: false,
                mitm_ca: None,
                credential_broker: false,
                credential_broker_enabled_mitm: false,
                credential_providers: BTreeMap::new(),
                credential_broker_openai_host: None,
                credential_broker_context: crate::CredentialBrokerContext::default(),
                dangerously_allow_plaintext_credential_injection: false,
                mitm_hooks: Vec::new(),
            }
        );
    }

    #[test]
    fn disabling_credential_broker_restores_independent_mitm_setting() {
        for (mitm, add_hook) in [(false, false), (true, false), (false, true)] {
            let mut original = NetworkProxyConfig {
                enabled: true,
                mitm,
                ..Default::default()
            };
            let mut config = original.clone();
            for _ in 0..2 {
                config.set_credential_broker_enabled(/*enabled*/ true);
            }
            if add_hook {
                config.mitm_hooks.push(MitmHookConfig {
                    host: "api.example".to_string(),
                    ..Default::default()
                });
                original.mitm_hooks.clone_from(&config.mitm_hooks);
                original.mitm = true;
            }
            for _ in 0..2 {
                config.set_credential_broker_enabled(/*enabled*/ false);
            }
            assert_eq!(config, original);
            // Upstream also asserts that remote serialization rejects exactly the MITM-enabled
            // configurations; that assertion returns together with `RemoteNetworkProxyConfig`.
        }
    }

    #[test]
    fn disabling_credential_broker_preserves_external_ca_added_while_enabled() {
        let mut config = NetworkProxyConfig {
            enabled: true,
            ..Default::default()
        };
        config.set_credential_broker_enabled(/*enabled*/ true);
        let ca = NetworkMitmCaConfig {
            certificate_file: "/run/proxy/ca.pem".to_string(),
            private_key_file: "/run/proxy/key.pem".to_string(),
        };
        config.mitm_ca = Some(ca.clone());

        config.set_credential_broker_enabled(/*enabled*/ false);

        assert_eq!(
            config,
            NetworkProxyConfig {
                enabled: true,
                mitm: true,
                mitm_ca: Some(ca),
                ..Default::default()
            }
        );
    }

    #[test]
    #[cfg(unix)]
    fn credential_broker_context_accepts_non_unicode_environment() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::process::Command;

        const CHILD_ENV: &str = "CODEX_TEST_NON_UNICODE_BROKER_CONTEXT";
        if std::env::var_os(CHILD_ENV).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "config::tests::credential_broker_context_accepts_non_unicode_environment",
                    "--nocapture",
                ])
                .env(CHILD_ENV, OsString::from_vec(vec![0xff]))
                .env(OsString::from_vec(vec![0xfe]), "unrelated")
                .env("GH_HOST", OsString::from_vec(vec![0xff]))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }

        let mut config = NetworkProxyConfig::default();
        config.set_credential_broker_enabled(/*enabled*/ true);
        config.configure_credential_broker_environment(&HashMap::new());
        assert!(config.credential_broker);
    }

    #[test]
    fn credential_broker_only_accepts_trusted_https_openai_endpoints() {
        let mut config = NetworkProxyConfig::default();

        for (base_url, expected_host) in [
            (
                Some("https://gateway.example.com/v1"),
                Some("gateway.example.com"),
            ),
            (
                Some("https://gateway.example.com./v1"),
                Some("gateway.example.com"),
            ),
            (Some("https://[2001:db8::1]/v1"), Some("2001:db8::1")),
            (Some("http://gateway.example.com/v1"), None),
            (Some("https://user@gateway.example.com/v1"), None),
            (Some("not-a-url"), None),
            (None, None),
        ] {
            config.set_credential_broker_openai_base_url(base_url);
            assert_eq!(
                config.credential_broker_openai_host.as_deref(),
                expected_host
            );
        }
    }

    #[test]
    fn managed_proxy_ports_reject_ephemeral_ports() {
        let mut config = NetworkProxyConfig {
            proxy_url: "http://127.0.0.1:0".to_string(),
            ..Default::default()
        };

        assert_eq!(
            managed_proxy_ports(&config).unwrap_err().to_string(),
            "network.proxy_url must use a fixed non-zero port for managed proxy provisioning"
        );

        config.proxy_url = "http://127.0.0.1:3128".to_string();
        config.socks_url = "socks5h://127.0.0.1:48081".to_string();
        assert_eq!(managed_proxy_ports(&config).unwrap(), vec![3128, 48081]);

        config.socks_url = "socks5h://127.0.0.1:0".to_string();
        assert_eq!(
            managed_proxy_ports(&config).unwrap_err().to_string(),
            "network.socks_url must use a fixed non-zero port for managed proxy provisioning"
        );

        config.enable_socks5 = false;
        assert_eq!(managed_proxy_ports(&config).unwrap(), vec![3128]);
    }

    #[test]
    fn network_proxy_config_uses_struct_defaults_for_missing_fields() {
        let config: NetworkProxyConfig = serde_json::from_str(r#"{ "enabled": true }"#).unwrap();
        let expected = NetworkProxyConfig {
            enabled: true,
            ..NetworkProxyConfig::default()
        };

        assert_eq!(config, expected);
    }

    #[test]
    fn network_proxy_config_supports_external_mitm_ca() {
        let config: NetworkProxyConfig = serde_json::from_str(
            r#"{
                "mitm": true,
                "mitm_ca": {
                    "certificate_file": "/run/proxy/ca.pem",
                    "private_key_file": "/run/proxy/key.pem"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            config,
            NetworkProxyConfig {
                mitm: true,
                mitm_ca: Some(NetworkMitmCaConfig {
                    certificate_file: "/run/proxy/ca.pem".to_string(),
                    private_key_file: "/run/proxy/key.pem".to_string(),
                }),
                ..NetworkProxyConfig::default()
            }
        );
    }

    #[test]
    fn set_allowed_domains_preserves_existing_deny_for_same_pattern() {
        let mut settings = NetworkProxyConfig::default();
        settings.set_denied_domains(vec!["example.com".to_string()]);

        settings.set_allowed_domains(vec!["example.com".to_string()]);

        assert_eq!(settings.allowed_domains(), None);
        assert_eq!(
            settings.denied_domains(),
            Some(vec!["example.com".to_string()])
        );
    }

    #[test]
    fn network_domain_permissions_serialize_to_effective_map_shape() {
        let mut settings = NetworkProxyConfig::default();
        settings.set_denied_domains(vec!["example.com".to_string()]);
        settings.set_allowed_domains(vec!["example.com".to_string()]);
        let config = settings;

        let value = serde_json::to_value(&config).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "enabled": false,
                "proxy_url": "http://127.0.0.1:3128",
                "enable_socks5": true,
                "socks_url": "http://127.0.0.1:8081",
                "enable_socks5_udp": true,
                "allow_upstream_proxy": true,
                "dangerously_allow_non_loopback_proxy": false,
                "mode": "full",
                "domains": {
                    "example.com": "deny",
                },
                "unix_sockets": null,
                "mitm": false,
                "credential_broker": false,
                "dangerously_allow_plaintext_credential_injection": false,
                "mitm_hooks": [],
            })
        );
    }

    #[test]
    fn parse_host_port_defaults_for_empty_string() {
        assert!(parse_host_port("", /*default_port*/ 1234).is_err());
    }

    #[test]
    fn parse_host_port_defaults_for_whitespace() {
        assert!(parse_host_port("   ", /*default_port*/ 5555).is_err());
    }

    #[test]
    fn parse_host_port_parses_host_port_without_scheme() {
        assert_eq!(
            parse_host_port("127.0.0.1:8080", /*default_port*/ 3128).unwrap(),
            SocketAddressParts {
                host: "127.0.0.1".to_string(),
                port: 8080,
            }
        );
    }

    #[test]
    fn parse_host_port_parses_host_port_with_scheme_and_path() {
        assert_eq!(
            parse_host_port(
                "http://example.com:8080/some/path",
                /*default_port*/ 3128
            )
            .unwrap(),
            SocketAddressParts {
                host: "example.com".to_string(),
                port: 8080,
            }
        );
    }

    #[test]
    fn parse_host_port_strips_userinfo() {
        assert_eq!(
            parse_host_port(
                "http://user:pass@host.example:5555",
                /*default_port*/ 3128
            )
            .unwrap(),
            SocketAddressParts {
                host: "host.example".to_string(),
                port: 5555,
            }
        );
    }

    #[test]
    fn parse_host_port_parses_ipv6_with_brackets() {
        assert_eq!(
            parse_host_port("http://[::1]:9999", /*default_port*/ 3128).unwrap(),
            SocketAddressParts {
                host: "::1".to_string(),
                port: 9999,
            }
        );
    }

    #[test]
    fn parse_host_port_does_not_treat_unbracketed_ipv6_as_host_port() {
        assert_eq!(
            parse_host_port("2001:db8::1", /*default_port*/ 3128).unwrap(),
            SocketAddressParts {
                host: "2001:db8::1".to_string(),
                port: 3128,
            }
        );
    }

    #[test]
    fn parse_host_port_falls_back_to_default_port_when_port_is_invalid() {
        assert_eq!(
            parse_host_port("example.com:notaport", /*default_port*/ 3128).unwrap(),
            SocketAddressParts {
                host: "example.com".to_string(),
                port: 3128,
            }
        );
    }

    #[test]
    fn host_and_port_from_network_addr_defaults_for_empty_string() {
        assert_eq!(
            host_and_port_from_network_addr("", /*default_port*/ 1234),
            "<missing>"
        );
    }

    #[test]
    fn host_and_port_from_network_addr_formats_ipv6() {
        assert_eq!(
            host_and_port_from_network_addr("http://[::1]:8080", /*default_port*/ 3128),
            "[::1]:8080"
        );
    }

    #[test]
    fn resolve_addr_maps_localhost_to_loopback() {
        assert_eq!(
            resolve_addr("localhost", /*default_port*/ 3128).unwrap(),
            "127.0.0.1:3128".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn resolve_addr_parses_ip_literals() {
        assert_eq!(
            resolve_addr("1.2.3.4", /*default_port*/ 80).unwrap(),
            "1.2.3.4:80".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn resolve_addr_parses_ipv6_literals() {
        assert_eq!(
            resolve_addr("http://[::1]:8080", /*default_port*/ 3128).unwrap(),
            "[::1]:8080".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn resolve_addr_falls_back_to_loopback_for_hostnames() {
        assert_eq!(
            resolve_addr("http://example.com:5555", /*default_port*/ 3128).unwrap(),
            "127.0.0.1:5555".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn clamp_bind_addrs_allows_non_loopback_when_enabled() {
        let cfg = NetworkProxyConfig {
            dangerously_allow_non_loopback_proxy: true,
            ..Default::default()
        };
        let http_addr = "0.0.0.0:3128".parse::<SocketAddr>().unwrap();
        let socks_addr = "0.0.0.0:8081".parse::<SocketAddr>().unwrap();

        let (http_addr, socks_addr) = clamp_bind_addrs(http_addr, socks_addr, &cfg);

        assert_eq!(http_addr, "0.0.0.0:3128".parse::<SocketAddr>().unwrap());
        assert_eq!(socks_addr, "0.0.0.0:8081".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn clamp_bind_addrs_forces_loopback_when_unix_sockets_enabled() {
        let cfg = {
            let mut settings = settings_with_unix_sockets(&["/tmp/docker.sock"]);
            settings.dangerously_allow_non_loopback_proxy = true;
            settings
        };
        let http_addr = "0.0.0.0:3128".parse::<SocketAddr>().unwrap();
        let socks_addr = "0.0.0.0:8081".parse::<SocketAddr>().unwrap();

        let (http_addr, socks_addr) = clamp_bind_addrs(http_addr, socks_addr, &cfg);

        assert_eq!(http_addr, "127.0.0.1:3128".parse::<SocketAddr>().unwrap());
        assert_eq!(socks_addr, "127.0.0.1:8081".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn clamp_bind_addrs_forces_loopback_when_all_unix_sockets_enabled() {
        let cfg = NetworkProxyConfig {
            dangerously_allow_non_loopback_proxy: true,
            dangerously_allow_all_unix_sockets: Some(true),
            ..Default::default()
        };
        let http_addr = "0.0.0.0:3128".parse::<SocketAddr>().unwrap();
        let socks_addr = "0.0.0.0:8081".parse::<SocketAddr>().unwrap();

        let (http_addr, socks_addr) = clamp_bind_addrs(http_addr, socks_addr, &cfg);

        assert_eq!(http_addr, "127.0.0.1:3128".parse::<SocketAddr>().unwrap());
        assert_eq!(socks_addr, "127.0.0.1:8081".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn resolve_runtime_validates_allow_unix_sockets_for_executor_os() {
        for (path, unix_absolute, windows_absolute) in [
            ("relative.sock", false, false),
            ("~/example.sock", false, false),
            ("/tmp/example.sock", true, true),
            (r"C:\example.sock", false, true),
            ("C:/example.sock", false, true),
            (r"\\server\share\example.sock", false, true),
            (r"\\?\C:\example.sock", false, true),
            (r"\\.\pipe\example", false, true),
            (r"C:example.sock", false, false),
            (r"\example.sock", false, false),
            (r"\\server", false, false),
            ("/tmp/\0example.sock", false, false),
            ("C:\\example\0.sock", false, false),
        ] {
            let cfg = settings_with_unix_sockets(&[path]);
            for (executor_os, accepted) in [
                (Platform::Linux, unix_absolute),
                (Platform::Macos, unix_absolute),
                (Platform::Windows, windows_absolute),
                (Platform::Unknown, unix_absolute || windows_absolute),
            ] {
                assert_eq!(
                    resolve_runtime(&cfg, executor_os).is_ok(),
                    accepted,
                    "{executor_os:?}: {path:?}"
                );
            }
            // Each platform's CI compares the portable implementation to native
            // Core acceptance, with the deliberate shared NUL rejection.
            assert_eq!(
                resolve_runtime(&cfg, Platform::native()).is_ok(),
                (Path::new(path).is_absolute() || path.starts_with('/')) && !path.contains('\0'),
                "native parity: {path:?}"
            );
        }
    }

    #[test]
    fn resolve_runtime_accepts_unix_style_absolute_allow_unix_sockets_entries() {
        let mut cfg = settings_with_unix_sockets(&[
            "/private/tmp/example.sock",
            "/tmp/../example.sock",
            r"/tmp/name\part.sock",
        ]);
        for path in ["relative.sock", "~/example.sock", r"C:\example.sock", "\0"] {
            cfg.unix_sockets
                .as_mut()
                .unwrap()
                .entries
                .insert(path.to_string(), NetworkUnixSocketPermission::Deny);
        }

        for executor_os in [
            Platform::Linux,
            Platform::Macos,
            Platform::Windows,
            Platform::Unknown,
        ] {
            assert!(
                resolve_runtime(&cfg, executor_os).is_ok(),
                "unix-style absolute allow_unix_sockets entry should be accepted"
            );
        }
    }
