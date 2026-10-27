// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![cfg(unix)]
use ra_exec::filesystem::test_api::local_file_system::*;
use std::io;

use std::os::unix::fs::symlink;
#[test]
fn resolve_existing_path_handles_symlink_parent_dotdot_escape() -> io::Result<()> {
    let temp_dir = tempfile::TempDir::new()?;
    let allowed_dir = temp_dir.path().join("allowed");
    let outside_dir = temp_dir.path().join("outside");
    std::fs::create_dir_all(&allowed_dir)?;
    std::fs::create_dir_all(&outside_dir)?;
    symlink(&outside_dir, allowed_dir.join("link"))?;
    let resolved = resolve_existing_path(
        allowed_dir
            .join("link")
            .join("..")
            .join("secret.txt")
            .as_path(),
    )?;
    assert_eq!(
        resolved,
        resolve_existing_path(temp_dir.path())?.join("secret.txt")
    );
    Ok(())
}
