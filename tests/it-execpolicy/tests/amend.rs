//! Ported from the tests of Codex's `execpolicy/src/amend.rs`.

use ra_execpolicy::{
    Decision, NetworkRuleProtocol, blocking_append_allow_prefix_rule, blocking_append_network_rule,
};
use tempfile::tempdir;

#[test]
fn appends_rule_and_creates_directories() {
    let tmp = tempdir().expect("create temp dir");
    let policy_path = tmp.path().join("rules").join("default.rules");

    blocking_append_allow_prefix_rule(
        &policy_path,
        &[String::from("echo"), String::from("Hello, world!")],
    )
    .expect("append rule");

    let contents = std::fs::read_to_string(&policy_path).expect("default.rules should exist");
    assert_eq!(
        contents,
        r#"prefix_rule(pattern=["echo", "Hello, world!"], decision="allow")
"#
    );
}

#[test]
fn appends_rule_without_duplicate_newline() {
    let tmp = tempdir().expect("create temp dir");
    let policy_path = tmp.path().join("rules").join("default.rules");
    std::fs::create_dir_all(policy_path.parent().unwrap()).expect("create policy dir");
    std::fs::write(
        &policy_path,
        r#"prefix_rule(pattern=["ls"], decision="allow")
"#,
    )
    .expect("write seed rule");

    blocking_append_allow_prefix_rule(
        &policy_path,
        &[String::from("echo"), String::from("Hello, world!")],
    )
    .expect("append rule");

    let contents = std::fs::read_to_string(&policy_path).expect("read policy");
    assert_eq!(
        contents,
        r#"prefix_rule(pattern=["ls"], decision="allow")
prefix_rule(pattern=["echo", "Hello, world!"], decision="allow")
"#
    );
}

#[test]
fn inserts_newline_when_missing_before_append() {
    let tmp = tempdir().expect("create temp dir");
    let policy_path = tmp.path().join("rules").join("default.rules");
    std::fs::create_dir_all(policy_path.parent().unwrap()).expect("create policy dir");
    std::fs::write(
        &policy_path,
        r#"prefix_rule(pattern=["ls"], decision="allow")"#,
    )
    .expect("write seed rule without newline");

    blocking_append_allow_prefix_rule(
        &policy_path,
        &[String::from("echo"), String::from("Hello, world!")],
    )
    .expect("append rule");

    let contents = std::fs::read_to_string(&policy_path).expect("read policy");
    assert_eq!(
        contents,
        r#"prefix_rule(pattern=["ls"], decision="allow")
prefix_rule(pattern=["echo", "Hello, world!"], decision="allow")
"#
    );
}

#[test]
fn appends_network_rule() {
    let tmp = tempdir().expect("create temp dir");
    let policy_path = tmp.path().join("rules").join("default.rules");

    blocking_append_network_rule(
        &policy_path,
        "Api.GitHub.com",
        NetworkRuleProtocol::Https,
        Decision::Allow,
        Some("Allow https_connect access to api.github.com"),
    )
    .expect("append network rule");

    let contents = std::fs::read_to_string(&policy_path).expect("read policy");
    assert_eq!(
        contents,
        r#"network_rule(host="api.github.com", protocol="https", decision="allow", justification="Allow https_connect access to api.github.com")
"#
    );
}

#[test]
fn appends_prefix_and_network_rules() {
    let tmp = tempdir().expect("create temp dir");
    let policy_path = tmp.path().join("rules").join("default.rules");

    blocking_append_allow_prefix_rule(&policy_path, &[String::from("curl")])
        .expect("append prefix rule");
    blocking_append_network_rule(
        &policy_path,
        "api.github.com",
        NetworkRuleProtocol::Https,
        Decision::Allow,
        Some("Allow https_connect access to api.github.com"),
    )
    .expect("append network rule");

    let contents = std::fs::read_to_string(&policy_path).expect("read policy");
    assert_eq!(
        contents,
        r#"prefix_rule(pattern=["curl"], decision="allow")
network_rule(host="api.github.com", protocol="https", decision="allow", justification="Allow https_connect access to api.github.com")
"#
    );
}

#[test]
fn rejects_wildcard_network_rule_host() {
    let tmp = tempdir().expect("create temp dir");
    let policy_path = tmp.path().join("rules").join("default.rules");
    let err = blocking_append_network_rule(
        &policy_path,
        "*.example.com",
        NetworkRuleProtocol::Https,
        Decision::Allow,
        /*justification*/ None,
    )
    .expect_err("wildcards should be rejected");
    assert_eq!(
        err.to_string(),
        "invalid network rule: invalid rule: network_rule host must be a specific host; wildcards are not allowed"
    );
}

#[test]
fn appends_concurrently_without_losing_or_duplicating_rules() {
    let tmp = tempdir().expect("create temp dir");
    let policy_path = tmp.path().join("rules").join("default.rules");
    std::thread::scope(|scope| {
        for index in 0..8 {
            let policy_path = &policy_path;
            scope.spawn(move || {
                for _ in 0..2 {
                    blocking_append_allow_prefix_rule(
                        policy_path,
                        &[String::from("tool"), format!("{index}")],
                    )
                    .expect("append rule");
                }
            });
        }
    });

    let contents = std::fs::read_to_string(&policy_path).expect("read policy");
    let mut lines: Vec<&str> = contents.lines().collect();
    lines.sort_unstable();
    let expected: Vec<String> = (0..8)
        .map(|index| format!(r#"prefix_rule(pattern=["tool", "{index}"], decision="allow")"#))
        .collect();
    assert_eq!(lines, expected);
    assert!(contents.ends_with('\n'));
}
