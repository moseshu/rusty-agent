//! Accepting chosen public-surface changes into a baseline, and leaving the rest reported.

use xtask::baseline_policy::{accept, unused_fragments};

fn lines(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn only_named_additions_are_accepted() {
    let baseline = lines(&["fn a()"]);
    let current = lines(&["fn a()", "fn reviewed()", "fn unreviewed()"]);

    let accepted = accept(&baseline, &current, &["reviewed()"]);

    // `unreviewed()` contains `reviewed()` too: fragments are substrings, and the caller names
    // them precisely enough.
    assert_eq!(accepted.added, lines(&["fn reviewed()", "fn unreviewed()"]));
    let precise = accept(&baseline, &current, &["fn reviewed()"]);
    assert_eq!(precise.added, lines(&["fn reviewed()"]));
    assert_eq!(precise.baseline, lines(&["fn a()", "fn reviewed()"]));
    assert!(precise.removed.is_empty());
}

#[test]
fn a_named_signature_change_replaces_the_old_line_and_an_unnamed_removal_stays_reported() {
    let baseline = lines(&["const fn with_ts(self)", "fn gone()", "fn kept()"]);
    let current = lines(&["fn kept()", "fn with_ts(self)"]);

    let accepted = accept(&baseline, &current, &["with_ts"]);

    assert_eq!(accepted.added, lines(&["fn with_ts(self)"]));
    assert_eq!(accepted.removed, lines(&["const fn with_ts(self)"]));
    // `gone()` was not named, so it stays in the baseline and the gate keeps reporting it removed.
    assert_eq!(
        accepted.baseline,
        lines(&["fn kept()", "fn with_ts(self)", "fn gone()"])
    );
}

#[test]
fn a_fragment_that_changed_nothing_is_reported() {
    let changed = lines(&["fn reviewed()"]);
    assert_eq!(
        unused_fragments(&["reviewed", "reveiwed"], &changed),
        ["reveiwed"]
    );
}
