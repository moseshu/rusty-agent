//! Quoting an argument so a shell reads it back as one word.
//!
//! The reference quotes with `shlex.quote` in two places this workspace ports: a backend joining an
//! argument vector before handing it to `sh -c`, and the shell tool prefixing a `cd` into the
//! working directory. The backend and the tool live in crates that do not depend on each other, so
//! the one port of the function lives here.

/// The characters that survive a shell unquoted.
///
/// The reference's allowlist, character for character. Deliberately narrow: `~` and `*` are absent
/// because a shell would expand them, and anything outside ASCII is quoted rather than guessed at.
const SAFE: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_@%+=:,./-";

/// Renders one argument so a POSIX shell reads it back unchanged.
///
/// An empty argument becomes `''`, because nothing at all is not an argument. Anything containing a
/// character outside the safe set is wrapped in single quotes, and an embedded single quote is
/// spelled `'"'"'` — leave the quoted run, emit a literal quote, start a new run.
#[must_use]
pub fn quote(argument: &str) -> String {
    if argument.is_empty() {
        return "''".to_owned();
    }
    if argument.bytes().all(|byte| SAFE.contains(&byte)) {
        return argument.to_owned();
    }
    format!("'{}'", argument.replace('\'', "'\"'\"'"))
}

/// Renders an argument vector as a command line a shell splits back into those arguments.
pub fn join<'a>(arguments: impl IntoIterator<Item = &'a str>) -> String {
    arguments
        .into_iter()
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ")
}
