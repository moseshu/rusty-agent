// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ra_config::*;

#[test]
fn ema_rejects_alternate_credentials_and_executor_custody() {
    for extra in [
        "bearer_token_env_var='TOKEN'",
        "http_headers.Authorization='secret'",
        "http_headers.Accept='application/json'",
        "env_http_headers.X-Key='TOKEN'",
        "http_headers_helper='get-headers'",
        "oauth={client_id='client', client_secret='secret'}",
        "environment_id='remote'",
    ] {
        let server: McpServerConfig = toml::from_str(&format!(
            "url='https://resource.example'\nauth='ema_auth'\n{extra}"
        ))
        .unwrap();
        assert!(server.validate_ema_auth_transport().is_err(), "{extra}");
    }
    let server: McpServerConfig =
        toml::from_str("url='https://resource.example'\nauth='ema_auth'\nhttp_headers={}").unwrap();
    assert_eq!(server.validate_ema_auth_transport(), Ok(()));
}

#[test]
fn explicit_registration_captures_the_resource_and_clears_on_failure() {
    let mut server: McpServerConfig = toml::from_str(
        r#"
url = "https://resource.example/mcp"
auth = "ema_auth"
scopes = ["tools"]
oauth_resource = "https://resource.example"
[oauth]
client_id = "resource-client"
authorization_server_issuer = "https://as.example"
"#,
    )
    .expect("EMA config");
    let idp = McpServerIdpOAuthConfig {
        issuer: "https://idp.example".into(),
        client_id: "enterprise".into(),
    };
    server
        .resolve_ema_registration(&idp)
        .expect("trusted registration");
    let registration = server.ema_registration().expect("registration");
    assert_eq!(registration.idp(), &idp);
    assert_eq!(registration.server_url(), "https://resource.example/mcp");
    assert_eq!(registration.resource(), Some("https://resource.example"));
    assert_eq!(registration.client_id(), Some("resource-client"));
    assert_eq!(
        registration.authorization_server_issuer(),
        Some("https://as.example")
    );
    assert_eq!(registration.scopes(), &["tools"]);
    assert_eq!(server.oauth_idp(), Some(&idp));
    let serialized = serde_json::to_value(&server).expect("serialize");
    assert!(serialized["oauth"].get("ema_registration").is_none());
    let round_trip: McpServerConfig = serde_json::from_value(serialized).expect("deserialize");
    assert!(round_trip.ema_registration().is_none());
    server.environment_id = "remote".into();
    assert_eq!(
        server.resolve_ema_registration(&idp),
        Err("ema_auth requires a host-owned MCP connection")
    );
    assert!(server.ema_registration().is_none());
}

#[test]
fn declaration_cannot_deserialize_host_registration() {
    let server: McpServerConfig = serde_json::from_value(serde_json::json!({
        "url": "https://resource.example/mcp", "auth": "ema_auth",
        "oauth": {"ema_registration": {"idp": {"issuer": "https://attacker.example", "client_id": "other"}}}
    })).expect("unknown runtime-only authorization is ignored");
    assert!(server.ema_registration().is_none());
}

#[test]
fn openai_account_auth_is_not_an_available_product_auth_flow() {
    assert!(
        toml::from_str::<McpServerConfig>("url = 'https://example.com/mcp'\nauth = 'chatgpt'")
            .is_err()
    );
    for auth in ["oauth", "ema_auth"] {
        assert!(
            toml::from_str::<McpServerConfig>(&format!(
                "url = 'https://example.com/mcp'\nauth = '{auth}'"
            ))
            .is_ok()
        );
    }
}

#[test]
fn timeout_compatibility_prefers_seconds_and_rejects_invalid_values() {
    let server: McpServerConfig = toml::from_str("command = 'server'\nstartup_timeout_sec = 1.25\nstartup_timeout_ms = 2\ntool_timeout_sec = 0.5").expect("timeouts");
    assert_eq!(
        server.startup_timeout_sec,
        Some(std::time::Duration::from_millis(1250))
    );
    assert_eq!(
        server.tool_timeout_sec,
        Some(std::time::Duration::from_millis(500))
    );
    let server: McpServerConfig =
        toml::from_str("command = 'server'\nstartup_timeout_ms = 2").expect("legacy timeout");
    assert_eq!(
        server.startup_timeout_sec,
        Some(std::time::Duration::from_millis(2))
    );
    for (key, value) in [
        ("startup_timeout_sec", "-1.0"),
        ("startup_timeout_sec", "nan"),
        ("startup_timeout_sec", "inf"),
        ("tool_timeout_sec", "-1.0"),
        ("tool_timeout_sec", "inf"),
    ] {
        assert!(
            toml::from_str::<McpServerConfig>(&format!("command = 'server'\n{key} = {value}"))
                .is_err()
        );
    }
}

#[test]
fn credential_names_preserve_local_compatibility_without_remote_collisions() {
    let mut server: McpServerConfig = toml::from_str("command = 'server'").expect("server");
    assert_eq!(server.oauth_credential_name("plain"), "plain");
    assert_eq!(
        server.oauth_credential_name("executor:plain"),
        "local:executor:plain"
    );
    assert_eq!(
        server.oauth_credential_name("local:plain"),
        "local:local:plain"
    );
    assert_eq!(
        server.oauth_credential_name("ema-idp:plain"),
        "local:ema-idp:plain"
    );
    server.environment_id = "remote:one".into();
    let first = server.oauth_credential_name("server:two").into_owned();
    server.environment_id = "remote".into();
    let second = server.oauth_credential_name("one:server:two").into_owned();
    assert_ne!(first, second);
    assert_eq!(first, "executor:cmVtb3RlOm9uZQ:c2VydmVyOnR3bw");
}
