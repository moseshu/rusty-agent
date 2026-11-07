// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Update application with Codex's `apply_patch` semantics.
//!
//! Codex groups a file update into chunks, one per `@@` section, and locates each chunk's old
//! lines with `seek_sequence`: the first position that matches at the strictest level that
//! matches at all. A section whose lines are found twice applies at the first; a chunk that only
//! adds lines appends them to the end of the file; every line of the result ends with a line
//! ending. [`crate::apply_hunks`] is a stricter, separate choice that refuses ambiguous context.
//!
//! [`crate::parse_patch`] splits one section into several [`PatchHunk`]s wherever context
//! follows a change, so the chunks are rebuilt here from the hunks that continue a section.

use std::path::Path;

use crate::{AppliedFileUpdate, PatchConflict, PatchHunk, PatchMatchLevel};

/// Applies update hunks the way Codex's `apply_patch` does, without accessing the filesystem.
///
/// Each `@@` section is matched in turn, after the previous one, at its first match; unchanged
/// lines keep their own line endings, inserted lines take the file's first line ending (LF for a
/// file without one), and every line of the result ends with a line ending.
///
/// # Errors
///
/// [`PatchConflict::HunkFailed`] names the section, counted from zero, whose scope line or old
/// lines were not found at any matching level.
pub fn apply_chunks(
    path: &Path,
    original: &str,
    hunks: &[PatchHunk],
) -> Result<AppliedFileUpdate, PatchConflict> {
    let mut source = SourceFile::parse(original);
    let lines = source.line_texts();
    let chunks = chunks(hunks);
    let (replacements, match_level) = compute_replacements(&lines, path, &chunks)?;
    let lines_removed = replacements.iter().map(|(_, old_len, _)| old_len).sum();
    let lines_added = replacements.iter().map(|(_, _, new)| new.len()).sum();
    source.apply_replacements(&replacements);
    Ok(AppliedFileUpdate::new(
        source.into_contents(),
        match_level,
        lines_added,
        lines_removed,
    ))
}

/// One `@@` section: Codex's `UpdateFileChunk`.
#[derive(Debug, Default)]
struct Chunk {
    /// The scope lines of the section's header, searched for before its old lines.
    change_context: Vec<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    /// Pairs of indices into `old_lines` and `new_lines` of the lines parsed as context.
    context_line_indices: Vec<(usize, usize)>,
    is_end_of_file: bool,
}

impl Chunk {
    fn push_context_line(&mut self, line: &str) {
        self.context_line_indices
            .push((self.old_lines.len(), self.new_lines.len()));
        self.old_lines.push(line.to_owned());
        self.new_lines.push(line.to_owned());
    }
}

/// Rebuilds the sections the parser split into hunks. A hunk that continues its section starts
/// with the context the previous hunk ended with, so that context is taken once.
fn chunks(hunks: &[PatchHunk]) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    for hunk in hunks {
        let continues = hunk.continues_previous() && !chunks.is_empty();
        if !continues {
            let mut chunk = Chunk {
                change_context: hunk.context_anchors().to_vec(),
                ..Chunk::default()
            };
            for line in hunk.context_before() {
                chunk.push_context_line(line);
            }
            chunks.push(chunk);
        }
        let Some(chunk) = chunks.last_mut() else {
            continue;
        };
        chunk.old_lines.extend(hunk.removed_lines().iter().cloned());
        chunk.new_lines.extend(hunk.added_lines().iter().cloned());
        for line in hunk.context_after() {
            chunk.push_context_line(line);
        }
        chunk.is_end_of_file = hunk.end_of_file();
    }
    chunks
}

type Replacement = (usize, usize, Vec<String>);

/// Codex's `compute_replacements`: where each chunk's old lines are, and what replaces them.
fn compute_replacements(
    original_lines: &[String],
    path: &Path,
    chunks: &[Chunk],
) -> Result<(Vec<Replacement>, PatchMatchLevel), PatchConflict> {
    let failed = |hunk_index| PatchConflict::HunkFailed {
        path: path.to_path_buf(),
        hunk_index,
        level: PatchMatchLevel::Normalized,
    };
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index = 0;
    let mut match_level = PatchMatchLevel::Exact;

    for (chunk_index, chunk) in chunks.iter().enumerate() {
        // Codex's grammar has one scope line per section; nested scopes narrow in order.
        for context in &chunk.change_context {
            let (index, level) = seek_sequence(
                original_lines,
                std::slice::from_ref(context),
                line_index,
                false,
            )
            .ok_or_else(|| failed(chunk_index))?;
            match_level = match_level.max(level);
            line_index = index + 1;
        }

        if chunk.old_lines.is_empty() {
            replacements.push((original_lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }

        // The last old line is often an empty string standing for the file's final newline, which
        // the source lines do not carry as an element of their own; retry without it.
        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }
        let (start_idx, level) = found.ok_or_else(|| failed(chunk_index))?;
        match_level = match_level.max(level);

        // Context lines occur on both sides; keep the original ones in place so their exact
        // contents and terminators survive.
        let mut old_start = 0;
        let mut new_start = 0;
        for &(old_context, new_context) in &chunk.context_line_indices {
            if old_context >= pattern.len() || new_context >= new_slice.len() {
                break;
            }
            if old_start != old_context || new_start != new_context {
                replacements.push((
                    start_idx + old_start,
                    old_context - old_start,
                    new_slice[new_start..new_context].to_vec(),
                ));
            }
            old_start = old_context + 1;
            new_start = new_context + 1;
        }
        if old_start != pattern.len() || new_start != new_slice.len() {
            replacements.push((
                start_idx + old_start,
                pattern.len() - old_start,
                new_slice[new_start..].to_vec(),
            ));
        }
        line_index = start_idx + pattern.len();
    }

    replacements.sort_by_key(|(index, _, _)| *index);
    Ok((replacements, match_level))
}

/// Codex's `seek_sequence`: the first index from `start` where `pattern` matches `lines`, trying
/// exact, trailing-whitespace-insensitive, whitespace-insensitive and punctuation-normalized
/// comparison in turn, with the level it matched at. An end-of-file pattern is searched for at
/// the end of the file first.
fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<(usize, PatchMatchLevel)> {
    if pattern.is_empty() {
        return Some((start, PatchMatchLevel::Exact));
    }
    // A pattern longer than the input cannot match, and the scans below would slice out of range.
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof && lines.len() >= pattern.len() {
        (lines.len() - pattern.len()).max(start)
    } else {
        start
    };
    let last = lines.len().saturating_sub(pattern.len());
    for (level, matches) in [
        (PatchMatchLevel::Exact, exact as LineMatch),
        (PatchMatchLevel::TrimEnd, trim_end as LineMatch),
        (PatchMatchLevel::Trim, trim as LineMatch),
        (PatchMatchLevel::Normalized, normalized as LineMatch),
    ] {
        for index in search_start..=last {
            if pattern
                .iter()
                .enumerate()
                .all(|(offset, expected)| matches(&lines[index + offset], expected))
            {
                return Some((index, level));
            }
        }
    }
    None
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
    normalise(actual) == normalise(expected)
}

/// Maps typographic dashes, quotes and spaces to their ASCII forms, after trimming.
fn normalise(value: &str) -> String {
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

#[derive(Clone, Copy)]
enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

struct SourceLine {
    text: String,
    ending: Option<LineEnding>,
}

/// Codex's `SourceFile`: the lines of a file with the ending each one had.
struct SourceFile {
    lines: Vec<SourceLine>,
    preferred_ending: LineEnding,
}

impl SourceFile {
    /// Splits contents into lines at LF, CRLF and lone CR, keeping each ending. The first ending
    /// becomes the style of inserted lines; a file with none uses LF.
    fn parse(contents: &str) -> Self {
        let bytes = contents.as_bytes();
        let mut lines = Vec::new();
        let mut preferred_ending = None;
        let mut line_start = 0;
        let mut cursor = 0;
        while cursor < bytes.len() {
            let (ending, ending_len) = match bytes[cursor] {
                b'\r' if bytes.get(cursor + 1) == Some(&b'\n') => (LineEnding::CrLf, 2),
                b'\r' => (LineEnding::Cr, 1),
                b'\n' => (LineEnding::Lf, 1),
                _ => {
                    cursor += 1;
                    continue;
                }
            };
            preferred_ending.get_or_insert(ending);
            lines.push(SourceLine {
                text: contents[line_start..cursor].to_owned(),
                ending: Some(ending),
            });
            cursor += ending_len;
            line_start = cursor;
        }
        if line_start < contents.len() {
            lines.push(SourceLine {
                text: contents[line_start..].to_owned(),
                ending: None,
            });
        }
        Self {
            lines,
            preferred_ending: preferred_ending.unwrap_or(LineEnding::Lf),
        }
    }

    fn line_texts(&self) -> Vec<String> {
        self.lines.iter().map(|line| line.text.clone()).collect()
    }

    /// Rebuilds the file from source-ordered, non-overlapping replacements. Unchanged lines keep
    /// their endings, inserted lines use the preferred one, and every line ends up with an ending,
    /// as Codex's updates have always added a final newline.
    fn apply_replacements(&mut self, replacements: &[Replacement]) {
        let mut source_lines = std::mem::take(&mut self.lines).into_iter();
        let mut new_lines = Vec::new();
        let mut source_index = 0;
        for (start_idx, old_len, new_segment) in replacements {
            for line in source_lines
                .by_ref()
                .take(start_idx.saturating_sub(source_index))
            {
                new_lines.push(line);
            }
            for _ in source_lines.by_ref().take(*old_len) {}
            new_lines.extend(new_segment.iter().map(|text| SourceLine {
                text: text.clone(),
                ending: Some(self.preferred_ending),
            }));
            source_index = start_idx + old_len;
        }
        new_lines.extend(source_lines);
        self.lines = new_lines;
        for line in &mut self.lines {
            line.ending.get_or_insert(self.preferred_ending);
        }
    }

    fn into_contents(self) -> String {
        let mut contents = String::new();
        for line in self.lines {
            contents.push_str(&line.text);
            if let Some(ending) = line.ending {
                contents.push_str(ending.as_str());
            }
        }
        contents
    }
}
