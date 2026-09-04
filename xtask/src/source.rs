//! Shared helpers for walking the source tree.

use std::path::{Path, PathBuf};

/// The workspace root.
///
/// Derived from the compile-time `CARGO_MANIFEST_DIR` rather than the current directory: a gate
/// should inspect the same code no matter where it was invoked.
pub(crate) fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest).to_path_buf()
}

/// Whether this checkout holds the baseline directory the reconciling gates read.
///
/// `api/` is deliberately out of version control, so a fresh clone has nothing for `public-api`,
/// `schema-stability`, and `prompt-dump` to compare against — they skip and name the command that
/// would produce it. **Present but incomplete is a different thing entirely**: a crate or an
/// artifact somebody added without blessing, which is exactly what those gates are for, so that
/// stays a failure.
pub(crate) fn baselines_present() -> bool {
    workspace_root().join("api").is_dir()
}

/// A path relative to the workspace root, for error messages.
pub(crate) fn relative(path: &Path) -> String {
    path.strip_prefix(workspace_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Recursively collects the `.rs` files under a directory. Returns empty when it does not exist.
///
/// **`target/` is skipped**: build output contains build-script-generated `.rs` files
/// (`tests/target/` alone is 4 GB), which are neither source nor anything a gate should scan as
/// source.
pub(crate) fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect(dir, &mut files);
    files.sort();
    files
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every crate name under `crates/`, in lexical order.
pub(crate) fn crate_names() -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(workspace_root().join("crates")) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join("Cargo.toml").is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names
}

/// The library crate names under `crates/` (those with `src/lib.rs`), in lexical order.
///
/// The gates keep no crate list — the filesystem is the list. Create a library crate and the test
/// host and the feature matrix account for it the same day, with nobody having to remember to come
/// back and edit a constant table.
pub(crate) fn library_crates() -> Vec<String> {
    let root = workspace_root().join("crates");
    crate_names()
        .into_iter()
        .filter(|name| root.join(name).join("src").join("lib.rs").is_file())
        .collect()
}

/// Whether the whole line is a comment.
///
/// Only leading comments count and **no lexing is done**: a gate should rather miss something
/// than cry wolf — flagging valid code makes people distrust the gate itself, which is worse than
/// one miss. Keywords inside trailing comments are therefore missed, a known and accepted cost.
pub(crate) fn is_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')
}
