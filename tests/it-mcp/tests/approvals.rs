//! Local MCP approval normalization and legacy discovery follow Python's server contract.

#[path = "../support/mod.rs"]
mod support;

use ra_core::tool::{Tool, ToolApprovalPolicy, ToolGuardrailId};
use ra_mcp::{
    approval::RequireApprovalSetting,
    client::McpClientOptions,
    filter::ToolFilter,
    in_process::InProcessToolServer,
    tool::{McpDiscoveryOptions, McpToolNaming, discover_tools},
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn normalized_policies_use_original_names_and_leave_unlisted_tools_unapproved() {
    let cases = [
        (json!(null), [false, false, false, false]),
        (json!(false), [false, false, false, false]),
        (json!(true), [true, true, true, true]),
        (json!("never"), [false, false, false, false]),
        (json!("always"), [true, true, true, true]),
        (json!({}), [false, false, false, false]),
        (
            json!({"guarded":"always", "safe":"never"}),
            [true, false, false, false],
        ),
        (
            json!({"guarded":true, "safe":false}),
            [true, false, false, false],
        ),
        (
            json!({"always":"always", "never":"never"}),
            [false, false, true, false],
        ),
        (
            json!({"always":{"tool_names":["guarded", "guarded"]}, "never":{"tool_names":["safe"]}}),
            [true, false, false, false],
        ),
        (
            json!({"always":{"tool_names":["guarded"], "ignored":true}, "never":{}}),
            [true, false, false, false],
        ),
        (
            json!({"never":{"tool_names":["safe"]}}),
            [false, false, false, false],
        ),
    ];
    for (value, required) in cases {
        let policy: RequireApprovalSetting = serde_json::from_value(value.clone()).unwrap();
        let mut fixture = support::Fixture::new("docs");
        fixture.tools_override = Some(
            ["guarded", "safe", "always", "never"]
                .map(support::declaration)
                .into(),
        );
        let mut server = InProcessToolServer::connect(
            "docs",
            fixture,
            McpClientOptions::default().with_require_approval(policy),
        )
        .await
        .unwrap();
        for naming in [
            McpToolNaming::Original,
            McpToolNaming::ServerPrefixed,
            McpToolNaming::Namespaced,
        ] {
            let tools = discover_tools(
                &[(server.client(), ToolFilter::default())],
                &McpDiscoveryOptions::new().with_naming(naming),
            )
            .await
            .unwrap();
            for tool in &tools {
                let index = ["guarded", "safe", "always", "never"]
                    .iter()
                    .position(|name| *name == tool.info().tool().name)
                    .unwrap();
                let required = required[index];
                assert_eq!(
                    tool.options().approval(),
                    if required {
                        ToolApprovalPolicy::Always
                    } else {
                        ToolApprovalPolicy::Never
                    },
                    "{value}"
                );
            }
        }
        server.cleanup().await.unwrap();
    }
}

#[test]
fn malformed_policies_fail_instead_of_silently_disabling_approval() {
    for (value, diagnostic) in [
        (json!("alwyas"), "expected 'always' or 'never'"),
        (json!({"delete":"alwyas"}), "delete"),
        (
            json!({"always":{"tool_names":["delete"]},"never":{"tool_names":["delete"]}}),
            "both always and never",
        ),
        (
            json!({"always":{"tool_names":["delete"]}, "other":false}),
            "expected only",
        ),
        (json!({"always":{"tool_names":"delete"}}), "list of strings"),
        (json!({"always":{"tool_names":[1]}}), "tool_names[0]"),
        (
            json!({"always":{"tool_names":[]}, "never":true}),
            "object with tool_names",
        ),
        (json!({"always":{}}), "require_approval"),
        (json!({"delete":null}), "delete"),
        (json!(["delete"]), "require_approval"),
        (json!(1), "require_approval"),
    ] {
        let error = RequireApprovalSetting::try_from(value.clone()).unwrap_err();
        assert!(error.to_string().contains(diagnostic), "{value}: {error}");
        assert!(serde_json::from_value::<RequireApprovalSetting>(value).is_err());
    }
}

#[tokio::test]
async fn callable_without_discovery_agent_requires_approval_without_running_the_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut server = InProcessToolServer::connect(
        "docs",
        support::Fixture::new("docs"),
        McpClientOptions::default().with_require_approval(RequireApprovalSetting::dynamic_sync(
            move |_, _, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            },
        )),
    )
    .await
    .unwrap();
    let tools = discover_tools(
        &[(server.client(), ToolFilter::default())],
        &McpDiscoveryOptions::new(),
    )
    .await
    .unwrap();
    assert!(!tools.is_empty());
    assert!(
        tools
            .iter()
            .all(|tool| tool.options().approval() == ToolApprovalPolicy::Always)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn configured_server_retains_approval_after_reconnecting() {
    use ra_mcp::{
        McpServer,
        client::stdio::{McpServerStdio, McpStdioParams},
    };
    let server = McpServerStdio::new(McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture")))
        .with_options(
            McpClientOptions::default()
                .with_require_approval(true)
                .with_tool_input_guardrails([ToolGuardrailId::new("input").unwrap()])
                .with_tool_output_guardrails([ToolGuardrailId::new("output").unwrap()]),
        );
    for _ in 0..2 {
        server.connect().await.unwrap();
        let tools = discover_tools(
            &[(server.client().unwrap(), ToolFilter::default())],
            &McpDiscoveryOptions::new(),
        )
        .await
        .unwrap();
        assert!(!tools.is_empty());
        assert!(
            tools
                .iter()
                .all(|tool| tool.options().approval() == ToolApprovalPolicy::Always)
        );
        for tool in &tools {
            assert_eq!(
                tool.options().input_guardrails(),
                &[ToolGuardrailId::new("input").unwrap()]
            );
            assert_eq!(
                tool.options().output_guardrails(),
                &[ToolGuardrailId::new("output").unwrap()]
            );
        }
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn server_guardrails_have_separate_tool_options_and_do_not_leak_across_servers_or_filters() {
    let input = [
        ToolGuardrailId::new("first_input").unwrap(),
        ToolGuardrailId::new("second_input").unwrap(),
    ];
    let output = [ToolGuardrailId::new("output").unwrap()];
    let mut guarded = support::Fixture::new("guarded");
    guarded.tools_override = Some(
        ["first", "second", "filtered"]
            .map(support::declaration)
            .into(),
    );
    let mut guarded = InProcessToolServer::connect(
        "guarded",
        guarded,
        McpClientOptions::default()
            .with_tool_input_guardrails(input.clone())
            .with_tool_output_guardrails(output.clone())
            .with_tool_filter(ToolFilter::new(None, ["filtered".into()].into())),
    )
    .await
    .unwrap();
    let mut plain = support::Fixture::new("plain");
    plain.tools_override = Some(vec![support::declaration("unguarded")]);
    let mut plain = InProcessToolServer::connect("plain", plain, McpClientOptions::default())
        .await
        .unwrap();
    let tools = discover_tools(
        &[
            (guarded.client(), ToolFilter::default()),
            (plain.client(), ToolFilter::default()),
        ],
        &McpDiscoveryOptions::new(),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 3);
    for tool in &tools {
        let options = tool.options();
        if tool.info().server_name() == "guarded" {
            assert_eq!(options.input_guardrails(), input);
            assert_eq!(options.output_guardrails(), output);
            let changed = options
                .with_input_guardrail(ToolGuardrailId::new("local_input").unwrap())
                .with_output_guardrail(ToolGuardrailId::new("local_output").unwrap());
            assert_eq!(changed.input_guardrails().len(), 3);
            assert_eq!(changed.output_guardrails().len(), 2);
            assert_eq!(tool.options().input_guardrails(), input);
            assert_eq!(tool.options().output_guardrails(), output);
        } else {
            assert!(options.input_guardrails().is_empty());
            assert!(options.output_guardrails().is_empty());
        }
    }
    assert_eq!(guarded.client().tool_input_guardrails(), input);
    assert_eq!(guarded.client().tool_output_guardrails(), output);
    guarded.cleanup().await.unwrap();
    plain.cleanup().await.unwrap();
}
