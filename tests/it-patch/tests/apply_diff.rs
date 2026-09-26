//! `ra_patch::apply_diff`: the reference's V4A applier.
//!
//! Ported from the reference's `tests/test_apply_diff.py`, one test per upstream test in the
//! upstream order, followed by `tests/test_apply_diff_helpers.py`. The helper tests call private
//! functions upstream; each is restated here through `apply_diff` with an input that reaches the
//! same branch, and the two that no input can reach are named where they would be.

use ra_patch::{ApplyDiffMode, apply_diff};

fn update(input: &str, diff: &str) -> Result<String, String> {
    apply_diff(input, diff, ApplyDiffMode::Default).map_err(|error| error.message().to_owned())
}

fn create(diff: &str) -> Result<String, String> {
    apply_diff("", diff, ApplyDiffMode::Create).map_err(|error| error.message().to_owned())
}

fn lines(lines: &[&str]) -> String {
    lines.join("\n")
}

// ---- test_apply_diff.py --------------------------------------------------------------------

#[test]
fn a_floating_hunk_adds_lines() {
    let diff = lines(&["@@", "+hello", "+world"]);
    assert_eq!(update("", &diff).unwrap(), "hello\nworld\n");
}

#[test]
fn an_empty_input_takes_the_diffs_crlf() {
    let diff = ["@@", "+hello", "+world"].join("\r\n");
    assert_eq!(update("", &diff).unwrap(), "hello\r\nworld\r\n");
}

#[test]
fn a_created_file_needs_plus_lines() {
    assert_eq!(
        create("plain line").unwrap_err(),
        "Invalid Add File Line: plain line"
    );
}

#[test]
fn a_created_file_keeps_its_trailing_newline() {
    let diff = lines(&["+hello", "+world", "+"]);
    assert_eq!(create(&diff).unwrap(), "hello\nworld\n");
}

#[test]
fn a_contextual_replacement_is_applied() {
    let diff = lines(&["@@ line1", "-line2", "+updated", " line3"]);
    assert_eq!(
        update("line1\nline2\nline3\n", &diff).unwrap(),
        "line1\nupdated\nline3\n"
    );
}

#[test]
fn stacked_anchors_from_the_tool_description_are_applied() {
    let input = lines(&[
        "class BaseClass",
        "    def search():",
        "        pass",
        "",
        "class Subclass",
        "    def search():",
        "        pass",
    ]) + "\n";
    let diff = lines(&[
        "@@ class BaseClass",
        "@@     def search():",
        "-        pass",
        "+        raise NotImplementedError()",
        "",
        "@@ class Subclass",
        "@@     def search():",
        "-        pass",
        "+        raise NotImplementedError()",
    ]);
    let expected = lines(&[
        "class BaseClass",
        "    def search():",
        "        raise NotImplementedError()",
        "",
        "class Subclass",
        "    def search():",
        "        raise NotImplementedError()",
    ]) + "\n";
    assert_eq!(update(&input, &diff).unwrap(), expected);
}

#[test]
fn a_parent_anchor_already_passed_is_reused_by_the_next_hunk() {
    let input = lines(&[
        "class Target",
        "    def first():",
        "        pass",
        "",
        "    def second():",
        "        pass",
    ]) + "\n";
    let diff = lines(&[
        "@@ class Target",
        "@@     def first():",
        "-        pass",
        "+        return 1",
        "@@ class Target",
        "@@     def second():",
        "-        pass",
        "+        return 2",
    ]);
    let expected = lines(&[
        "class Target",
        "    def first():",
        "        return 1",
        "",
        "    def second():",
        "        return 2",
    ]) + "\n";
    assert_eq!(update(&input, &diff).unwrap(), expected);
}

#[test]
fn stacked_anchors_narrow_to_the_named_block() {
    let input = lines(&[
        "class First",
        "    def target():",
        "        return 0",
        "",
        "class Second",
        "    def helper():",
        "        pass",
        "",
        "    def target():",
        "        pass",
    ]) + "\n";
    let diff = lines(&[
        "@@ class Second",
        "@@     def target():",
        "-        pass",
        "+        return 1",
    ]);
    let expected = lines(&[
        "class First",
        "    def target():",
        "        return 0",
        "",
        "class Second",
        "    def helper():",
        "        pass",
        "",
        "    def target():",
        "        return 1",
    ]) + "\n";
    assert_eq!(update(&input, &diff).unwrap(), expected);
}

#[test]
fn a_single_unmatched_anchor_is_only_advisory() {
    let diff = lines(&["@@ nope", "-b", "+B"]);
    assert_eq!(update("a\nb\n", &diff).unwrap(), "a\nB\n");
}

#[test]
fn partially_matched_stacked_anchors_are_refused() {
    let input = lines(&[
        "class Target",
        "    def helper():",
        "        pass",
        "",
        "    def desired():",
        "        return 1",
    ]) + "\n";
    let diff = lines(&[
        "@@ class Target",
        "@@     def missing():",
        "-        pass",
        "+        return 99",
    ]);
    let error = update(&input, &diff).unwrap_err();
    assert_eq!(error, "Invalid Anchor 1:\n    def missing():");
}

#[test]
fn stacked_anchors_whose_first_is_missing_are_refused() {
    let input = "class Wrong\n    def desired():\n        pass\n";
    let diff = lines(&[
        "@@ class Target",
        "@@     def desired():",
        "-        pass",
        "+        return 99",
    ]);
    assert!(
        update(input, &diff)
            .unwrap_err()
            .starts_with("Invalid Anchor")
    );
}

#[test]
fn a_missing_anchor_followed_by_a_bare_marker_is_refused() {
    let diff = lines(&["@@ missing", "@@", "-b", "+B"]);
    assert_eq!(
        update("a\nb\n", &diff).unwrap_err(),
        "Invalid Anchor 0:\nmissing"
    );
}

#[test]
fn stacked_anchors_accept_a_trailing_bare_marker() {
    let input = "class Only\n    def run():\n        pass\n";
    let diff = lines(&["@@ class Only", "@@", "-        pass", "+        return 1"]);
    assert_eq!(
        update(input, &diff).unwrap(),
        "class Only\n    def run():\n        return 1\n"
    );
}

#[test]
fn a_context_mismatch_is_refused() {
    let diff = lines(&["@@ -1,2 +1,2 @@", " x", "-two", "+2"]);
    assert_eq!(
        update("one\ntwo\n", &diff).unwrap_err(),
        "Invalid Context 0:\nx\ntwo"
    );
}

#[test]
fn a_crlf_input_patched_with_lf_stays_crlf() {
    let diff = lines(&["@@ line1", "-line2", "+updated", " line3"]);
    assert_eq!(
        update("line1\r\nline2\r\nline3\r\n", &diff).unwrap(),
        "line1\r\nupdated\r\nline3\r\n"
    );
}

#[test]
fn an_lf_input_patched_with_crlf_stays_lf() {
    let diff = ["@@ line1", "-line2", "+updated", " line3"].join("\r\n");
    assert_eq!(
        update("line1\nline2\nline3\n", &diff).unwrap(),
        "line1\nupdated\nline3\n"
    );
}

#[test]
fn a_crlf_input_patched_with_crlf_stays_crlf() {
    let diff = ["@@ line1", "-line2", "+updated", " line3"].join("\r\n");
    assert_eq!(
        update("line1\r\nline2\r\nline3\r\n", &diff).unwrap(),
        "line1\r\nupdated\r\nline3\r\n"
    );
}

#[test]
fn a_created_file_keeps_crlf_newlines() {
    let diff = ["+hello", "+world", "+"].join("\r\n");
    assert_eq!(create(&diff).unwrap(), "hello\r\nworld\r\n");
}

#[test]
fn an_end_of_file_hunk_appends_without_a_blank_line() {
    assert_eq!(
        update("a\nb\n", "@@\n+c\n*** End of File").unwrap(),
        "a\nb\nc\n"
    );
    assert_eq!(
        update("a\nb", "@@\n+c\n*** End of File").unwrap(),
        "a\nb\nc"
    );
}

#[test]
fn an_end_of_file_hunk_matches_the_last_occurrence_of_its_context() {
    assert_eq!(
        update("x\nfoo\ny\nfoo\n", " foo\n+added\n*** End of File").unwrap(),
        "x\nfoo\ny\nfoo\nadded\n"
    );
}

#[test]
fn an_end_of_file_hunk_keeps_crlf() {
    assert_eq!(
        update("a\r\nb\r\n", "@@\n+c\n*** End of File").unwrap(),
        "a\r\nb\r\nc\r\n"
    );
}

// ---- test_apply_diff_helpers.py ------------------------------------------------------------

/// `test_normalize_diff_lines_drops_trailing_blank`: one final newline adds no empty line.
#[test]
fn a_diffs_final_newline_is_not_an_extra_line() {
    assert_eq!(update("a\nb\n", "@@\n-b\n+c\n").unwrap(), "a\nc\n");
    assert_eq!(update("a\nb\n", "@@\n-b\n+c").unwrap(), "a\nc\n");
}

// `test_is_done_true_when_index_out_of_range` and `test_read_str_returns_empty_when_missing_prefix`
// check the parser's cursor primitives on states no diff produces: the parser appends an
// end-of-patch sentinel, so it never runs off the end, and a line without the `@@ ` prefix is
// covered by every hunk above that has a bare `@@`.

/// `test_read_section_returns_eof_flag`: an end-of-file marker ends the hunk and anchors it.
#[test]
fn an_end_of_file_marker_alone_anchors_the_insertion_at_the_end() {
    assert_eq!(
        update("a\nb\n", "+c\n*** End of File").unwrap(),
        "a\nb\nc\n"
    );
}

/// `test_read_section_raises_on_invalid_marker`.
#[test]
fn an_unknown_triple_star_line_is_refused() {
    assert_eq!(
        update("a\n", "@@\n*** Bad Marker").unwrap_err(),
        "Invalid Line: *** Bad Marker"
    );
}

/// `test_read_section_raises_when_empty_segment`.
#[test]
fn a_header_with_nothing_under_it_is_refused() {
    assert_eq!(
        update("a\n", "@@").unwrap_err(),
        "Nothing in this section - index=1 *** End Patch"
    );
}

/// `test_find_context_eof_fallbacks`: an end-of-file hunk found nowhere is refused.
#[test]
fn an_end_of_file_hunk_found_nowhere_is_refused() {
    assert_eq!(
        update("one\n", " missing\n+x\n*** End of File").unwrap_err(),
        "Invalid EOF Context 0:\nmissing"
    );
}

/// `test_find_context_eof_ignores_trailing_empty_split_element`: an empty end-of-file hunk lands
/// after the last real line, and the terminating newline is not an empty line context can match.
#[test]
fn an_end_of_file_hunk_ignores_the_terminating_newline() {
    assert_eq!(
        update("a\nb\n", "+z\n*** End of File").unwrap(),
        "a\nb\nz\n"
    );
    assert_eq!(
        update("a\n", " \n+z\n*** End of File").unwrap_err(),
        "Invalid EOF Context 0:\n"
    );
}

/// `test_find_context_core_stripped_matches`: whitespace at both ends is the last level tried.
#[test]
fn context_matches_ignoring_surrounding_whitespace() {
    assert_eq!(
        update(" line \n", "@@\n line\n+next").unwrap(),
        " line \nnext\n"
    );
}

/// `test_apply_chunks_rejects_bad_chunks`, the overlap half. An end-of-file hunk is tried at the end
/// before the cursor is consulted, so it can land on a line an earlier hunk already replaced.
///
/// The other half, a chunk past the end of the input, has no diff that produces it: every chunk is
/// placed inside a context that was found in the input.
#[test]
fn hunks_that_overlap_are_refused() {
    assert_eq!(
        update("a\n", "@@\n-a\n+A\n@@\n-a\n+B\n*** End of File").unwrap_err(),
        "applyDiff: overlapping chunk at 0 (cursor 1)"
    );
}

// ---- beyond the reference's tests ----------------------------------------------------------

/// Python's `str.strip` also strips the ASCII separator controls, which Rust's `trim` keeps.
#[test]
fn ascii_separator_controls_count_as_whitespace_when_matching() {
    assert_eq!(update("\u{1c}a\u{1f}\n", "@@\n-a\n+b").unwrap(), "b\n");
}

/// Only the first hunk may omit its `@@`.
#[test]
fn a_later_hunk_without_a_header_is_refused() {
    assert_eq!(
        update("a\nb\nc\n", "@@\n-a\n+A\n*** End of File\n-c\n+C").unwrap_err(),
        "Invalid Line:\n-c"
    );
}

/// A create stops at the next file header rather than refusing it.
#[test]
fn a_created_file_ends_at_the_next_file_header() {
    assert_eq!(create("+a\n*** Add File: b\n+b").unwrap(), "a");
}
