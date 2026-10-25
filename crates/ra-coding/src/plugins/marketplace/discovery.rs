// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Filesystem-only Git root discovery used by marketplace scans.

use std::path::{Path, PathBuf};

pub(super) fn get_git_repo_root(base_dir: &Path) -> Option<PathBuf> {
    let base = if base_dir.is_dir() {
        base_dir
    } else {
        base_dir.parent()?
    };
    find_ancestor_git_entry(base).map(|(repo_root, _)| repo_root)
}

fn find_ancestor_git_entry(base_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = base_dir.to_path_buf();

    loop {
        let dot_git = dir.join(".git");
        if dot_git.exists() && (!dot_git.is_dir() || dot_git.join("HEAD").exists()) {
            return Some((dir, dot_git));
        }

        // Pop one component (go up one directory). `pop` returns false when
        // we have reached the filesystem root.
        if !dir.pop() {
            break;
        }
    }

    None
}
