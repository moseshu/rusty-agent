//! Fuzzy matching for V4A hunk context.

use crate::parse::PatchMatchLevel;

/// Finds every occurrence of `pattern` from `search_start` at the strictest level that matches.
///
/// Returning all matches, rather than the first one, lets the product reject ambiguous edits
/// instead of silently changing an arbitrary repeated block.
pub(crate) fn find_candidates(
    lines: &[String],
    pattern: &[String],
    search_start: usize,
    end_of_file: bool,
) -> (PatchMatchLevel, Vec<usize>) {
    if pattern.is_empty() {
        let start = if end_of_file {
            lines.len()
        } else {
            search_start.min(lines.len())
        };
        return (PatchMatchLevel::Exact, vec![start]);
    }
    if pattern.len() > lines.len() {
        return (PatchMatchLevel::Exact, Vec::new());
    }

    let last_start = lines.len() - pattern.len();
    let candidates = if end_of_file {
        vec![last_start]
    } else if search_start > last_start {
        Vec::new()
    } else {
        (search_start..=last_start).collect()
    };

    for (level, matches) in [
        (PatchMatchLevel::Exact, exact as LineMatch),
        (PatchMatchLevel::TrimEnd, trim_end as LineMatch),
        (PatchMatchLevel::Trim, trim as LineMatch),
        (PatchMatchLevel::Normalized, normalized as LineMatch),
    ] {
        let found = candidates
            .iter()
            .copied()
            .filter(|start| {
                pattern
                    .iter()
                    .enumerate()
                    .all(|(offset, expected)| matches(&lines[start + offset], expected))
            })
            .collect::<Vec<_>>();
        if !found.is_empty() {
            return (level, found);
        }
    }

    (PatchMatchLevel::Exact, Vec::new())
}

/// Finds a V4A `@@ scope` anchor before a hunk's ordinary context search.
///
/// Patch headers often name a declaration without reproducing its full source line, such as
/// `@@ fn run()` for `fn run() {`. Exact matching remains preferred; a trimmed prefix is the
/// intentionally narrower fallback used only for this non-replacement anchor.
pub(crate) fn find_anchor_candidates(
    lines: &[String],
    anchor: &str,
    search_start: usize,
) -> (PatchMatchLevel, Vec<usize>) {
    let pattern = vec![anchor.to_owned()];
    let (level, candidates) = find_candidates(lines, &pattern, search_start, false);
    if !candidates.is_empty() {
        return (level, candidates);
    }
    let anchor = anchor.trim();
    let candidates = lines
        .iter()
        .enumerate()
        .skip(search_start)
        .filter_map(|(index, line)| line.trim_start().starts_with(anchor).then_some(index))
        .collect();
    (PatchMatchLevel::Trim, candidates)
}

type LineMatch = fn(&str, &str) -> bool;

fn exact(actual: &str, expected: &str) -> bool {
    actual == expected
}
fn trim_end(actual: &str, expected: &str) -> bool {
    actual.trim_end() == expected.trim_end()
}
fn trim(actual: &str, expected: &str) -> bool {
    actual.trim() == expected.trim()
}
fn normalized(actual: &str, expected: &str) -> bool {
    normalize(actual) == normalize(expected)
}

fn normalize(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| match character {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}
