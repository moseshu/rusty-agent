// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_executor::EnvironmentProvider;

use ra_http::HttpClientFactory;
use ra_http::OutboundProxyPolicy;
use std::{collections::HashMap, path::PathBuf, time::Duration};

use ra_executor::test_api::environment_toml::*;
use tempfile::tempdir;
#[test]
fn auth_bearer_token_requires_websocket_and_preserves_timeouts() {
    let config: EnvironmentsToml = toml::from_str(
        r#"
            [[environments]]
            id = "remote"
            url = "wss://executor.example"
            auth_bearer_token = "private-token"
            initialize_timeout_sec = 17
        "#,
    )
    .unwrap();
    assert!(!format!("{config:?}").contains("private-token"));
    let provider = TomlEnvironmentProvider::new(config).unwrap();
    let ExecServerTransportParams::WebSocketUrl {
        http_headers,
        initialize_timeout,
        ..
    } = &provider.environments[0].transport
    else {
        panic!("expected websocket")
    };
    assert_eq!(
        http_headers[http::header::AUTHORIZATION],
        "Bearer private-token"
    );
    assert!(http_headers[http::header::AUTHORIZATION].is_sensitive());
    assert_eq!(*initialize_timeout, Duration::from_secs(17));
    let error = TomlEnvironmentProvider::new(EnvironmentsToml {
        environments: vec![EnvironmentToml {
            id: "stdio".to_string(),
            program: Some("executor".to_string()),
            auth_bearer_token: Some("private-token".into()),
            ..Default::default()
        }],
        ..Default::default()
    })
    .unwrap_err();
    assert!(error.to_string().contains("auth_bearer_token requires url"));
}
#[tokio::test]
async fn toml_provider_includes_local_and_adds_configured_environments() {
    let provider = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: Some("ssh-dev".to_string()),
        include_local: None,
        environments: vec![
            EnvironmentToml {
                id: "devbox".to_string(),
                url: Some(" ws://127.0.0.1:8765 ".to_string()),
                ..Default::default()
            },
            EnvironmentToml {
                id: "ssh-dev".to_string(),
                program: Some(" ssh ".to_string()),
                args: Some(vec![
                    "dev".to_string(),
                    "codex exec-server --listen stdio".to_string(),
                ]),
                env: Some(HashMap::from([(
                    "CODEX_LOG".to_string(),
                    "debug".to_string(),
                )])),
                ..Default::default()
            },
        ],
    })
    .expect("provider");
    let snapshot = provider.snapshot().await.expect("environments");
    let environments = snapshot.entries_for_test().to_vec();
    let default = snapshot.default;
    let include_local = snapshot.include_local;
    let environment_ids: Vec<_> = environments.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(environment_ids, vec!["devbox", "ssh-dev"]);
    let environments: HashMap<_, _> = environments
        .into_iter()
        .map(|entry| (entry.id, entry.transport))
        .collect();
    assert!(include_local);
    assert!(!environments.contains_key(LOCAL_ENVIRONMENT_ID));
    assert!(matches!(
        &environments["devbox"],
        ExecServerTransportParams::WebSocketUrl { .. }
    ));
    assert!(matches!(
        &environments["ssh-dev"],
        ExecServerTransportParams::StdioCommand { .. }
    ));
    assert_eq!(
        default,
        EnvironmentDefault::EnvironmentId("ssh-dev".to_string())
    );
}
#[tokio::test]
async fn toml_provider_default_omitted_selects_local() {
    let provider = TomlEnvironmentProvider::new(EnvironmentsToml::default()).expect("provider");
    let snapshot = provider.snapshot().await.expect("environments");
    assert!(snapshot.include_local);
    assert_eq!(
        snapshot.default,
        EnvironmentDefault::EnvironmentId(LOCAL_ENVIRONMENT_ID.to_string())
    );
}
#[tokio::test]
async fn toml_provider_default_none_disables_default() {
    let provider = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: Some("none".to_string()),
        include_local: None,
        environments: Vec::new(),
    })
    .expect("provider");
    let snapshot = provider.snapshot().await.expect("environments");
    assert!(snapshot.include_local);
    assert_eq!(snapshot.default, EnvironmentDefault::Disabled);
}
#[tokio::test]
async fn toml_provider_can_disable_local_environment() {
    let provider = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: Some("ssh-dev".to_string()),
        include_local: Some(false),
        environments: vec![EnvironmentToml {
            id: "ssh-dev".to_string(),
            program: Some("ssh".to_string()),
            ..Default::default()
        }],
    })
    .expect("provider");
    let snapshot = provider.snapshot().await.expect("environments");
    assert!(!snapshot.include_local);
    assert_eq!(
        snapshot.default,
        EnvironmentDefault::EnvironmentId("ssh-dev".to_string())
    );
}
#[tokio::test]
async fn toml_provider_without_local_and_default_omitted_disables_default() {
    let provider = TomlEnvironmentProvider::new(EnvironmentsToml {
        include_local: Some(false),
        ..Default::default()
    })
    .expect("provider");
    let snapshot = provider.snapshot().await.expect("environments");
    assert!(!snapshot.include_local);
    assert_eq!(snapshot.default, EnvironmentDefault::Disabled);
}
#[test]
fn toml_provider_rejects_local_default_when_local_is_disabled() {
    let err = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: Some(LOCAL_ENVIRONMENT_ID.to_string()),
        include_local: Some(false),
        environments: Vec::new(),
    })
    .expect_err("local default without local environment should fail");
    assert_eq!(
        err.to_string(),
        "exec-server protocol error: default environment `local` is not configured"
    );
}
#[test]
fn toml_provider_rejects_invalid_environments() {
    let cases = [
        (
            EnvironmentToml {
                id: "local".to_string(),
                url: Some("ws://127.0.0.1:8765".to_string()),
                ..Default::default()
            },
            "environment id `local` is reserved",
        ),
        (
            EnvironmentToml {
                id: " devbox ".to_string(),
                url: Some("ws://127.0.0.1:8765".to_string()),
                ..Default::default()
            },
            "environment id ` devbox ` must not contain surrounding whitespace",
        ),
        (
            EnvironmentToml {
                id: "dev box".to_string(),
                url: Some("ws://127.0.0.1:8765".to_string()),
                ..Default::default()
            },
            "environment id `dev box` must contain only ASCII letters, numbers, '-' or '_'",
        ),
        (
            EnvironmentToml {
                id: "devbox".to_string(),
                url: Some("http://127.0.0.1:8765".to_string()),
                ..Default::default()
            },
            "environment url `http://127.0.0.1:8765` must use ws:// or wss://",
        ),
        (
            EnvironmentToml {
                id: "devbox".to_string(),
                url: Some("ws://127.0.0.1:8765".to_string()),
                program: Some("codex".to_string()),
                ..Default::default()
            },
            "environment `devbox` must set exactly one of url or program",
        ),
        (
            EnvironmentToml {
                id: "devbox".to_string(),
                program: Some(" ".to_string()),
                ..Default::default()
            },
            "environment `devbox` program cannot be empty",
        ),
        (
            EnvironmentToml {
                id: "devbox".to_string(),
                args: Some(Vec::new()),
                ..Default::default()
            },
            "environment `devbox` args, env, and cwd require program",
        ),
        (
            EnvironmentToml {
                id: "ssh-dev".to_string(),
                program: Some("ssh".to_string()),
                connect_timeout_sec: Some(Duration::from_secs(1)),
                ..Default::default()
            },
            "environment `ssh-dev` connect_timeout_sec requires url",
        ),
    ];
    for (item, expected) in cases {
        let err = TomlEnvironmentProvider::new(EnvironmentsToml {
            default: None,
            include_local: None,
            environments: vec![item],
        })
        .expect_err("invalid item should fail");
        assert_eq!(
            err.to_string(),
            format!("exec-server protocol error: {expected}")
        );
    }
}
#[test]
fn toml_provider_resolves_relative_stdio_cwd_from_config_dir() {
    let config_dir = tempdir().expect("tempdir");
    let provider = TomlEnvironmentProvider::new_with_config_dir(
        EnvironmentsToml {
            default: None,
            include_local: None,
            environments: vec![EnvironmentToml {
                id: "ssh-dev".to_string(),
                program: Some("ssh".to_string()),
                cwd: Some(PathBuf::from("workspace")),
                ..Default::default()
            }],
        },
        Some(config_dir.path()),
    )
    .expect("provider");
    let ExecServerTransportParams::StdioCommand {
        command,
        initialize_timeout,
    } = &provider.environments[0].transport
    else {
        panic!("expected stdio transport");
    };
    assert_eq!(
        command,
        &StdioExecServerCommand {
            program: "ssh".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: Some(config_dir.path().join("workspace")),
        }
    );
    assert_eq!(
        *initialize_timeout,
        DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT
    );
}
#[test]
fn toml_provider_parses_configured_transport_timeouts() {
    let provider = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: None,
        include_local: None,
        environments: vec![
            EnvironmentToml {
                id: "devbox".to_string(),
                url: Some("ws://127.0.0.1:8765".to_string()),
                connect_timeout_sec: Some(Duration::from_secs(12)),
                initialize_timeout_sec: Some(Duration::from_secs(34)),
                ..Default::default()
            },
            EnvironmentToml {
                id: "ssh-dev".to_string(),
                program: Some("ssh".to_string()),
                initialize_timeout_sec: Some(Duration::from_secs(56)),
                ..Default::default()
            },
        ],
    })
    .expect("provider");
    let ExecServerTransportParams::WebSocketUrl {
        websocket_url,
        connect_timeout,
        initialize_timeout,
        ..
    } = &provider.environments[0].transport
    else {
        panic!("expected websocket transport");
    };
    assert_eq!(websocket_url, "ws://127.0.0.1:8765");
    assert_eq!(*connect_timeout, Duration::from_secs(12));
    assert_eq!(*initialize_timeout, Duration::from_secs(34));
    let ExecServerTransportParams::StdioCommand {
        command,
        initialize_timeout,
    } = &provider.environments[1].transport
    else {
        panic!("expected stdio transport");
    };
    assert_eq!(
        command,
        &StdioExecServerCommand {
            program: "ssh".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
        }
    );
    assert_eq!(*initialize_timeout, Duration::from_secs(56));
}
#[test]
fn toml_provider_rejects_relative_stdio_cwd_without_config_dir() {
    let err = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: None,
        include_local: None,
        environments: vec![EnvironmentToml {
            id: "ssh-dev".to_string(),
            program: Some("ssh".to_string()),
            cwd: Some(PathBuf::from("workspace")),
            ..Default::default()
        }],
    })
    .expect_err("relative cwd without config dir should fail");
    assert_eq!(
        err.to_string(),
        "exec-server protocol error: environment `ssh-dev` cwd must be absolute"
    );
}
#[test]
fn toml_provider_rejects_duplicate_ids() {
    let err = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: None,
        include_local: None,
        environments: vec![
            EnvironmentToml {
                id: "devbox".to_string(),
                url: Some("ws://127.0.0.1:8765".to_string()),
                ..Default::default()
            },
            EnvironmentToml {
                id: "devbox".to_string(),
                program: Some("codex".to_string()),
                ..Default::default()
            },
        ],
    })
    .expect_err("duplicate id should fail");
    assert_eq!(
        err.to_string(),
        "exec-server protocol error: environment id `devbox` is duplicated"
    );
}
#[test]
fn toml_provider_rejects_overlong_id() {
    let id = "a".repeat(MAX_ENVIRONMENT_ID_LEN + 1);
    let err = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: None,
        include_local: None,
        environments: vec![EnvironmentToml {
            id: id.clone(),
            url: Some("ws://127.0.0.1:8765".to_string()),
            ..Default::default()
        }],
    })
    .expect_err("overlong id should fail");
    assert_eq!(
        err.to_string(),
        format!(
            "exec-server protocol error: environment id `{id}` cannot be longer than {MAX_ENVIRONMENT_ID_LEN} characters"
        )
    );
}
#[test]
fn toml_provider_rejects_unknown_default() {
    let err = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: Some("missing".to_string()),
        include_local: None,
        environments: Vec::new(),
    })
    .expect_err("unknown default should fail");
    assert_eq!(
        err.to_string(),
        "exec-server protocol error: default environment `missing` is not configured"
    );
}
#[doc = " Requirements from a TOML entry reach that environment without affecting other entries."]
#[tokio::test]
async fn required_skills_from_toml_reach_their_environment() {
    let codex_home = tempdir().expect("tempdir");
    std::fs::write(
        codex_home.path().join(ENVIRONMENTS_TOML_FILE),
        r#"
include_local = false
[[environments]]
id = "training"
url = "ws://127.0.0.1:4512"
[environments.skills]
required = ["computer-use"]

[[environments]]
id = "other"
url = "ws://127.0.0.1:4513"
"#,
    )
    .expect("write environments.toml");
    let provider =
        environment_provider_from_codex_home(codex_home.path()).expect("environment provider");
    let manager = ra_executor::test_api::environment::from_snapshot(
        provider.snapshot().await.expect("environment snapshot"),
        None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
    .expect("environment manager");
    let requirements = ["training", "other"].map(|id| {
        manager
            .get_environment(id)
            .expect("registered environment")
            .required_skills()
            .to_vec()
    });
    assert_eq!(requirements, [vec!["computer-use".to_string()], Vec::new()]);
}
#[test]
fn load_environments_toml_reads_root_environment_list() {
    let codex_home = tempdir().expect("tempdir");
    let path = codex_home.path().join(ENVIRONMENTS_TOML_FILE);
    std::fs::write(
        &path,
        r#"
default = "ssh-dev"
include_local = false

[[environments]]
id = "devbox"
url = "ws://127.0.0.1:4512"
connect_timeout_sec = 12.0
initialize_timeout_sec = 34.0

[[environments]]
id = "ssh-dev"
program = "ssh"
args = ["dev", "codex exec-server --listen stdio"]
cwd = "/tmp"
[environments.env]
CODEX_LOG = "debug"
"#,
    )
    .expect("write environments.toml");
    let environments = load_environments_toml(&path)
        .expect("environments.toml")
        .expect("environments.toml should exist");
    assert_eq!(environments.default.as_deref(), Some("ssh-dev"));
    assert_eq!(environments.include_local, Some(false));
    assert_eq!(environments.environments.len(), 2);
    assert_eq!(
        environments.environments[0],
        EnvironmentToml {
            id: "devbox".to_string(),
            url: Some("ws://127.0.0.1:4512".to_string()),
            connect_timeout_sec: Some(Duration::from_secs(12)),
            initialize_timeout_sec: Some(Duration::from_secs(34)),
            ..Default::default()
        }
    );
    assert_eq!(
        environments.environments[1],
        EnvironmentToml {
            id: "ssh-dev".to_string(),
            program: Some("ssh".to_string()),
            args: Some(vec![
                "dev".to_string(),
                "codex exec-server --listen stdio".to_string(),
            ]),
            env: Some(HashMap::from([(
                "CODEX_LOG".to_string(),
                "debug".to_string(),
            )])),
            cwd: Some(PathBuf::from("/tmp")),
            ..Default::default()
        }
    );
}
#[test]
fn load_environments_toml_rejects_unknown_fields() {
    let codex_home = tempdir().expect("tempdir");
    let cases = [
        ("unknown = true\n", "unknown field `unknown`"),
        (
            r#"
[[environments]]
id = "devbox"
url = "ws://127.0.0.1:4512"
unknown = true
"#,
            "unknown field `unknown`",
        ),
    ];
    for (index, (contents, expected)) in cases.into_iter().enumerate() {
        let path = codex_home.path().join(format!("environments-{index}.toml"));
        std::fs::write(&path, contents).expect("write environments.toml");
        let err = load_environments_toml(&path).expect_err("unknown field should fail");
        assert!(
            err.to_string().contains(expected),
            "expected `{err}` to contain `{expected}`"
        );
    }
}
#[test]
fn toml_provider_rejects_malformed_websocket_url() {
    let err = TomlEnvironmentProvider::new(EnvironmentsToml {
        default: None,
        include_local: None,
        environments: vec![EnvironmentToml {
            id: "devbox".to_string(),
            url: Some("ws://".to_string()),
            ..Default::default()
        }],
    })
    .expect_err("malformed websocket url should fail");
    assert!(
        err.to_string()
            .contains("environment url `ws://` is invalid"),
        "expected malformed URL error, got `{err}`"
    );
}
#[tokio::test]
async fn environment_provider_from_codex_home_uses_present_environments_file() {
    let codex_home = tempdir().expect("tempdir");
    std::fs::write(
        codex_home.path().join(ENVIRONMENTS_TOML_FILE),
        r#"
default = "none"
include_local = false
"#,
    )
    .expect("write environments.toml");
    let provider =
        environment_provider_from_codex_home(codex_home.path()).expect("environment provider");
    let snapshot = provider.snapshot().await.expect("environments");
    let environment_ids: Vec<_> = snapshot
        .entries_for_test()
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    assert!(!snapshot.include_local);
    assert!(!environment_ids.contains(&LOCAL_ENVIRONMENT_ID.to_string()));
    assert_eq!(snapshot.default, EnvironmentDefault::Disabled);
}
#[tokio::test]
async fn environment_provider_from_codex_home_falls_back_when_file_is_missing() {
    let codex_home = tempdir().expect("tempdir");
    let provider =
        environment_provider_from_codex_home(codex_home.path()).expect("environment provider");
    let snapshot = provider.snapshot().await.expect("environments");
    let environment_ids: Vec<_> = snapshot
        .entries_for_test()
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    assert!(snapshot.include_local);
    assert!(!environment_ids.contains(&LOCAL_ENVIRONMENT_ID.to_string()));
    assert_eq!(
        snapshot.default,
        EnvironmentDefault::EnvironmentId(LOCAL_ENVIRONMENT_ID.to_string())
    );
}
