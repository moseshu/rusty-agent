//! The parser's handling of the builder across files, which this port lends to the policy builtins
//! through a thread-local rather than Codex's evaluator `extra` value.

use ra_execpolicy::{Decision, PolicyParser, RuleMatch};

fn tokens(cmd: &[&str]) -> Vec<String> {
    cmd.iter().map(std::string::ToString::to_string).collect()
}

/// As Codex's in-place builder keeps them, the rules a file added before it failed stay with the
/// parser, and later files add to them.
#[test]
fn rules_added_before_an_error_stay_with_the_parser() {
    let mut parser = PolicyParser::new();
    let error = parser
        .parse(
            "broken.rules",
            r#"
prefix_rule(pattern = ["git"], decision = "prompt")
prefix_rule(pattern = ["ls"], decision = "maybe")
"#,
        )
        .expect_err("an unknown decision fails the file");
    assert!(
        error.to_string().contains("invalid decision: maybe"),
        "{error}"
    );

    parser
        .parse(
            "next.rules",
            r#"prefix_rule(pattern = ["cat"], decision = "forbidden")"#,
        )
        .expect("the parser keeps working after an error");
    let policy = parser.build();

    let git = policy.check(&tokens(&["git", "status"]), &|_| Decision::Allow);
    assert_eq!(git.decision, Decision::Prompt);
    let cat = policy.check(&tokens(&["cat", "x"]), &|_| Decision::Allow);
    assert_eq!(cat.decision, Decision::Forbidden);
    let ls = policy.check(&tokens(&["ls"]), &|_| Decision::Allow);
    assert!(matches!(
        ls.matched_rules.as_slice(),
        [RuleMatch::HeuristicsRuleMatch { .. }]
    ));
}

/// Two parsers on one thread keep their rules apart, including one parsed between the other's
/// files.
#[test]
fn parsers_on_one_thread_keep_their_rules_apart() {
    let mut first = PolicyParser::new();
    let mut second = PolicyParser::new();
    first
        .parse("first.rules", r#"prefix_rule(pattern = ["git"])"#)
        .unwrap();
    second
        .parse(
            "second.rules",
            r#"prefix_rule(pattern = ["rm"], decision = "forbidden")"#,
        )
        .unwrap();
    first
        .parse(
            "first-more.rules",
            r#"prefix_rule(pattern = ["ls"], decision = "prompt")"#,
        )
        .unwrap();

    let first = first.build();
    let second = second.build();
    assert_eq!(
        first
            .rules()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        ["git", "ls"].into_iter().collect()
    );
    assert_eq!(
        second
            .rules()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["rm"]
    );
}

/// The extended dialect with f-strings, standard globals and ordinary Starlark control flow are
/// available to a rules file, as in Codex.
#[test]
fn rules_files_are_full_starlark() {
    let mut parser = PolicyParser::new();
    parser
        .parse(
            "loop.rules",
            r#"
tools = ["fmt", "check"]
for tool in tools:
    prefix_rule(pattern = ["cargo", tool], justification = f"cargo {tool} is safe")
"#,
        )
        .expect("loops and f-strings evaluate");
    let policy = parser.build();
    let evaluation = policy.check(&tokens(&["cargo", "check"]), &|_| Decision::Prompt);
    assert_eq!(
        evaluation.matched_rules,
        vec![RuleMatch::PrefixRuleMatch {
            matched_prefix: tokens(&["cargo", "check"]),
            decision: Decision::Allow,
            resolved_program: None,
            justification: Some("cargo check is safe".to_string()),
        }]
    );
}
