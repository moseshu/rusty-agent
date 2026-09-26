//! The reference's V4A diff applier.
//!
//! A port of `openai-agents-python`'s `agents/apply_diff.py`, which applies the diff body of one
//! file operation to that file's text. The patch envelope — `*** Begin Patch`, the per-file headers
//! — is parsed by whoever calls this; what arrives here is the lines under one header.
//!
//! # Why this is not [`crate::apply_hunks`]
//!
//! Both read V4A, and they disagree about what a hunk means. [`crate::apply_hunks`] collects every
//! place a hunk's context matches and refuses an edit that matches more than one, adds a fourth
//! matching level that folds Unicode punctuation, anchors an end-of-file hunk only at the end, and
//! reports conflicts as values. The reference takes the first match at the strictest of three
//! levels, lets an end-of-file hunk fall back to a forward search, treats stacked `@@` headers as a
//! narrowing search that must match, and fails with a message. The sandbox tools port the
//! reference's behaviour, so they get the reference's algorithm; the coding tools keep theirs.
//!
//! # Porting notes
//!
//! - Whitespace-insensitive comparisons strip what Python's `str.strip` strips, which is Rust's
//!   whitespace plus the four ASCII separator controls `\x1c`–`\x1f`.
//! - The reference also totals a fuzz score while matching and never reads it; it is not kept.
//! - Messages are the reference's, verbatim: they reach the model through the patch tool, and a
//!   model corrects its patch by what they say.

use std::fmt;

/// Which of the two diff forms is being applied.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApplyDiffMode {
    /// An update: hunks of context, deletions and insertions against existing text.
    #[default]
    Default,
    /// A new file: every line is an insertion.
    Create,
}

/// Why a diff could not be applied.
///
/// The reference raises `ValueError` with the same message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyDiffError {
    message: String,
}

impl ApplyDiffError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// What was wrong, in the reference's words.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ApplyDiffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApplyDiffError {}

type DiffResult<T> = Result<T, ApplyDiffError>;

const END_PATCH: &str = "*** End Patch";
const END_FILE: &str = "*** End of File";
const SECTION_TERMINATORS: [&str; 4] = [
    END_PATCH,
    "*** Update File:",
    "*** Delete File:",
    "*** Add File:",
];
const END_SECTION_MARKERS: [&str; 5] = [
    END_PATCH,
    "*** Update File:",
    "*** Delete File:",
    "*** Add File:",
    END_FILE,
];

/// Applies a V4A diff to `input`.
///
/// In [`ApplyDiffMode::Create`] every line must start with `+` and `input` is not read. Otherwise the
/// diff is a sequence of hunks, each optionally introduced by one or more `@@` headers, located in
/// `input` by their context lines. The result keeps `input`'s newline style — CRLF if it has one —
/// and a created file takes the diff's.
///
/// # Errors
///
/// Returns [`ApplyDiffError`] for a line the format does not allow, a hunk whose context or stacked
/// headers cannot be found, and hunks that overlap.
pub fn apply_diff(input: &str, diff: &str, mode: ApplyDiffMode) -> DiffResult<String> {
    let newline = detect_newline(input, diff, mode);
    let diff_lines = normalize_diff_lines(diff);
    if mode == ApplyDiffMode::Create {
        return parse_create_diff(diff_lines, newline);
    }

    let normalized_input = input.replace("\r\n", "\n");
    let chunks = parse_update_diff(diff_lines, &normalized_input)?;
    apply_chunks(&normalized_input, &chunks, newline)
}

/// Whether Python's `str.isspace` holds for `character`.
fn is_py_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

fn py_strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}

fn py_rstrip(text: &str) -> &str {
    text.trim_end_matches(is_py_space)
}

/// Splits on LF or CRLF, drops trailing carriage returns, and drops one final empty line.
fn normalize_diff_lines(diff: &str) -> Vec<String> {
    let mut lines: Vec<String> = diff
        .split('\n')
        .map(|line| line.trim_end_matches('\r').to_owned())
        .collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn detect_newline_from_text(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// The newline the result is written with.
///
/// A created file has no input to take it from, so it uses the diff's.
fn detect_newline(input: &str, diff: &str, mode: ApplyDiffMode) -> &'static str {
    if mode != ApplyDiffMode::Create && input.contains('\n') {
        return detect_newline_from_text(input);
    }
    detect_newline_from_text(diff)
}

/// The diff's lines with the end-of-patch sentinel appended, and a position in them.
struct ParserState {
    lines: Vec<String>,
    index: usize,
}

impl ParserState {
    fn new(mut lines: Vec<String>) -> Self {
        lines.push(END_PATCH.to_owned());
        Self { lines, index: 0 }
    }

    fn current(&self) -> Option<&str> {
        self.lines.get(self.index).map(String::as_str)
    }

    fn is_done(&self, prefixes: &[&str]) -> bool {
        self.current()
            .is_none_or(|line| prefixes.iter().any(|prefix| line.starts_with(prefix)))
    }

    /// Consumes the current line when it starts with `prefix`, returning the rest of it.
    ///
    /// An absent prefix and a present one followed by nothing both return an empty string; the
    /// caller tells them apart by whether the position moved.
    fn read_str(&mut self, prefix: &str) -> String {
        let Some(rest) = self.current().and_then(|line| line.strip_prefix(prefix)) else {
            return String::new();
        };
        let rest = rest.to_owned();
        self.index += 1;
        rest
    }
}

fn parse_create_diff(lines: Vec<String>, newline: &str) -> DiffResult<String> {
    let mut parser = ParserState::new(lines);
    let mut output = Vec::new();
    while !parser.is_done(&SECTION_TERMINATORS) {
        let line = &parser.lines[parser.index];
        parser.index += 1;
        let Some(content) = line.strip_prefix('+') else {
            return Err(ApplyDiffError::new(format!(
                "Invalid Add File Line: {line}"
            )));
        };
        output.push(content.to_owned());
    }
    Ok(output.join(newline))
}

/// One replacement: at `orig_index`, remove `del_lines` and insert `ins_lines`.
#[derive(Debug, Clone)]
struct Chunk {
    orig_index: usize,
    del_lines: Vec<String>,
    ins_lines: Vec<String>,
}

fn parse_update_diff(lines: Vec<String>, input: &str) -> DiffResult<Vec<Chunk>> {
    let mut parser = ParserState::new(lines);
    let input_lines: Vec<&str> = input.split('\n').collect();
    let mut chunks = Vec::new();
    let mut cursor = 0;

    while !parser.is_done(&END_SECTION_MARKERS) {
        let (anchors, anchor_count) = read_anchors(&mut parser);

        // Only the first hunk may omit its `@@`.
        if anchor_count == 0 && cursor != 0 {
            let current_line = parser.current().unwrap_or("");
            return Err(ApplyDiffError::new(format!(
                "Invalid Line:\n{current_line}"
            )));
        }

        let require_anchor_match = anchor_count > 1;
        for (index, anchor) in anchors.iter().enumerate() {
            cursor = advance_cursor_to_anchor(
                anchor,
                &input_lines,
                cursor,
                require_anchor_match,
                index > 0,
            )?;
        }

        let section = read_section(&parser.lines, parser.index)?;
        let Some(new_index) =
            find_context(&input_lines, &section.next_context, cursor, section.eof)
        else {
            let context = section.next_context.join("\n");
            return Err(ApplyDiffError::new(if section.eof {
                format!("Invalid EOF Context {cursor}:\n{context}")
            } else {
                format!("Invalid Context {cursor}:\n{context}")
            }));
        };

        cursor = new_index + section.next_context.len();
        parser.index = section.end_index;
        chunks.extend(section.section_chunks.into_iter().map(|chunk| Chunk {
            orig_index: chunk.orig_index + new_index,
            ..chunk
        }));
    }

    Ok(chunks)
}

/// Consumes the `@@` headers that introduce one hunk.
///
/// A hunk may carry several stacked headers, so nested code can be located when one header plus
/// context is still ambiguous:
///
/// ```text
/// @@ class BaseClass
/// @@     def method():
/// ```
///
/// Returns the non-empty headers in the order they narrow the search, and how many header lines
/// were consumed, bare `@@` markers included.
fn read_anchors(parser: &mut ParserState) -> (Vec<String>, usize) {
    let mut anchors = Vec::new();
    let mut anchor_count = 0;
    loop {
        let start_index = parser.index;
        let anchor = parser.read_str("@@ ");
        let mut consumed = parser.index != start_index;
        if !consumed && parser.current() == Some("@@") {
            parser.index += 1;
            consumed = true;
        }
        if !consumed {
            break;
        }
        anchor_count += 1;
        if !py_strip(&anchor).is_empty() {
            anchors.push(anchor);
        }
    }
    (anchors, anchor_count)
}

/// Moves the cursor past the line an `@@` header names.
///
/// A header already matched somewhere before the cursor leaves it where it is, unless this is a
/// later header in a stack, which must be found after the one before it. An exact match is tried
/// before a whitespace-insensitive one. A header that matches nowhere is advisory on its own and an
/// error in a stack.
fn advance_cursor_to_anchor(
    anchor: &str,
    input_lines: &[&str],
    cursor: usize,
    require_match: bool,
    force_forward_search: bool,
) -> DiffResult<usize> {
    let before_cursor = &input_lines[..cursor.min(input_lines.len())];
    let search_forward = |matches: &dyn Fn(&str) -> bool| {
        (cursor..input_lines.len()).find(|&index| matches(input_lines[index]))
    };

    let exact = |line: &str| line == anchor;
    if !force_forward_search && before_cursor.iter().any(|line| exact(line)) {
        return Ok(cursor);
    }
    if let Some(index) = search_forward(&exact) {
        return Ok(index + 1);
    }

    let stripped_anchor = py_strip(anchor);
    let trimmed = |line: &str| py_strip(line) == stripped_anchor;
    if !force_forward_search && before_cursor.iter().any(|line| trimmed(line)) {
        return Ok(cursor);
    }
    if let Some(index) = search_forward(&trimmed) {
        return Ok(index + 1);
    }

    if require_match {
        return Err(ApplyDiffError::new(format!(
            "Invalid Anchor {cursor}:\n{anchor}"
        )));
    }
    Ok(cursor)
}

/// One hunk's lines: its context, the replacements inside it, and where the next hunk starts.
struct ReadSectionResult {
    next_context: Vec<String>,
    section_chunks: Vec<Chunk>,
    end_index: usize,
    eof: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineMode {
    Keep,
    Add,
    Delete,
}

fn read_section(lines: &[String], start_index: usize) -> DiffResult<ReadSectionResult> {
    let mut context: Vec<String> = Vec::new();
    let mut del_lines: Vec<String> = Vec::new();
    let mut ins_lines: Vec<String> = Vec::new();
    let mut section_chunks = Vec::new();
    let mut mode = LineMode::Keep;
    let mut index = start_index;

    while let Some(raw) = lines.get(index) {
        if raw.starts_with("@@")
            || raw.starts_with(END_PATCH)
            || raw.starts_with("*** Update File:")
            || raw.starts_with("*** Delete File:")
            || raw.starts_with("*** Add File:")
            || raw.starts_with(END_FILE)
            || raw == "***"
        {
            break;
        }
        if raw.starts_with("***") {
            return Err(ApplyDiffError::new(format!("Invalid Line: {raw}")));
        }

        index += 1;
        let last_mode = mode;
        // An empty line is an unchanged empty line whose leading space was lost.
        let line = if raw.is_empty() { " " } else { raw.as_str() };
        mode = match line.as_bytes()[0] {
            b'+' => LineMode::Add,
            b'-' => LineMode::Delete,
            b' ' => LineMode::Keep,
            _ => return Err(ApplyDiffError::new(format!("Invalid Line: {line}"))),
        };
        // The prefix is one ASCII byte, so this slices on a character boundary.
        let text = line[1..].to_owned();

        if mode == LineMode::Keep
            && last_mode != mode
            && (!del_lines.is_empty() || !ins_lines.is_empty())
        {
            section_chunks.push(Chunk {
                orig_index: context.len() - del_lines.len(),
                del_lines: std::mem::take(&mut del_lines),
                ins_lines: std::mem::take(&mut ins_lines),
            });
        }

        match mode {
            LineMode::Delete => {
                del_lines.push(text.clone());
                context.push(text);
            }
            LineMode::Add => ins_lines.push(text),
            LineMode::Keep => context.push(text),
        }
    }

    if !del_lines.is_empty() || !ins_lines.is_empty() {
        section_chunks.push(Chunk {
            orig_index: context.len() - del_lines.len(),
            del_lines,
            ins_lines,
        });
    }

    if lines.get(index).is_some_and(|line| line == END_FILE) {
        return Ok(ReadSectionResult {
            next_context: context,
            section_chunks,
            end_index: index + 1,
            eof: true,
        });
    }

    if index == start_index {
        let next_line = lines.get(index).map_or("", String::as_str);
        return Err(ApplyDiffError::new(format!(
            "Nothing in this section - index={index} {next_line}"
        )));
    }

    Ok(ReadSectionResult {
        next_context: context,
        section_chunks,
        end_index: index,
        eof: false,
    })
}

/// Where a hunk's context starts, searching from `start`.
///
/// An end-of-file hunk is tried at the end of the file first and then searched forward from
/// `start`. The empty element that splitting text ending in a newline leaves behind is the file's
/// terminator rather than a line, so such a hunk appends after the last real line.
fn find_context(lines: &[&str], context: &[String], start: usize, eof: bool) -> Option<usize> {
    if !eof {
        return find_context_core(lines, context, start);
    }
    let search_lines = match lines.split_last() {
        Some((&"", rest)) => rest,
        _ => lines,
    };
    let end_start = search_lines.len().saturating_sub(context.len());
    find_context_core(search_lines, context, end_start)
        .or_else(|| find_context_core(search_lines, context, start.min(search_lines.len())))
}

/// The first place `context` matches at or after `start`: exactly, then ignoring trailing
/// whitespace, then ignoring whitespace at both ends.
fn find_context_core(lines: &[&str], context: &[String], start: usize) -> Option<usize> {
    if context.is_empty() {
        return Some(start);
    }
    let levels: [fn(&str) -> &str; 3] = [|value| value, py_rstrip, py_strip];
    levels.iter().find_map(|map| {
        (start..lines.len()).find(|&index| equals_slice(lines, context, index, *map))
    })
}

fn equals_slice(source: &[&str], target: &[String], start: usize, map: fn(&str) -> &str) -> bool {
    let Some(window) = source.get(start..start + target.len()) else {
        return false;
    };
    window
        .iter()
        .zip(target)
        .all(|(source, target)| map(source) == map(target))
}

fn apply_chunks(input: &str, chunks: &[Chunk], newline: &str) -> DiffResult<String> {
    let orig_lines: Vec<&str> = input.split('\n').collect();
    let mut dest_lines: Vec<&str> = Vec::new();
    let mut cursor = 0;

    for chunk in chunks {
        if chunk.orig_index > orig_lines.len() {
            return Err(ApplyDiffError::new(format!(
                "applyDiff: chunk.origIndex {} > input length {}",
                chunk.orig_index,
                orig_lines.len()
            )));
        }
        if cursor > chunk.orig_index {
            return Err(ApplyDiffError::new(format!(
                "applyDiff: overlapping chunk at {} (cursor {cursor})",
                chunk.orig_index
            )));
        }

        dest_lines.extend(&orig_lines[cursor..chunk.orig_index]);
        dest_lines.extend(chunk.ins_lines.iter().map(String::as_str));
        cursor = chunk.orig_index + chunk.del_lines.len();
    }

    dest_lines.extend(&orig_lines[cursor.min(orig_lines.len())..]);
    Ok(dest_lines.join(newline))
}
