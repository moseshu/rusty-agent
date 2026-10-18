// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Naming and filtering cases ported from Codex connection_manager_tests, and Python's
//! prefixed-name cases from tests/mcp/test_mcp_util.py with names produced by running upstream.
#[path = "../support/mod.rs"]
mod support;
use ra_mcp::{
    catalog::{
        McpToolInfo, name_tools_for_function_calling,
        normalize_tools_for_model_with_prefix as normalize,
    },
    filter::{ToolFilterStatic, tool_is_model_visible},
};
use serde_json::json;
use std::collections::{BTreeSet, HashSet};

fn info(server: &str, tool: &str) -> McpToolInfo {
    McpToolInfo::new(server, support::declaration(tool))
}

#[test]
fn short_names_and_prefix_exemptions_keep_raw_identity() {
    let tools = normalize(
        vec![info("history", "search"), info("notes", "read")],
        true,
        &["history".into()],
    );
    assert_eq!(tools[0].callable_namespace(), Some("history"));
    assert_eq!(tools[1].callable_namespace(), Some("mcp__notes"));
    assert_eq!(tools[0].tool().name, "search");
    assert_eq!(
        normalize(vec![info("history", "search")], false, &[])[0].callable_namespace(),
        Some("history")
    );
}
#[test]
fn duplicate_raw_identity_is_skipped() {
    assert_eq!(
        normalize(vec![info("a", "read"), info("a", "read")], true, &[]).len(),
        1
    );
}
#[test]
fn sanitization_preserves_original_protocol_names() {
    let tools = normalize(vec![info("server.one", "tool.two-three")], true, &[]);
    assert_eq!(tools[0].callable_namespace(), Some("mcp__server_one"));
    assert_eq!(tools[0].callable_name(), "tool_two_three");
    assert_eq!(tools[0].server_name(), "server.one");
    assert_eq!(tools[0].tool().name, "tool.two-three");
}
#[test]
fn namespace_and_tool_collisions_are_stable_across_input_order() {
    let inputs = vec![
        info("basic-server", "lookup"),
        info("basic_server", "lookup"),
        info("one", "tool-name"),
        info("one", "tool_name"),
        info("服务", "搜索"),
    ];
    let first = normalize(inputs.clone(), true, &[]);
    let second = normalize(inputs.into_iter().rev(), true, &[]);
    assert_eq!(first, second);
    let names: HashSet<_> = first
        .iter()
        .map(|t| (t.callable_namespace(), t.callable_name()))
        .collect();
    assert_eq!(names.len(), 5);
    assert!(first.iter().all(|t| {
        t.callable_name()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    }));
}
#[test]
fn long_names_fit_both_responses_parts_and_the_codex_combined_limit() {
    for length in [64, 65, 80, 128, 129, 256, 1024] {
        let tools = normalize(
            vec![
                info(&"s".repeat(length), &"a".repeat(length)),
                info(&"s".repeat(length), &"b".repeat(length)),
            ],
            true,
            &[],
        );
        assert!(tools.iter().all(|t| {
            let namespace = t.callable_namespace().unwrap();
            namespace.len() <= 64
                && t.callable_name().len() <= 64
                && namespace.len() + 2 + t.callable_name().len() <= 128
        }));
        assert_eq!(tools[0].callable_namespace(), tools[1].callable_namespace());
        assert_ne!(tools[0].callable_name(), tools[1].callable_name());
    }
    for length in [64, 65, 80] {
        let raw = "a".repeat(length);
        let tools = normalize(vec![info("a", &raw)], true, &[]);
        assert_eq!(tools[0].callable_namespace(), Some("mcp__a"));
        assert_eq!(tools[0].callable_name().len(), 64);
        assert_eq!(tools[0].callable_name() == raw, length == 64);
    }
}

#[test]
fn shortened_namespaces_are_shared_and_cannot_merge_with_literal_names() {
    let raw = "s".repeat(80);
    let first = normalize(vec![info(&raw, "search")], false, &[]);
    let literal = first[0].callable_namespace().unwrap();
    let inputs = vec![
        info(&raw, "search"),
        info(&raw, "fetch"),
        info(literal, "search"),
        info(&format!("{raw}!"), "search"),
    ];
    let forward = normalize(inputs.clone(), false, &[]);
    assert_eq!(forward, normalize(inputs.into_iter().rev(), false, &[]));
    let server_tools: Vec<_> = forward
        .iter()
        .filter(|tool| tool.server_name() == raw)
        .collect();
    assert_eq!(
        server_tools[0].callable_namespace(),
        server_tools[1].callable_namespace()
    );
    let namespaces: HashSet<_> = forward
        .iter()
        .map(|tool| tool.callable_namespace())
        .collect();
    assert_eq!(namespaces.len(), 3);
    assert_eq!(
        forward
            .iter()
            .find(|tool| tool.server_name() == literal)
            .unwrap()
            .callable_namespace(),
        Some(literal)
    );
}
#[test]
fn filters_distinguish_missing_and_empty_allowlist_and_deny_wins() {
    assert!(ToolFilterStatic::default().allows("any"));
    assert!(!ToolFilterStatic::new(Some(HashSet::new()), HashSet::new()).allows("any"));
    let filter = ToolFilterStatic::new(
        Some(HashSet::from(["read".into(), "write".into()])),
        HashSet::from(["write".into()]),
    );
    assert!(filter.allows("read"));
    assert!(!filter.allows("write"));
    assert!(!filter.allows("other"));
}
#[test]
fn ui_visibility_requires_model_when_explicit() {
    for (visibility, expected) in [
        (json!(["app"]), false),
        (json!([]), false),
        (json!(["app", "model"]), true),
    ] {
        let mut tool = support::declaration("read");
        tool.meta = Some(serde_json::from_value(json!({"ui":{"visibility":visibility}})).unwrap());
        assert_eq!(tool_is_model_visible(&tool), expected);
    }
    assert!(tool_is_model_visible(&support::declaration("read")));
}

#[test]
fn python_metadata_precedence_keeps_ui_description_separate() {
    for (description, title, annotation, expected_title, expected_description) in [
        (
            Some("long description"),
            Some("explicit"),
            Some("annotation"),
            Some("explicit"),
            "long description",
        ),
        (
            None,
            Some("explicit"),
            Some("annotation"),
            Some("explicit"),
            "explicit",
        ),
        (
            Some(""),
            Some(""),
            Some("annotation"),
            Some("annotation"),
            "annotation",
        ),
        (None, None, None, None, ""),
    ] {
        let tool = serde_json::from_value(json!({"name":"search","inputSchema":{},
            "description":description,"title":title,"annotations":{"title":annotation}}))
        .unwrap();
        let info = McpToolInfo::new("server", tool);
        assert_eq!(info.title(), expected_title);
        assert_eq!(info.description_for_model(), expected_description);
        assert_eq!(
            info.description_for_ui(),
            description.filter(|s| !s.is_empty())
        );
    }
}

#[test]
fn url_derived_names_do_not_expose_credentials_to_the_model_or_debug_output() {
    let original =
        "streamable_http: https://username:password@mcp.example.test:8443/mcp?token=secret#private";
    let tools = normalize(vec![info(original, "search")], true, &[]);
    let visible = format!(
        "{:?} {} {}",
        tools[0],
        tools[0].callable_name(),
        tools[0].callable_namespace().unwrap()
    );
    for secret in ["username", "password", "secret", "private"] {
        assert!(!visible.contains(secret));
    }
    assert_eq!(tools[0].server_name(), original);
    let invalid = normalize(
        vec![info(
            "https://username:password@host:invalid/?secret",
            "search",
        )],
        true,
        &[],
    );
    assert!(!format!("{:?}", invalid[0]).contains("password"));
}

// Each entry is one server and its tools, in listing order.
fn python_names(servers: &[(&str, &[&str])], reserved: &[&str]) -> Vec<String> {
    let tools = servers
        .iter()
        .enumerate()
        .flat_map(|(index, (server, tools))| tools.iter().map(move |tool| (index, *server, *tool)))
        .map(|(index, server, tool)| info(server, tool).with_server_index(index))
        .collect();
    let reserved: BTreeSet<String> = reserved.iter().map(|name| (*name).to_owned()).collect();
    name_tools_for_function_calling(tools, true, &reserved)
        .unwrap()
        .iter()
        .map(|tool| {
            assert!(tool.callable_namespace().is_none());
            tool.callable_name().to_owned()
        })
        .collect()
}

#[test]
fn python_prefixed_names_match_upstream_output() {
    let long_server = format!("server_{}", "a".repeat(100));
    let long_tool = format!("tool_{}", "b".repeat(100));
    let long_tools = [long_tool.as_str()];
    type NameCase<'a> = (Vec<(&'a str, &'a [&'a str])>, Vec<&'a str>, Vec<&'a str>);
    let cases: Vec<NameCase<'_>> = vec![
        (
            vec![
                ("docs", &["search", "fetch"]),
                ("calendar", &["search", "update"]),
            ],
            vec![],
            vec![
                "mcp_docs__search",
                "mcp_docs__fetch",
                "mcp_calendar__search",
                "mcp_calendar__update",
            ],
        ),
        (
            vec![
                (long_server.as_str(), &long_tools[..]),
                (long_server.as_str(), &long_tools[..]),
            ],
            vec![],
            vec![
                "mcp_server_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_901e5c80",
                "mcp_server_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_0e47f3a5",
            ],
        ),
        (
            vec![
                ("foo", &["create_issue"]),
                ("foo!", &["create_issue"]),
                ("foo_0beec7b5", &["create_issue"]),
            ],
            vec![],
            vec![
                "mcp_foo__create_issue_b2b06633",
                "mcp_foo__create_issue_48819d22",
                "mcp_foo_0beec7b5__create_issue",
            ],
        ),
        (
            vec![("docs", &["search", "search!"])],
            vec![],
            vec!["mcp_docs__search_15de6fa1", "mcp_docs__search_afdecfe2"],
        ),
        (
            vec![("天気サーバー", &["search"]), ("docs", &["検索"])],
            vec![],
            vec!["mcp_server__search", "mcp_docs__tool"],
        ),
        (
            vec![(
                "streamable_http: https://user:s3cr3t_pw@mcp.example.test:8443/mcp?api_key=K#F",
                &["search"],
            )],
            vec![],
            vec!["mcp_streamable_http__https___mcp_example_test_8443_mcp__search"],
        ),
        (
            vec![("docs", &["search"])],
            vec!["mcp_docs__search"],
            vec!["mcp_docs__search_15de6fa1"],
        ),
    ];
    for (servers, reserved, expected) in cases {
        assert_eq!(python_names(&servers, &reserved), expected);
    }
}

#[test]
fn python_original_names_reject_duplicates_across_servers_with_escaped_names() {
    for (name, escaped) in [
        (
            "alpha\nFORGED\x1b[31m, trailing",
            "'alpha\\nFORGED\\x1b[31m, trailing'",
        ),
        (
            "alpha\rFORGED\u{2028}\u{202e}",
            "'alpha\\rFORGED\\u2028\\u202e'",
        ),
    ] {
        let error = python_original(&[("server_1", &[name]), ("server_2", &[name])]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&format!(
            "Duplicate tool names found across MCP servers: {escaped}. "
        )));
        assert!(message.contains("McpToolNaming::ServerPrefixed"));
        assert!(
            !["\n", "\r", "\x1b", "\u{2028}", "\u{202e}"]
                .iter()
                .any(|control| message.contains(control))
        );
    }
    let error = python_original(&[
        ("server_1", &["zeta", "alpha"]),
        ("server_2", &["alpha", "zeta"]),
    ])
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Duplicate tool names found across MCP servers: 'alpha', 'zeta'. ")
    );
    let names = python_original(&[("docs", &["search.v2"]), ("calendar", &["update"])]).unwrap();
    assert_eq!(names, ["search.v2", "update"]);
}

fn python_original(servers: &[(&str, &[&str])]) -> ra_core::error::Result<Vec<String>> {
    let mut tools = Vec::new();
    for (index, (server, names)) in servers.iter().enumerate() {
        for name in *names {
            tools.push(
                McpToolInfo::new(*server, support::declaration(name)).with_server_index(index),
            );
        }
    }
    Ok(
        name_tools_for_function_calling(tools, false, &BTreeSet::new())?
            .iter()
            .map(|tool| tool.callable_name().to_owned())
            .collect(),
    )
}
