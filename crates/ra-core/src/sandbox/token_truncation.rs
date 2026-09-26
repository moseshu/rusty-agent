//! Cutting command output down to a budget, keeping both ends.
//!
//! A port of the reference's `util/token_truncation.py`, itself a port of the truncation codex
//! applies to tool output. The budget is stated in tokens or in bytes, and a token is estimated as
//! four bytes of UTF-8 — no tokenizer is involved, so the answer is the same whatever model reads
//! it. What survives is a head and a tail split evenly around a marker saying how much was removed,
//! because the start of a command's output says what it was doing and the end says how it finished.
//!
//! Counting follows the reference exactly, down to the units: a character count is a count of
//! Unicode scalar values, as Python's `len` of a `str` is, and a line count is what Python's
//! `str.splitlines` would return. Both appear in text a model reads, so an approximation would be a
//! different output rather than an equivalent one.

/// How many bytes one token is taken to be.
pub const APPROX_BYTES_PER_TOKEN: usize = 4;

/// Which unit a truncation budget is stated in.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TruncationMode {
    /// A budget in bytes of UTF-8; the marker reports removed characters.
    Bytes,
    /// A budget in estimated tokens; the marker reports removed tokens.
    Tokens,
}

/// A budget, and the unit it is stated in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TruncationPolicy {
    mode: TruncationMode,
    limit: usize,
}

impl TruncationPolicy {
    /// The unit.
    #[must_use]
    pub const fn mode(&self) -> TruncationMode {
        self.mode
    }

    /// The budget in that unit. Never negative: the constructors clamp a negative limit to zero.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// A budget of `limit` bytes, where anything negative means none at all.
    #[must_use]
    pub fn bytes(limit: i64) -> Self {
        Self {
            mode: TruncationMode::Bytes,
            limit: clamp_limit(limit),
        }
    }

    /// A budget of `limit` estimated tokens, where anything negative means none at all.
    #[must_use]
    pub fn tokens(limit: i64) -> Self {
        Self {
            mode: TruncationMode::Tokens,
            limit: clamp_limit(limit),
        }
    }

    /// The budget expressed in tokens.
    #[must_use]
    pub fn token_budget(&self) -> usize {
        match self.mode {
            TruncationMode::Bytes => approx_tokens_from_byte_count(self.limit),
            TruncationMode::Tokens => self.limit,
        }
    }

    /// The budget expressed in bytes.
    #[must_use]
    pub fn byte_budget(&self) -> usize {
        match self.mode {
            TruncationMode::Bytes => self.limit,
            TruncationMode::Tokens => approx_bytes_for_tokens(to_i64(self.limit)),
        }
    }
}

fn clamp_limit(limit: i64) -> usize {
    usize::try_from(limit.max(0)).unwrap_or(usize::MAX)
}

fn to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Cuts `content` to the policy's budget, prefixing the line count when anything was cut.
///
/// Content within budget is returned as it is, without the prefix.
#[must_use]
pub fn formatted_truncate_text(content: &str, policy: TruncationPolicy) -> String {
    if content.len() <= policy.byte_budget() {
        return content.to_owned();
    }
    let prefix = format!("Total output lines: {}\n\n", python_line_count(content));
    match policy.mode {
        TruncationMode::Tokens => truncate_token_output(content, policy, &prefix),
        TruncationMode::Bytes => format!("{prefix}{}", truncate_text(content, policy)),
    }
}

/// Cuts `content` to the policy's budget, without a line-count prefix.
#[must_use]
pub fn truncate_text(content: &str, policy: TruncationPolicy) -> String {
    match policy.mode {
        TruncationMode::Bytes => truncate_with_byte_estimate(content, policy),
        TruncationMode::Tokens => truncate_with_token_budget(content, policy).0,
    }
}

/// Cuts `content` to `max_output_tokens`, and reports how many tokens it had when it was cut.
///
/// This is the form the shell tools and terminal sessions use: no budget, or content within it,
/// comes back unchanged with no count, so a count is present exactly when something was dropped.
/// The line-count prefix counts against the budget, and is left off when it would not fit.
#[must_use]
pub fn formatted_truncate_text_with_token_count(
    content: &str,
    max_output_tokens: Option<u64>,
) -> (String, Option<u64>) {
    let Some(max_output_tokens) = max_output_tokens else {
        return (content.to_owned(), None);
    };
    let policy = TruncationPolicy::tokens(i64::try_from(max_output_tokens).unwrap_or(i64::MAX));
    if content.len() <= policy.byte_budget() {
        return (content.to_owned(), None);
    }
    let prefix = format!("Total output lines: {}\n\n", python_line_count(content));
    let truncated = truncate_token_output(content, policy, &prefix);
    (truncated, Some(to_u64(approx_token_count(content))))
}

fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Cuts `content` to a token budget, and reports its original size when anything was cut.
#[must_use]
pub fn truncate_with_token_budget(
    content: &str,
    policy: TruncationPolicy,
) -> (String, Option<u64>) {
    if content.is_empty() {
        return (String::new(), None);
    }
    let max_tokens = policy.token_budget();
    if max_tokens > 0 && content.len() <= approx_bytes_for_tokens(to_i64(max_tokens)) {
        return (content.to_owned(), None);
    }
    let approx_total = approx_token_count(content);
    let truncated = truncate_token_output(content, policy, "");
    if truncated == content {
        return (truncated, None);
    }
    (truncated, Some(to_u64(approx_total)))
}

/// Keeps a head and a tail within the byte budget, the marker and `prefix` included.
///
/// The prefix is dropped rather than squeezed when it and the marker alone would not fit, and a
/// budget too small even for the marker returns as much of the marker as fits.
fn truncate_token_output(content: &str, policy: TruncationPolicy, prefix: &str) -> String {
    let max_bytes = policy.byte_budget();
    if max_bytes == 0 {
        return String::new();
    }

    let marker = format_truncation_marker(policy, approx_token_count(content));
    let prefix = if prefix.len() + marker.len() > max_bytes {
        ""
    } else {
        prefix
    };

    let Some(content_budget) = max_bytes.checked_sub(prefix.len() + marker.len()) else {
        return truncate_utf8(&marker, max_bytes).to_owned();
    };

    let (left_budget, right_budget) = split_budget(content_budget);
    let (_, left, right) = split_string(content, left_budget, right_budget);
    let removed_bytes = content.len() - left.len() - right.len();
    let removed_chars = content.chars().count() - left.chars().count() - right.chars().count();
    let marker = format_truncation_marker(
        policy,
        removed_units_for_source(policy, removed_bytes, removed_chars),
    );
    assemble_truncated_output(&format!("{prefix}{left}"), &right, &marker)
}

/// The longest leading part of `text` that fits in `max_bytes` without splitting a character.
fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Cuts `content` to a byte budget, marking how many characters were removed.
#[must_use]
pub fn truncate_with_byte_estimate(content: &str, policy: TruncationPolicy) -> String {
    if content.is_empty() {
        return String::new();
    }
    let total_chars = content.chars().count();
    let max_bytes = policy.byte_budget();

    if max_bytes == 0 {
        return format_truncation_marker(
            policy,
            removed_units_for_source(policy, content.len(), total_chars),
        );
    }
    if content.len() <= max_bytes {
        return content.to_owned();
    }

    let (left_budget, right_budget) = split_budget(max_bytes);
    let (removed_chars, left, right) = split_string(content, left_budget, right_budget);
    let marker = format_truncation_marker(
        policy,
        removed_units_for_source(policy, content.len() - max_bytes, removed_chars),
    );
    assemble_truncated_output(&left, &right, &marker)
}

/// Splits `content` into a head of at most `beginning_bytes` and a tail of at most `end_bytes`.
///
/// Neither side splits a character: a character that straddles a boundary goes to neither, and is
/// counted among the removed. Returns the removed character count, the head and the tail.
#[must_use]
pub fn split_string(
    content: &str,
    beginning_bytes: usize,
    end_bytes: usize,
) -> (usize, String, String) {
    if content.is_empty() {
        return (0, String::new(), String::new());
    }

    let length = content.len();
    let tail_start_target = length.saturating_sub(end_bytes);
    let mut prefix_end = 0;
    let mut suffix_start = length;
    let mut removed_chars = 0;
    let mut suffix_started = false;

    let mut byte_index = 0;
    for character in content.chars() {
        let char_end = byte_index + character.len_utf8();
        if char_end <= beginning_bytes {
            prefix_end = char_end;
            byte_index = char_end;
            continue;
        }
        if byte_index >= tail_start_target {
            if !suffix_started {
                suffix_start = byte_index;
                suffix_started = true;
            }
            byte_index = char_end;
            continue;
        }
        removed_chars += 1;
        byte_index = char_end;
    }

    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }

    (
        removed_chars,
        content[..prefix_end].to_owned(),
        content[suffix_start..].to_owned(),
    )
}

/// The text that stands in for what was removed.
#[must_use]
pub fn format_truncation_marker(policy: TruncationPolicy, removed_count: usize) -> String {
    match policy.mode {
        TruncationMode::Tokens => format!("…{removed_count} tokens truncated…"),
        TruncationMode::Bytes => format!("…{removed_count} chars truncated…"),
    }
}

/// Splits a budget into a head share and a tail share, the tail taking the odd byte.
#[must_use]
pub const fn split_budget(budget: usize) -> (usize, usize) {
    let left = budget / 2;
    (left, budget - left)
}

/// What the marker reports as removed: tokens estimated from bytes, or characters.
#[must_use]
pub fn removed_units_for_source(
    policy: TruncationPolicy,
    removed_bytes: usize,
    removed_chars: usize,
) -> usize {
    match policy.mode {
        TruncationMode::Tokens => approx_tokens_from_byte_count(removed_bytes),
        TruncationMode::Bytes => removed_chars,
    }
}

/// Joins the head, the marker and the tail.
#[must_use]
pub fn assemble_truncated_output(prefix: &str, suffix: &str, marker: &str) -> String {
    format!("{prefix}{marker}{suffix}")
}

/// The estimated token count of `text`, rounding up.
#[must_use]
pub const fn approx_token_count(text: &str) -> usize {
    approx_tokens_from_byte_count(text.len())
}

/// The bytes `tokens` tokens are estimated to take, where anything negative takes none.
#[must_use]
pub fn approx_bytes_for_tokens(tokens: i64) -> usize {
    clamp_limit(tokens).saturating_mul(APPROX_BYTES_PER_TOKEN)
}

/// The tokens `byte_count` bytes are estimated to be, rounding up.
#[must_use]
pub const fn approx_tokens_from_byte_count(byte_count: usize) -> usize {
    byte_count.div_ceil(APPROX_BYTES_PER_TOKEN)
}

/// How many lines Python's `str.splitlines` would split `text` into.
///
/// Every boundary it recognises counts, `\r\n` as one, and a trailing boundary does not start an
/// empty last line.
fn python_line_count(text: &str) -> usize {
    let mut lines = 0;
    let mut characters = text.chars().peekable();
    let mut line_has_content = false;
    while let Some(character) = characters.next() {
        let is_boundary = matches!(
            character,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if is_boundary {
            if character == '\r' && characters.peek() == Some(&'\n') {
                characters.next();
            }
            lines += 1;
            line_has_content = false;
        } else {
            line_has_content = true;
        }
    }
    if line_has_content {
        lines += 1;
    }
    lines
}
