// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Startup parsing ports Codex's protocol tests and the app-server CLI default.

use clap::Parser;
use ra_core::session::SessionId;
use ra_protocol::server::{
    InternalSessionSource, SessionSource, SessionSourceArgs, SubAgentSource,
};
use serde_json::json;

#[derive(Parser)]
struct HostArgs {
    #[command(flatten)]
    source: SessionSourceArgs,
}

#[test]
fn session_source_from_startup_arg_maps_known_values() {
    for (value, expected) in [
        ("cli", SessionSource::Cli),
        ("vscode", SessionSource::VSCode),
        ("exec", SessionSource::Exec),
        ("unknown", SessionSource::Unknown),
        ("mcp", SessionSource::Mcp),
        ("appserver", SessionSource::Mcp),
        ("app-server", SessionSource::Mcp),
        ("app_server", SessionSource::Mcp),
        ("  APP-SERVER  ", SessionSource::Mcp),
        (" VSCode ", SessionSource::VSCode),
    ] {
        assert_eq!(SessionSource::from_startup_arg(value).unwrap(), expected);
    }
}

#[test]
fn session_source_from_startup_arg_normalizes_custom_values() {
    for value in ["atlas", " Atlas ", "ATLAS"] {
        assert_eq!(
            SessionSource::from_startup_arg(value).unwrap(),
            SessionSource::Custom("atlas".into())
        );
    }
    assert_eq!(
        SessionSource::from_startup_arg(" Rusty-Agent ").unwrap(),
        SessionSource::Custom("rusty-agent".into())
    );
    assert_eq!(
        SessionSource::from_startup_arg(" Custom:Host ").unwrap(),
        SessionSource::Custom("custom:host".into())
    );
}

#[test]
fn startup_args_default_to_vscode_and_use_upstream_parser() {
    assert_eq!(SessionSource::default(), SessionSource::VSCode);
    assert_eq!(
        SessionSourceArgs::default().session_source(),
        &SessionSource::VSCode
    );
    assert_eq!(
        HostArgs::try_parse_from(["host"])
            .unwrap()
            .source
            .session_source(),
        &SessionSource::VSCode
    );
    for value in [" CLI ", "app_server", " Rusty-Agent ", "unknown"] {
        let args = HostArgs::try_parse_from(["host", "--session-source", value]).unwrap();
        assert_eq!(
            args.source.session_source(),
            &SessionSource::from_startup_arg(value).unwrap()
        );
    }
    for value in ["", " ", "\t\n"] {
        assert_eq!(
            SessionSource::from_startup_arg(value),
            Err("session source must not be empty")
        );
        let error = HostArgs::try_parse_from(["host", "--session-source", value])
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("session source must not be empty")
        );
    }
}

#[test]
fn source_metadata_preserves_core_names_and_opaque_parent_ids() {
    let cases = [
        (SessionSource::VSCode, json!("vscode")),
        (SessionSource::Mcp, json!("mcp")),
        (
            SessionSource::Custom("Host".into()),
            json!({"custom":"Host"}),
        ),
        (
            SessionSource::Internal(InternalSessionSource::Guardian),
            json!({"internal":"guardian"}),
        ),
        (
            SessionSource::SubAgent(SubAgentSource::Review),
            json!({"subagent":"review"}),
        ),
    ];
    for (source, value) in cases {
        assert_eq!(serde_json::to_value(&source).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<SessionSource>(value).unwrap(),
            source
        );
    }
    let value = json!({"subagent":{"thread_spawn":{
        "parent_thread_id":"opaque-parent", "depth":1, "agent_type":"reviewer"
    }}});
    let source: SessionSource = serde_json::from_value(value).unwrap();
    assert!(
        matches!(&source, SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id, agent_role, agent_path: None, agent_nickname: None, depth: 1
    }) if parent_thread_id == &SessionId::new("opaque-parent") && agent_role.as_deref() == Some("reviewer"))
    );
    assert_eq!(
        serde_json::to_value(source).unwrap()["subagent"]["thread_spawn"]["agent_role"],
        "reviewer"
    );
    assert_eq!(
        serde_json::from_value::<SessionSource>(json!("future-source")).unwrap(),
        SessionSource::Unknown
    );
}
