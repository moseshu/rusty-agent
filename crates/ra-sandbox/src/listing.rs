//! Reading a directory listing back out of `ls -la`.
//!
//! A session that cannot call `stat` itself — because the filesystem is somewhere else, or because
//! the listing has to happen as another account — asks the sandbox to run `ls -la` and parses what
//! comes back. That is the reference's approach, and the parsing has to cope with the two `ls`
//! implementations a sandbox image might carry.

use ra_core::sandbox::{EntryKind, FileEntry, Permissions};

/// Reads the output of `ls -la` into entries.
///
/// Lines that are not entries — the `total` header, blanks, anything too short to be a row — are
/// skipped rather than reported: this is output from a command, not a wire format, and a parser
/// that failed on an unfamiliar line would break a listing over a locale banner. `.` and `..` are
/// dropped because they are not contents.
///
/// `base` is the directory that was listed, and relative names are re-attached to it so every entry
/// carries a path a caller can use.
#[must_use]
pub fn parse_ls_la(output: &str, base: &str) -> Vec<FileEntry> {
    output
        .lines()
        .filter_map(|line| parse_entry(line, base))
        .collect()
}

/// Reads one row, or decides it is not one.
fn parse_entry(line: &str, base: &str) -> Option<FileEntry> {
    if line.is_empty() || line.starts_with("total") {
        return None;
    }
    let parts = split_whitespace(line, 8);
    if parts.len() < 9 {
        return None;
    }

    let raw_mode = parts[0];
    let owner = parts[2];
    let group = parts[3];
    let kind = match raw_mode.chars().next() {
        Some('d') => EntryKind::Directory,
        Some('-') => EntryKind::File,
        Some('l') => EntryKind::Symlink,
        _ => EntryKind::Other,
    };

    // A device reports an identifier where a file reports a size, in one of two spellings: GNU
    // prints `major, minor` and so shifts every later column by one, BSD prints a single hex word.
    // `stat` calls a device zero bytes, and so does this.
    let (size, name) = if matches!(raw_mode.chars().next(), Some('c' | 'b')) {
        if parts[4].ends_with(',') {
            let shifted = split_whitespace(line, 9);
            if shifted.len() < 10 {
                return None;
            }
            (0, shifted[9].to_owned())
        } else {
            (0, parts[8].to_owned())
        }
    } else {
        (parts[4].parse::<u64>().ok()?, parts[8].to_owned())
    };

    // Only the rwx bits and directory-ness are modelled, so a symlink or a device has its type
    // marker normalized away rather than failing to parse.
    let mode = match raw_mode.chars().next() {
        Some('d' | '-') => raw_mode.to_owned(),
        Some(_) if raw_mode.len() >= 2 => format!("-{}", &raw_mode[1..]),
        _ => raw_mode.to_owned(),
    };

    let name = if kind == EntryKind::Symlink {
        name.split_once(" -> ")
            .map_or(name.clone(), |(name, _)| name.to_owned())
    } else {
        name
    };
    if name == "." || name == ".." {
        return None;
    }

    let path = if name.starts_with('/') {
        name
    } else if base == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", base.trim_end_matches('/'))
    };

    Some(
        FileEntry::new(path, Permissions::from_str_mode(&mode).ok()?)
            .with_ownership(owner, group)
            .with_size(size)
            .with_kind(kind),
    )
}

/// Splits on runs of whitespace, keeping the remainder whole after `limit` splits.
///
/// The port of `str.split(maxsplit=...)`: a filename can contain spaces, so the last column has to
/// survive as one field rather than being split into several.
fn split_whitespace(line: &str, limit: usize) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::with_capacity(limit + 1);
    let mut rest = line.trim_start();
    while parts.len() < limit {
        match rest.find(char::is_whitespace) {
            Some(end) => {
                parts.push(&rest[..end]);
                rest = rest[end..].trim_start();
            }
            None => break,
        }
    }
    if !rest.is_empty() {
        parts.push(rest);
    }
    parts
}
