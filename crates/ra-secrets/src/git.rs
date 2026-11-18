// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Filesystem-only Git root discovery used to name secret environments.

use std::path::{Path, PathBuf};

pub(crate) fn get_git_repo_root(base_dir: &Path) -> Option<PathBuf> {
    let base = if base_dir.is_dir() {
        base_dir
    } else {
        base_dir.parent()?
    };
    find_ancestor_git_entry(base)
}

fn find_ancestor_git_entry(base_dir: &Path) -> Option<PathBuf> {
    let mut dir = base_dir.to_path_buf();

    loop {
        let dot_git = dir.join(".git");
        if dot_git.exists() && (!dot_git.is_dir() || dot_git.join("HEAD").exists()) {
            return Some(dir);
        }

        // Pop one component (go up one directory). `pop` returns false when
        // we have reached the filesystem root.
        if !dir.pop() {
            break;
        }
    }

    None
}
