use std::path::Path;

use ra_patch::{PatchAction, PatchConflict, PatchMatchLevel, apply_hunks, parse_patch};

#[test]
fn parses_all_v4a_operations_and_move_as_ordered_actions() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Add File: new.txt\n+created\n*** Update File: old.txt\n*** Move to: moved.txt\n@@\n-old\n+new\n*** Delete File: gone.txt\n*** End Patch\n",
    )
    .expect("a valid patch");
    assert_eq!(plan.actions().len(), 4);
    assert!(matches!(plan.actions()[0], PatchAction::AddFile { .. }));
    assert!(matches!(plan.actions()[1], PatchAction::UpdateFile { .. }));
    assert!(matches!(plan.actions()[2], PatchAction::MoveFile { .. }));
    assert!(matches!(plan.actions()[3], PatchAction::DeleteFile { .. }));
}

#[test]
fn parses_a_move_without_unnecessary_empty_update() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Update File: old.txt\n*** Move to: moved.txt\n*** End Patch\n",
    )
    .expect("a move-only update is valid V4A");
    assert_eq!(plan.actions().len(), 1);
    assert!(matches!(plan.actions()[0], PatchAction::MoveFile { .. }));
}

#[test]
fn parser_refuses_paths_that_escape_the_workspace() {
    let error = parse_patch("*** Begin Patch\n*** Add File: ../outside\n+x\n*** End Patch\n")
        .expect_err("parent paths must be rejected before filesystem access");
    assert!(error.to_string().contains("workspace root"));
}

#[test]
fn applies_fuzzy_context_without_rewriting_crlf() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Update File: file.txt\n@@\n-  old   \n+new\n*** End Patch\n",
    )
    .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    let result = apply_hunks(Path::new("file.txt"), "head\r\n  old\r\ntail\r\n", hunks)
        .expect("trim-end match");
    assert_eq!(result.match_level(), PatchMatchLevel::TrimEnd);
    assert_eq!(result.content(), "head\r\nnew\r\ntail\r\n");
}

#[test]
fn uses_the_optional_hunk_header_as_context() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Update File: file.txt\n@@ fn target()\n-old\n+new\n*** End Patch\n",
    )
    .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    let result = apply_hunks(Path::new("file.txt"), "fn target()\nold\n", hunks)
        .expect("header narrows the hunk");
    assert_eq!(result.content(), "fn target()\nnew\n");
}

#[test]
fn applies_the_standard_hunk_with_trailing_context() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Update File: file.txt\n@@\n a\n-b\n+B\n c\n*** End Patch\n",
    )
    .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    let result =
        apply_hunks(Path::new("file.txt"), "a\nb\nc\n", hunks).expect("trailing context applies");
    assert_eq!(result.content(), "a\nB\nc\n");
}

#[test]
fn splits_multiple_change_blocks_that_share_context() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Update File: file.txt\n@@\n a\n+x\n b\n+y\n c\n*** End Patch\n",
    )
    .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    assert_eq!(hunks.len(), 2);
    let result = apply_hunks(Path::new("file.txt"), "a\nb\nc\n", hunks).expect("both blocks apply");
    assert_eq!(result.content(), "a\nx\nb\ny\nc\n");
}

#[test]
fn appending_to_a_file_without_a_final_newline_keeps_lines_separate() {
    let plan =
        parse_patch("*** Begin Patch\n*** Update File: file.txt\n@@\n+added\n*** End Patch\n")
            .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    let result = apply_hunks(Path::new("file.txt"), "first\nlast", hunks).expect("append");
    assert_eq!(result.content(), "added\nfirst\nlast");

    let eof_plan = parse_patch(
        "*** Begin Patch\n*** Update File: file.txt\n@@\n+added\n*** End of File\n*** End Patch\n",
    )
    .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &eof_plan.actions()[0] else {
        panic!("update expected");
    };
    let result = apply_hunks(Path::new("file.txt"), "first\nlast", hunks).expect("eof append");
    assert_eq!(result.content(), "first\nlast\nadded");
}

#[test]
fn header_anchor_narrows_the_search_without_becoming_adjacent_context() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Update File: file.txt\n@@ fn target()\n-old\n+new\n*** End Patch\n",
    )
    .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    let result = apply_hunks(
        Path::new("file.txt"),
        "fn target() {\n    setup();\nold\n",
        hunks,
    )
    .expect("anchor search");
    assert_eq!(result.content(), "fn target() {\n    setup();\nnew\n");
}

#[test]
fn nested_header_anchors_narrow_the_search() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Update File: file.txt\n@@ class Foo\n@@     def bar(self):\n-        old\n+        new\n*** End Patch\n",
    )
    .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    let result = apply_hunks(
        Path::new("file.txt"),
        "class Foo:\n    def bar(self):\n        old\n",
        hunks,
    )
    .expect("nested anchors narrow the hunk");
    assert_eq!(
        result.content(),
        "class Foo:\n    def bar(self):\n        new\n"
    );
}

#[test]
fn repeated_header_anchor_is_disambiguated_by_hunk_context() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Update File: file.txt\n@@ fn new()\n-        old\n+        new\n*** End Patch\n",
    )
    .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    let result = apply_hunks(
        Path::new("file.txt"),
        "impl First {\n    fn new() {\n        other\n    }\n}\nimpl Second {\n    fn new() {\n        old\n    }\n}\n",
        hunks,
    )
    .expect("hunk context disambiguates repeated anchors");
    assert_eq!(
        result.content(),
        "impl First {\n    fn new() {\n        other\n    }\n}\nimpl Second {\n    fn new() {\n        new\n    }\n}\n"
    );
}

#[test]
fn replacing_the_last_line_preserves_a_missing_final_newline() {
    let plan = parse_patch(
        "*** Begin Patch\n*** Update File: file.txt\n@@\n-last\n+LAST\n*** End Patch\n",
    )
    .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    let result = apply_hunks(Path::new("file.txt"), "first\nlast", hunks).expect("replacement");
    assert_eq!(result.content(), "first\nLAST");
}

#[test]
fn parser_accepts_blank_lines_after_the_end_marker() {
    let plan = parse_patch("*** Begin Patch\n*** Add File: file.txt\n+x\n*** End Patch\n\n  \n")
        .expect("trailing blank lines are harmless");
    assert_eq!(plan.actions().len(), 1);
}

#[test]
fn refuses_an_ambiguous_context_instead_of_selecting_the_first_match() {
    let plan =
        parse_patch("*** Begin Patch\n*** Update File: file.txt\n@@\n-old\n+new\n*** End Patch\n")
            .expect("a valid patch");
    let PatchAction::UpdateFile { hunks, .. } = &plan.actions()[0] else {
        panic!("update expected");
    };
    let error = apply_hunks(Path::new("file.txt"), "old\nkeep\nold\n", hunks)
        .expect_err("multiple matches must be explicit");
    assert!(matches!(
        error,
        PatchConflict::AmbiguousMatch {
            candidate_count: 2,
            ..
        }
    ));
}
