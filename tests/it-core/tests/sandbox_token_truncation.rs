//! `ra-core::sandbox::token_truncation`: cutting command output to a budget, keeping both ends.
//!
//! Ported from the reference's `tests/sandbox/test_token_truncation.py`, one test per upstream
//! test, followed by exact outputs taken from running the reference's module on the same input —
//! the upstream tests check shapes, and the text a model reads is the contract.

use ra_core::sandbox::token_truncation::{
    APPROX_BYTES_PER_TOKEN, TruncationPolicy, approx_bytes_for_tokens, approx_token_count,
    approx_tokens_from_byte_count, format_truncation_marker, formatted_truncate_text,
    formatted_truncate_text_with_token_count, removed_units_for_source, split_budget, split_string,
    truncate_text, truncate_with_byte_estimate, truncate_with_token_budget,
};

/// Twenty lines of repeated words, the reference's fixture for budgets with metadata.
fn twenty_lines() -> String {
    (0..20)
        .map(|index| format!("line {index}: {}", "value ".repeat(8).trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

// `test_truncation_policy_clamps_negative_limits_and_converts_budgets`
#[test]
fn negative_limits_clamp_to_zero_in_both_units() {
    let byte_policy = TruncationPolicy::bytes(-10);
    let token_policy = TruncationPolicy::tokens(-2);

    assert_eq!(byte_policy.limit(), 0);
    assert_eq!(byte_policy.token_budget(), 0);
    assert_eq!(byte_policy.byte_budget(), 0);
    assert_eq!(token_policy.limit(), 0);
    assert_eq!(token_policy.token_budget(), 0);
    assert_eq!(token_policy.byte_budget(), 0);
}

// `test_formatted_truncate_text_returns_short_content_unchanged`
#[test]
fn content_within_budget_is_returned_unchanged() {
    assert_eq!(
        formatted_truncate_text("short", TruncationPolicy::bytes(20)),
        "short"
    );
}

// `test_formatted_truncate_text_adds_line_count_when_truncated`
#[test]
fn a_cut_adds_the_line_count() {
    let result = formatted_truncate_text("alpha\nbeta\ngamma", TruncationPolicy::bytes(8));

    assert!(result.starts_with("Total output lines: 3\n\n"));
    assert!(result.contains("chars truncated"));
    assert_eq!(
        result,
        "Total output lines: 3\n\nalph…8 chars truncated…amma"
    );
}

// `test_formatted_truncate_text_keeps_token_metadata_within_budget`
#[test]
fn a_token_cut_keeps_its_metadata_within_the_budget() {
    let result = formatted_truncate_text(&twenty_lines(), TruncationPolicy::tokens(32));

    assert!(result.starts_with("Total output lines: 20\n\n"));
    assert!(result.contains("tokens truncated"));
    assert!(approx_token_count(&result) <= 32);
}

// `test_formatted_truncate_text_with_token_count_handles_none_and_short_content`
#[test]
fn no_budget_or_short_content_reports_no_count() {
    assert_eq!(
        formatted_truncate_text_with_token_count("short", None),
        ("short".to_owned(), None)
    );
    assert_eq!(
        formatted_truncate_text_with_token_count("short", Some(10)),
        ("short".to_owned(), None)
    );
}

// `test_formatted_truncate_text_with_token_count_reports_original_count`
#[test]
fn a_cut_reports_the_original_token_count() {
    let (result, original) = formatted_truncate_text_with_token_count("abcdefghi", Some(1));

    assert!(approx_token_count(&result) <= 1);
    assert_eq!(original, Some(approx_token_count("abcdefghi") as u64));
    assert_eq!(result, "…3");
}

// `test_formatted_truncate_text_with_token_count_keeps_metadata_within_budget`
#[test]
fn a_counted_cut_keeps_its_metadata_within_the_budget() {
    let content = twenty_lines();
    let (result, original) = formatted_truncate_text_with_token_count(&content, Some(32));

    assert!(result.starts_with("Total output lines: 20\n\n"));
    assert!(result.contains("tokens truncated"));
    assert!(approx_token_count(&result) <= 32);
    assert_eq!(original, Some(approx_token_count(&content) as u64));
    assert_eq!(
        result,
        "Total output lines: 20\n\nline 0: value value value value value v…263 tokens truncated…\
         lue value value value value value value"
    );
    assert_eq!(original, Some(283));
}

// `test_truncate_text_dispatches_byte_and_token_modes`
#[test]
fn truncate_text_follows_the_policys_unit() {
    let bytes = truncate_text("abcdef", TruncationPolicy::bytes(4));
    assert!(bytes.starts_with('a'));
    assert_eq!(bytes, "ab…2 chars truncated…ef");

    let tokens = truncate_text(
        &"abcdefghijklmnopqrstuvwxyz".repeat(2),
        TruncationPolicy::tokens(8),
    );
    assert!(tokens.contains("tokens truncated"));
    assert!(approx_token_count(&tokens) <= 8);
}

// `test_truncate_with_token_budget_handles_empty_and_short_content`
#[test]
fn a_token_budget_leaves_empty_and_short_content_alone() {
    assert_eq!(
        truncate_with_token_budget("", TruncationPolicy::tokens(1)),
        (String::new(), None)
    );
    assert_eq!(
        truncate_with_token_budget("abc", TruncationPolicy::tokens(1)),
        ("abc".to_owned(), None)
    );
}

// `test_truncate_with_token_budget_includes_marker_within_budget`
#[test]
fn a_token_budget_counts_the_marker_against_itself() {
    let content = "abcdefghijklmnopqrstuvwxyz".repeat(2);
    let (result, original) = truncate_with_token_budget(&content, TruncationPolicy::tokens(8));

    assert!(result.contains("tokens truncated"));
    assert!(approx_token_count(&result) <= 8);
    assert_eq!(original, Some(approx_token_count(&content) as u64));
    assert_eq!(result, "abc…12 tokens truncated…wxyz");
}

// `test_formatted_truncate_text_with_zero_token_budget_returns_empty_payload`
#[test]
fn a_zero_token_budget_returns_nothing_but_still_counts() {
    let (result, original) = formatted_truncate_text_with_token_count("content", Some(0));

    assert_eq!(result, "");
    assert_eq!(original, Some(approx_token_count("content") as u64));
}

// `test_truncate_with_byte_estimate_handles_empty_zero_and_short_content`
#[test]
fn a_byte_budget_handles_empty_zero_and_short_content() {
    assert_eq!(
        truncate_with_byte_estimate("", TruncationPolicy::bytes(0)),
        ""
    );
    let zero = truncate_with_byte_estimate("abc", TruncationPolicy::bytes(0));
    assert!(zero.contains("chars truncated"));
    assert_eq!(zero, "…3 chars truncated…");
    assert_eq!(
        truncate_with_byte_estimate("abc", TruncationPolicy::bytes(10)),
        "abc"
    );
}

// `test_split_string_preserves_utf8_boundaries`
#[test]
fn a_split_never_cuts_a_character() {
    let (removed_chars, prefix, suffix) = split_string("aあbいc", 2, 4);

    assert_eq!(prefix, "a");
    assert_eq!(suffix, "いc");
    assert_eq!(removed_chars, 2);
}

// `test_split_string_handles_empty_content`
#[test]
fn splitting_nothing_gives_nothing() {
    assert_eq!(split_string("", 10, 10), (0, String::new(), String::new()));
}

// `test_formatting_and_estimate_helpers`
#[test]
fn the_formatting_and_estimate_helpers() {
    let byte_policy = TruncationPolicy::bytes(8);
    let token_policy = TruncationPolicy::tokens(2);

    assert!(format_truncation_marker(byte_policy, 3).contains("chars truncated"));
    assert!(format_truncation_marker(token_policy, 2).contains("tokens truncated"));
    assert_eq!(split_budget(5), (2, 3));
    assert_eq!(removed_units_for_source(byte_policy, 10, 4), 4);
    assert_eq!(removed_units_for_source(token_policy, 9, 4), 3);
    assert_eq!(approx_token_count("abcde"), 2);
    assert_eq!(approx_bytes_for_tokens(-1), 0);
    assert_eq!(approx_tokens_from_byte_count(0), 0);
    assert_eq!(approx_tokens_from_byte_count(5), 2);
    assert_eq!(APPROX_BYTES_PER_TOKEN, 4);
}

// Beyond the upstream file: exact outputs from the reference's own module.

/// The case the shell tool's upstream test pins: a budget smaller than the marker returns as much
/// of the marker as fits, without splitting the ellipsis.
#[test]
fn a_budget_smaller_than_the_marker_keeps_what_fits_of_it() {
    assert_eq!(
        formatted_truncate_text_with_token_count("stdout: pwd\nstderr: pwd", Some(2)),
        ("…6 tok".to_owned(), Some(6))
    );
    assert_eq!(
        formatted_truncate_text_with_token_count("héllo wörld ünïcode", Some(3)),
        ("…6 tokens ".to_owned(), Some(6))
    );
}

/// Lines are counted as Python's `splitlines` counts them: `\r\n` is one boundary, and form feeds
/// and the Unicode separators are boundaries too.
#[test]
fn lines_are_counted_as_splitlines_counts_them() {
    let prefix = |content: &str| {
        formatted_truncate_text(content, TruncationPolicy::bytes(1))
            .split("\n\n")
            .next()
            .map(str::to_owned)
    };

    assert_eq!(prefix("a\n").as_deref(), Some("Total output lines: 1"));
    assert_eq!(prefix("a\n\nb").as_deref(), Some("Total output lines: 3"));
    assert_eq!(
        prefix("a\r\nb\rc").as_deref(),
        Some("Total output lines: 3")
    );
    assert_eq!(
        prefix("a\u{2028}b\u{0c}").as_deref(),
        Some("Total output lines: 2")
    );
    assert_eq!(
        formatted_truncate_text(&"a\r\nb c\u{0c}d".repeat(10), TruncationPolicy::tokens(4)),
        "…20 tokens tru"
    );
}
