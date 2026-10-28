// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![cfg(unix)]
use ra_paths::resolve_symlink_write_paths;
use std::os::unix::fs::symlink;
#[test]
fn symlink_cycles_fall_back_to_root_write_path() -> std::io::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = dir.path().join("a");
    let b = dir.path().join("b");

    symlink(&b, &a)?;
    symlink(&a, &b)?;

    let resolved = resolve_symlink_write_paths(&a)?;

    assert_eq!(resolved.read_path, None);
    assert_eq!(resolved.write_path, a);
    Ok(())
}
