// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![cfg(windows)]
use ra_exec::filesystem::test_api::local_file_system::symlink_points_to_directory;
use std::io;

#[test]
fn symlink_points_to_directory_handles_dangling_directory_symlinks() -> io::Result<()> {
    use std::os::windows::fs::symlink_dir;

    let temp_dir = tempfile::TempDir::new()?;
    let source_dir = temp_dir.path().join("source");
    let link_path = temp_dir.path().join("source-link");
    std::fs::create_dir(&source_dir)?;

    if symlink_dir(&source_dir, &link_path).is_err() {
        return Ok(());
    }

    std::fs::remove_dir(&source_dir)?;

    assert_eq!(symlink_points_to_directory(&link_path)?, true);
    Ok(())
}
