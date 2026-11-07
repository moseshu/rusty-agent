// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! `apply_chunks` against Codex's update application: the matching cases of
//! `apply-patch/tests/suite/tool.rs`, the file contents `apply-patch/src/file_update_tests.rs`
//! expects, and `seek_sequence`'s own tests, run on contents instead of through the CLI.

use std::path::Path;

use ra_patch::{
    PatchAction, PatchConflict, PatchHunk, PatchMatchLevel, apply_chunks, apply_hunks, parse_patch,
};

fn hunks(patch: &str) -> Vec<PatchHunk> {
    let plan = parse_patch(patch).expect("a valid patch");
    match plan.actions() {
        [PatchAction::UpdateFile { hunks, .. }] => hunks.clone(),
        actions => panic!("one update expected: {actions:?}"),
    }
}

fn update(body: &str) -> Vec<PatchHunk> {
    hunks(&format!(
        "*** Begin Patch\n*** Update File: file.txt\n{body}\n*** End Patch"
    ))
}

fn apply(original: &str, body: &str) -> String {
    apply_chunks(Path::new("file.txt"), original, &update(body))
        .expect("the update applies")
        .content()
        .to_owned()
}

fn fails(original: &str, body: &str) -> PatchConflict {
    apply_chunks(Path::new("file.txt"), original, &update(body))
        .expect_err("the update must not apply")
}

// --- apply-patch/tests/suite/tool.rs --------------------------------------------------------

#[test]
fn applies_multiple_chunks() {
    assert_eq!(
        apply(
            "line1\nline2\nline3\nline4\n",
            "@@\n-line2\n+changed2\n@@\n-line4\n+changed4"
        ),
        "line1\nchanged2\nline3\nchanged4\n"
    );
}

#[test]
fn rejects_overlapping_end_of_file_chunks() {
    let conflict = fails(
        "one\n",
        "@@\n-one\n+first\n@@\n-one\n+second\n*** End of File",
    );
    assert!(
        matches!(conflict, PatchConflict::HunkFailed { hunk_index: 1, .. }),
        "{conflict:?}"
    );
}

#[test]
fn preserves_crlf_from_target_file() {
    assert_eq!(
        apply(
            "one\r\ntwo\r\nthree\r\n",
            "@@\n-one\n+uno\n@@\n two\n+\n+between\n three"
        ),
        "uno\r\ntwo\r\n\r\nbetween\r\nthree\r\n"
    );
}

#[test]
fn appends_after_trailing_blank_crlf_line() {
    assert_eq!(apply("a\r\n\r\n", "@@\n+new"), "a\r\n\r\nnew\r\n");
}

#[test]
fn preserves_cr_from_target_file() {
    assert_eq!(
        apply(
            "one\rtwo\rthree\r",
            "@@\n-one\n+uno\n@@\n two\n+\n+between\n three"
        ),
        "uno\rtwo\r\rbetween\rthree\r"
    );
}

#[test]
fn preserves_change_order_with_repeated_lines() {
    assert_eq!(apply("a\nb\n", "@@\n-a\n-b\n+b\n+b\n+a"), "b\nb\na\n");
}

#[test]
fn preserves_repeated_context_line_ending() {
    assert_eq!(apply("same\r\nsame\n", "@@\n-same\n same"), "same\n");
}

#[test]
fn preserves_untouched_mixed_line_endings() {
    assert_eq!(
        apply(
            "one\r\ntwo\rthree\nfour\r\n",
            "@@\n one\n two\n-three\n+THREE\n four"
        ),
        "one\r\ntwo\rTHREE\r\nfour\r\n"
    );
}

#[test]
fn uses_crlf_for_new_trailing_newline() {
    assert_eq!(apply("one\r\ntwo", "@@\n-one\n+ONE"), "ONE\r\ntwo\r\n");
}

#[test]
fn reports_missing_context_without_changing_anything() {
    let conflict = fails("line1\nline2\n", "@@\n-missing\n+changed");
    assert!(
        matches!(conflict, PatchConflict::HunkFailed { hunk_index: 0, .. }),
        "{conflict:?}"
    );
}

#[test]
fn updates_append_a_trailing_newline() {
    assert_eq!(
        apply(
            "no newline at end",
            "@@\n-no newline at end\n+first line\n+second line"
        ),
        "first line\nsecond line\n"
    );
}

// --- apply-patch/src/file_update_tests.rs (the contents; the unified diffs are Codex's own) ---

#[test]
fn file_update_two_sections() {
    assert_eq!(
        apply(
            "foo\nbar\nbaz\nqux\n",
            "@@\n foo\n-bar\n+BAR\n@@\n baz\n-qux\n+QUX"
        ),
        "foo\nBAR\nbaz\nQUX\n"
    );
}

#[test]
fn file_update_first_line_replacement() {
    assert_eq!(
        apply("foo\nbar\nbaz\n", "@@\n-foo\n+FOO\n bar"),
        "FOO\nbar\nbaz\n"
    );
}

#[test]
fn file_update_last_line_replacement() {
    assert_eq!(
        apply("foo\nbar\nbaz\n", "@@\n foo\n bar\n-baz\n+BAZ"),
        "foo\nbar\nBAZ\n"
    );
}

#[test]
fn file_update_insert_at_eof() {
    assert_eq!(
        apply("foo\nbar\nbaz\n", "@@\n+quux\n*** End of File"),
        "foo\nbar\nbaz\nquux\n"
    );
}

#[test]
fn file_update_interleaved_changes() {
    assert_eq!(
        apply(
            "a\nb\nc\nd\ne\nf\n",
            "@@\n a\n-b\n+B\n@@\n d\n-e\n+E\n@@\n f\n+g\n*** End of File"
        ),
        "a\nB\nc\nd\nE\nf\ng\n"
    );
}

// --- apply-patch/src/seek_sequence.rs -------------------------------------------------------

#[test]
fn seek_sequence_matches_exactly_first() {
    let update = apply_chunks(
        Path::new("file.txt"),
        "foo\nbar\nbaz\n",
        &update("@@\n-bar\n-baz\n+done"),
    )
    .expect("applies");
    assert_eq!(update.content(), "foo\ndone\n");
    assert_eq!(update.match_level(), PatchMatchLevel::Exact);
}

#[test]
fn seek_sequence_ignores_trailing_whitespace() {
    let update = apply_chunks(
        Path::new("file.txt"),
        "foo   \nbar\t\t\n",
        &update("@@\n-foo\n-bar\n+done"),
    )
    .expect("applies");
    assert_eq!(update.content(), "done\n");
    assert_eq!(update.match_level(), PatchMatchLevel::TrimEnd);
}

#[test]
fn seek_sequence_ignores_leading_and_trailing_whitespace() {
    let update = apply_chunks(
        Path::new("file.txt"),
        "    foo   \n   bar\t\n",
        &update("@@\n-foo\n-bar\n+done"),
    )
    .expect("applies");
    assert_eq!(update.content(), "done\n");
    assert_eq!(update.match_level(), PatchMatchLevel::Trim);
}

#[test]
fn seek_sequence_normalizes_typographic_punctuation() {
    let update = apply_chunks(
        Path::new("file.txt"),
        "say \u{201C}hi\u{201D} \u{2014} ok\n",
        &update("@@\n-say \"hi\" - ok\n+done"),
    )
    .expect("applies");
    assert_eq!(update.content(), "done\n");
    assert_eq!(update.match_level(), PatchMatchLevel::Normalized);
}

#[test]
fn seek_sequence_pattern_longer_than_input_does_not_match() {
    assert!(matches!(
        fails("just one line\n", "@@\n-to\n-match\n-these\n+x"),
        PatchConflict::HunkFailed { .. }
    ));
}

// --- Semantics this port carries over, against the stricter applier ------------------------

/// Repeated context applies at its first occurrence, where `apply_hunks` refuses it.
#[test]
fn repeated_context_applies_at_its_first_match() {
    let original = "old\nkeep\nold\n";
    let body = "@@\n-old\n+new";
    assert_eq!(apply(original, body), "new\nkeep\nold\n");
    assert!(matches!(
        apply_hunks(Path::new("file.txt"), original, &update(body)),
        Err(PatchConflict::AmbiguousMatch { .. })
    ));
}

/// A chunk that only adds lines appends them, whatever its scope line (scenario 016).
#[test]
fn a_pure_addition_appends_to_the_end_of_the_file() {
    assert_eq!(
        apply("line1\nline2\n", "@@\n+added line 1\n+added line 2"),
        "line1\nline2\nadded line 1\nadded line 2\n"
    );
    assert_eq!(apply("first\nlast", "@@\n+added"), "first\nlast\nadded\n");
}

/// A section stays one contiguous chunk even though the parser splits it where context follows a
/// change; only a new `@@` starts a chunk searched for after the previous one.
#[test]
fn a_section_is_matched_as_one_contiguous_chunk() {
    let original = "a\nk\nz\nk\nb\n";
    // One section: `a`, `k`, `b` must be adjacent, and they are not.
    assert!(matches!(
        fails(original, "@@\n-a\n+A\n k\n-b\n+B"),
        PatchConflict::HunkFailed { hunk_index: 0, .. }
    ));
    // Two sections: the second is looked for after the first.
    assert_eq!(
        apply(original, "@@\n-a\n+A\n k\n@@\n k\n-b\n+B"),
        "A\nk\nz\nk\nB\n"
    );
}

/// A scope line narrows the search to what follows its first match.
#[test]
fn a_scope_line_is_found_before_its_chunk() {
    assert_eq!(
        apply(
            "fn first()\nold\nfn second()\nold\n",
            "@@ fn second()\n-old\n+new"
        ),
        "fn first()\nold\nfn second()\nnew\n"
    );
    assert!(matches!(
        fails("fn first()\nold\n", "@@ fn missing()\n-old\n+new"),
        PatchConflict::HunkFailed { hunk_index: 0, .. }
    ));
}

/// Line counts cover changed lines only; context lines stay in place.
#[test]
fn counts_changed_lines_but_not_context() {
    let update = apply_chunks(
        Path::new("file.txt"),
        "a\nb\nc\n",
        &update("@@\n a\n-b\n+B1\n+B2\n c"),
    )
    .expect("applies");
    assert_eq!(update.lines_removed(), 1);
    assert_eq!(update.lines_added(), 2);
}
