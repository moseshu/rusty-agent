//! Product-reference policy used by the layering gate.
//!
//! The policy deliberately uses a strict textual match for product identifiers and aliases. A
//! provider-neutral protocol term may be exempted only through a product-specific trailing line
//! comment. Crate identifiers are never exemptible.

/// Prefix of a product-alias exemption in a trailing line comment.
pub const ALLOW_MARKER: &str = "layering-allow:";

/// One product-policy violation at a source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyViolation {
    line: usize,
    message: String,
}

impl PolicyViolation {
    /// One-based source line.
    #[must_use]
    pub const fn line(&self) -> usize {
        self.line
    }

    /// Human-readable violation detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Result of scanning one Rust source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductScan {
    violations: Vec<PolicyViolation>,
    exemptions: usize,
}

impl ProductScan {
    /// Violations found in the source.
    #[must_use]
    pub fn violations(&self) -> &[PolicyViolation] {
        &self.violations
    }

    /// Number of valid exemptions that suppressed an actual alias collision.
    #[must_use]
    pub const fn exemptions(&self) -> usize {
        self.exemptions
    }
}

/// Scans Rust source for forbidden product references and product-name branches.
///
/// An exemption has the form `// layering-allow: assistant = model role, not a product`. It must
/// be a real trailing line comment, name exactly one known bare product alias, include a non-empty
/// reason, and suppress an alias literal on the same line. Unused, malformed, or unknown markers
/// are violations. Product crate identifiers such as `ra_assistant` are never exemptible.
#[must_use]
pub fn scan_product_references(source: &str, products: &[&str]) -> ProductScan {
    let mut violations = Vec::new();
    let mut exemptions = 0;

    for (index, line) in lex_lines(source).into_iter().enumerate() {
        let line_number = index + 1;
        let allowance = parse_allowance(line.comment.as_deref(), products);

        let mut aliases_on_line = Vec::new();
        for &product in products {
            let snake = product.replace('-', "_");
            if line.code.contains(&snake) {
                violations.push(PolicyViolation {
                    line: line_number,
                    message: format!(
                        "product crate identifier `{snake}` is forbidden and cannot be exempted"
                    ),
                });
            }

            let alias = product.trim_start_matches("ra-");
            let quoted = format!("\"{alias}\"");
            if line.code.contains(&quoted) {
                aliases_on_line.push((alias, product));
            }
        }

        match allowance {
            Allowance::None => {
                for (_, product) in aliases_on_line {
                    violations.push(forbidden_alias(line_number, product));
                }
            }
            Allowance::Invalid(message) => {
                violations.push(PolicyViolation {
                    line: line_number,
                    message,
                });
                for (_, product) in aliases_on_line {
                    violations.push(forbidden_alias(line_number, product));
                }
            }
            Allowance::Valid { alias } => {
                let mut consumed = false;
                for (matched_alias, product) in aliases_on_line {
                    if matched_alias == alias {
                        consumed = true;
                    } else {
                        violations.push(forbidden_alias(line_number, product));
                    }
                }
                if consumed {
                    exemptions += 1;
                } else {
                    violations.push(PolicyViolation {
                        line: line_number,
                        message: format!(
                            "unused `{ALLOW_MARKER}` exemption for `{alias}`; no matching alias literal exists on this line"
                        ),
                    });
                }
            }
        }
    }

    ProductScan {
        violations,
        exemptions,
    }
}

fn forbidden_alias(line: usize, product: &str) -> PolicyViolation {
    PolicyViolation {
        line,
        message: format!(
            "framework source contains product alias `{product}`; use a product-specific trailing exemption only for a genuine protocol-term collision"
        ),
    }
}

enum Allowance<'a> {
    None,
    Valid { alias: &'a str },
    Invalid(String),
}

fn parse_allowance<'a>(comment: Option<&'a str>, products: &[&str]) -> Allowance<'a> {
    let Some(comment) = comment else {
        return Allowance::None;
    };
    let trimmed = comment.trim();
    if !trimmed.starts_with(ALLOW_MARKER) {
        return Allowance::None;
    }

    let body = trimmed[ALLOW_MARKER.len()..].trim();
    let Some((alias, reason)) = body.split_once('=') else {
        return Allowance::Invalid(format!(
            "malformed `{ALLOW_MARKER}` marker; expected `// {ALLOW_MARKER} <alias> = <reason>`"
        ));
    };
    let alias = alias.trim();
    let reason = reason.trim();
    if alias.is_empty() || reason.is_empty() {
        return Allowance::Invalid(format!(
            "malformed `{ALLOW_MARKER}` marker; alias and reason must both be non-empty"
        ));
    }
    if !products
        .iter()
        .map(|product| product.trim_start_matches("ra-"))
        .any(|known| known == alias)
    {
        return Allowance::Invalid(format!(
            "unknown product alias `{alias}` in `{ALLOW_MARKER}` marker"
        ));
    }

    Allowance::Valid { alias }
}

#[derive(Default)]
struct LexedLine {
    code: String,
    comment: Option<String>,
}

#[derive(Clone, Copy)]
enum LexState {
    Code,
    String { escaped: bool },
    RawString { hashes: usize },
    BlockComment { depth: usize },
}

fn lex_lines(source: &str) -> Vec<LexedLine> {
    let bytes = source.as_bytes();
    let mut lines = Vec::new();
    let mut line = LexedLine::default();
    let mut state = LexState::Code;
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'\n' {
            lines.push(line);
            line = LexedLine::default();
            if matches!(state, LexState::String { .. }) {
                state = LexState::String { escaped: false };
            }
            index += 1;
            continue;
        }

        match state {
            LexState::Code => {
                if starts_with(bytes, index, b"//") {
                    let end = bytes[index..]
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .map_or(bytes.len(), |offset| index + offset);
                    line.comment = Some(String::from_utf8_lossy(&bytes[index + 2..end]).into());
                    index = end;
                } else if starts_with(bytes, index, b"/*") {
                    line.code.push(' ');
                    state = LexState::BlockComment { depth: 1 };
                    index += 2;
                } else if let Some((length, hashes)) = raw_string_start(bytes, index) {
                    push_bytes(&mut line.code, &bytes[index..index + length]);
                    state = LexState::RawString { hashes };
                    index += length;
                } else if let Some(length) = char_literal_len(bytes, index) {
                    push_bytes(&mut line.code, &bytes[index..index + length]);
                    index += length;
                } else if bytes[index] == b'"' {
                    line.code.push('"');
                    state = LexState::String { escaped: false };
                    index += 1;
                } else {
                    line.code.push(char::from(bytes[index]));
                    index += 1;
                }
            }
            LexState::String { escaped } => {
                line.code.push(char::from(bytes[index]));
                if escaped {
                    state = LexState::String { escaped: false };
                } else if bytes[index] == b'\\' {
                    state = LexState::String { escaped: true };
                } else if bytes[index] == b'"' {
                    state = LexState::Code;
                }
                index += 1;
            }
            LexState::RawString { hashes } => {
                if raw_string_end(bytes, index, hashes) {
                    let length = hashes + 1;
                    push_bytes(&mut line.code, &bytes[index..index + length]);
                    state = LexState::Code;
                    index += length;
                } else {
                    line.code.push(char::from(bytes[index]));
                    index += 1;
                }
            }
            LexState::BlockComment { depth } => {
                if starts_with(bytes, index, b"/*") {
                    state = LexState::BlockComment { depth: depth + 1 };
                    index += 2;
                } else if starts_with(bytes, index, b"*/") {
                    state = if depth == 1 {
                        LexState::Code
                    } else {
                        LexState::BlockComment { depth: depth - 1 }
                    };
                    index += 2;
                } else {
                    index += 1;
                }
            }
        }
    }

    if !line.code.is_empty() || line.comment.is_some() || source.ends_with('\n') {
        lines.push(line);
    }
    lines
}

fn starts_with(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    bytes.get(index..index + needle.len()) == Some(needle)
}

fn raw_string_start(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let mut cursor = index;
    if bytes.get(cursor) == Some(&b'b') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'r') {
        return None;
    }
    cursor += 1;
    let hash_start = cursor;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'"') {
        return None;
    }
    let hashes = cursor - hash_start;
    Some((cursor - index + 1, hashes))
}

fn raw_string_end(bytes: &[u8], index: usize, hashes: usize) -> bool {
    bytes.get(index) == Some(&b'"')
        && bytes
            .get(index + 1..index + 1 + hashes)
            .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
}

fn char_literal_len(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index) != Some(&b'\'') {
        return None;
    }

    let mut cursor = index + 1;
    match *bytes.get(cursor)? {
        b'\\' => {
            cursor += 1;
            cursor += match *bytes.get(cursor)? {
                b'x' => {
                    if !bytes.get(cursor + 1)?.is_ascii_hexdigit()
                        || !bytes.get(cursor + 2)?.is_ascii_hexdigit()
                    {
                        return None;
                    }
                    3
                }
                b'u' => unicode_escape_len(bytes, cursor)?,
                b'\\' | b'\'' | b'"' | b'n' | b'r' | b't' | b'0' => 1,
                _ => return None,
            };
        }
        b'\'' | b'\n' | b'\r' | b'\t' => return None,
        byte if byte.is_ascii() => cursor += 1,
        _ => {
            let character = std::str::from_utf8(&bytes[cursor..]).ok()?.chars().next()?;
            cursor += character.len_utf8();
        }
    }

    (bytes.get(cursor) == Some(&b'\'')).then_some(cursor - index + 1)
}

fn unicode_escape_len(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index..index + 2) != Some(b"u{") {
        return None;
    }

    let mut cursor = index + 2;
    let mut digits = 0;
    while let Some(&byte) = bytes.get(cursor) {
        match byte {
            b'}' if digits > 0 => return Some(cursor - index + 1),
            b'_' => cursor += 1,
            byte if byte.is_ascii_hexdigit() => {
                digits += 1;
                cursor += 1;
            }
            _ => return None,
        }
    }
    None
}

fn push_bytes(target: &mut String, bytes: &[u8]) {
    target.push_str(&String::from_utf8_lossy(bytes));
}
