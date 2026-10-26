// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(clippy::expect_used)]

use ra_core::hook::HookEventName;

use ra_coding::hooks::matcher::HookMatcher;
use ra_coding::hooks::matcher::matcher_pattern_for_event;
use ra_coding::hooks::matcher::matches_matcher;

#[test]
fn matcher_omitted_matches_all_occurrences() {
    assert!(matches_matcher(/*matcher*/ None, Some("Bash")));
    assert!(matches_matcher(/*matcher*/ None, Some("Write")));
}

#[test]
fn matcher_star_matches_all_occurrences() {
    assert!(matches_matcher(
        Some(&HookMatcher::new("*").expect("valid matcher")),
        Some("Bash")
    ));
    assert!(matches_matcher(
        Some(&HookMatcher::new("*").expect("valid matcher")),
        Some("Edit")
    ));
    assert_eq!(HookMatcher::new("*").map(|_| ()), Ok(()));
}

#[test]
fn matcher_empty_string_matches_all_occurrences() {
    assert!(matches_matcher(
        Some(&HookMatcher::new("").expect("valid matcher")),
        Some("Bash")
    ));
    assert!(matches_matcher(
        Some(&HookMatcher::new("").expect("valid matcher")),
        Some("SessionStart")
    ));
    assert_eq!(HookMatcher::new("").map(|_| ()), Ok(()));
}

#[test]
fn exact_matcher_supports_pipe_alternatives() {
    assert!(matches_matcher(
        Some(&HookMatcher::new("Edit|Write").expect("valid matcher")),
        Some("Edit")
    ));
    assert!(matches_matcher(
        Some(&HookMatcher::new("Edit|Write").expect("valid matcher")),
        Some("Write")
    ));
    assert!(!matches_matcher(
        Some(&HookMatcher::new("Edit|Write").expect("valid matcher")),
        Some("Bash")
    ));
    assert_eq!(HookMatcher::new("Edit|Write").map(|_| ()), Ok(()));
}

#[test]
fn literal_matcher_uses_exact_matching() {
    assert!(matches_matcher(
        Some(&HookMatcher::new("Bash").expect("valid matcher")),
        Some("Bash")
    ));
    assert!(!matches_matcher(
        Some(&HookMatcher::new("Bash").expect("valid matcher")),
        Some("BashOutput")
    ));
    assert!(matches_matcher(
        Some(&HookMatcher::new("mcp__memory__create_entities").expect("valid matcher")),
        Some("mcp__memory__create_entities")
    ));
    assert!(!matches_matcher(
        Some(&HookMatcher::new("mcp__memory").expect("valid matcher")),
        Some("mcp__memory__create_entities")
    ));
    assert_eq!(HookMatcher::new("mcp__memory").map(|_| ()), Ok(()));
}

#[test]
fn matcher_uses_regex_when_it_contains_regex_characters() {
    assert!(matches_matcher(
        Some(&HookMatcher::new("^Bash").expect("valid matcher")),
        Some("BashOutput")
    ));
    assert_eq!(HookMatcher::new("^Bash").map(|_| ()), Ok(()));
}

#[test]
fn mcp_matchers_support_regex_wildcards() {
    assert!(matches_matcher(
        Some(&HookMatcher::new("mcp__memory__.*").expect("valid matcher")),
        Some("mcp__memory__create_entities")
    ));
    assert!(matches_matcher(
        Some(&HookMatcher::new("mcp__.*__write.*").expect("valid matcher")),
        Some("mcp__filesystem__write_file")
    ));
    assert!(!matches_matcher(
        Some(&HookMatcher::new("mcp__.*__write.*").expect("valid matcher")),
        Some("mcp__filesystem__read_file")
    ));
    assert_eq!(HookMatcher::new("mcp__memory__.*").map(|_| ()), Ok(()));
}

#[test]
fn matcher_supports_anchored_regexes() {
    assert!(matches_matcher(
        Some(&HookMatcher::new("^Bash$").expect("valid matcher")),
        Some("Bash")
    ));
    assert!(!matches_matcher(
        Some(&HookMatcher::new("^Bash$").expect("valid matcher")),
        Some("BashOutput")
    ));
    assert_eq!(HookMatcher::new("^Bash$").map(|_| ()), Ok(()));
}

#[test]
fn invalid_regex_is_rejected() {
    assert!(HookMatcher::new("[").is_err());
}

#[test]
fn unsupported_events_ignore_matchers() {
    assert_eq!(
        matcher_pattern_for_event(HookEventName::UserPromptSubmit, Some("^hello")),
        None
    );
    assert_eq!(
        matcher_pattern_for_event(HookEventName::Stop, Some("^done$")),
        None
    );
    assert_eq!(
        matcher_pattern_for_event(HookEventName::Interrupt, Some("^interrupted$")),
        None
    );
}

#[test]
fn supported_events_keep_matchers() {
    assert_eq!(
        matcher_pattern_for_event(HookEventName::PreToolUse, Some("Bash")),
        Some("Bash")
    );
    assert_eq!(
        matcher_pattern_for_event(HookEventName::PostToolUse, Some("Edit|Write")),
        Some("Edit|Write")
    );
    assert_eq!(
        matcher_pattern_for_event(HookEventName::SessionStart, Some("startup|resume")),
        Some("startup|resume")
    );
    assert_eq!(
        matcher_pattern_for_event(HookEventName::SessionEnd, Some("clear|other")),
        Some("clear|other")
    );
    assert_eq!(
        matcher_pattern_for_event(HookEventName::PreCompact, Some("^auto$")),
        Some("^auto$")
    );
    assert_eq!(
        matcher_pattern_for_event(HookEventName::PostCompact, Some("manual|auto")),
        Some("manual|auto")
    );
}
