// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Logical policy roots and top-level system aliases follow the upstream path contract.

#![cfg(unix)]

use std::{fs, os::unix::fs::symlink, path::PathBuf};

use ra_coding::{
    permissions::{
        FileSystemAccessMode as Access, FileSystemPath, FileSystemPermissions,
        FileSystemSandboxEntry, FileSystemSandboxPolicyContext, FileSystemSpecialPath,
    },
    skills::access_test_api::{
        canonicalize_preserving_symlinks, effective_read_entries, normalize_effective_path,
    },
};

fn native_entry(path: PathBuf, access: Access) -> FileSystemSandboxEntry {
    FileSystemSandboxEntry::new(FileSystemPath::Path { path }, access)
}

#[test]
fn canonicalize_preserving_symlinks_keeps_logical_symlink_path() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    let link = directory.path().join("link");
    fs::create_dir(&real).unwrap();
    symlink(&real, &link).unwrap();

    assert_ne!(fs::canonicalize(&link).unwrap(), link);
    assert_eq!(canonicalize_preserving_symlinks(&link).unwrap(), link);
}

#[test]
fn canonicalize_preserving_symlinks_keeps_logical_missing_child_under_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    let link = directory.path().join("link");
    fs::create_dir(&real).unwrap();
    symlink(&real, &link).unwrap();
    let missing = link.join("missing.txt");

    assert_eq!(canonicalize_preserving_symlinks(&missing).unwrap(), missing);
    assert_eq!(normalize_effective_path(&missing), missing);
}

#[test]
fn effective_runtime_roots_preserve_symlinked_paths() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    let link = directory.path().join("link");
    fs::create_dir_all(real.join("blocked")).unwrap();
    symlink(&real, &link).unwrap();
    let entries = vec![
        native_entry(link.clone(), Access::Write),
        native_entry(link.join("blocked"), Access::Deny),
        native_entry(link.join("missing/nested"), Access::Read),
    ];
    let roots = [directory.path().to_path_buf()];
    let context = FileSystemSandboxPolicyContext::new(directory.path(), &roots);

    assert_eq!(
        effective_read_entries(&FileSystemPermissions::new(entries.clone()), &context).unwrap(),
        entries
    );
}

#[test]
fn project_roots_special_path_preserves_symlinked_root() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    let link = directory.path().join("link");
    fs::create_dir_all(real.join("blocked")).unwrap();
    symlink(&real, &link).unwrap();
    let blocked = link.join("blocked");
    let policy = FileSystemPermissions::new(vec![
        FileSystemSandboxEntry::new(
            FileSystemPath::Special {
                value: FileSystemSpecialPath::project_roots(None),
            },
            Access::Write,
        ),
        native_entry(blocked.clone(), Access::Deny),
    ]);
    let roots = [link.clone()];
    let context = FileSystemSandboxPolicyContext::new(&link, &roots);

    assert_eq!(
        effective_read_entries(&policy, &context).unwrap(),
        vec![
            native_entry(link, Access::Write),
            native_entry(blocked, Access::Deny)
        ]
    );
}

#[test]
fn glob_prefixes_preserve_nested_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    let link = directory.path().join("link");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("reference.md"), "reference").unwrap();
    symlink(&real, &link).unwrap();
    let roots = [directory.path().to_path_buf()];
    let context = FileSystemSandboxPolicyContext::new(directory.path(), &roots);
    for suffix in ["*.md", "reference.md"] {
        let pattern = format!("{}/{suffix}", link.display());
        let entry =
            FileSystemSandboxEntry::new(FileSystemPath::GlobPattern { pattern }, Access::Deny);
        let policy = FileSystemPermissions::new(vec![entry.clone()]);
        let effective = effective_read_entries(&policy, &context).unwrap();

        assert!(!effective.is_empty());
        assert!(effective.iter().all(|candidate| candidate == &entry));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn system_aliases_resolve_without_nested_symlinks_and_preserve_missing_suffixes() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let logical = directory.path();
    let canonical = fs::canonicalize(logical).unwrap();
    assert_ne!(logical, canonical);
    assert_eq!(
        canonicalize_preserving_symlinks(logical).unwrap(),
        canonical
    );
    assert_eq!(
        normalize_effective_path(&logical.join("missing/nested")),
        canonical.join("missing/nested")
    );
    assert_eq!(
        canonicalize_preserving_symlinks(&logical.join("missing/nested")).unwrap(),
        logical.join("missing/nested")
    );

    let real = logical.join("real");
    let link = logical.join("link");
    fs::create_dir(&real).unwrap();
    symlink(&real, &link).unwrap();
    // A nested link preserves the entire logical path, including the /tmp spelling.
    assert_eq!(canonicalize_preserving_symlinks(&link).unwrap(), link);
    assert_eq!(
        normalize_effective_path(&link.join("missing")),
        link.join("missing")
    );
}
