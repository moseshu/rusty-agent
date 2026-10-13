//! Descriptor-scoped replacement and exclusive creation do not rewrite shared inodes.

use std::{path::Path, sync::Arc};

use ra_exec::fs::RootedFileSystem;

#[test]
fn exclusive_creation_preserves_an_existing_hardlink() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let protected = outside.path().join("protected");
    std::fs::write(&protected, "original").unwrap();
    std::fs::hard_link(&protected, workspace.path().join("alias")).unwrap();
    let filesystem = RootedFileSystem::open(workspace.path()).unwrap();
    assert!(
        filesystem
            .create_new_file(Path::new("alias"), b"changed")
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&protected).unwrap(), "original");
}

#[test]
fn replacement_detaches_a_hardlink_and_cleans_up_staging() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let protected = outside.path().join("protected");
    let alias = workspace.path().join("alias");
    std::fs::write(&protected, "original").unwrap();
    std::fs::hard_link(&protected, &alias).unwrap();
    let filesystem = RootedFileSystem::open(workspace.path()).unwrap();
    filesystem
        .replace_file(Path::new("alias"), b"changed")
        .unwrap();
    assert_eq!(std::fs::read_to_string(&protected).unwrap(), "original");
    assert_eq!(std::fs::read_to_string(&alias).unwrap(), "changed");
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 1);
}

#[test]
fn replacement_does_not_write_a_racing_hardlink() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let protected = outside.path().join("protected");
    let alias = workspace.path().join("alias");
    std::fs::write(&protected, "original").unwrap();
    std::fs::hard_link(&protected, &alias).unwrap();
    let filesystem = Arc::new(RootedFileSystem::open(workspace.path()).unwrap());
    let protected_clone = protected.clone();
    let racer = std::thread::spawn(move || {
        for _ in 0..200 {
            let _ = std::fs::remove_file(&alias);
            let _ = std::fs::hard_link(&protected_clone, &alias);
        }
    });
    for _ in 0..200 {
        let _ = filesystem.replace_file(Path::new("alias"), b"changed");
    }
    racer.join().unwrap();
    assert_eq!(std::fs::read_to_string(&protected).unwrap(), "original");
    assert!(std::fs::read_dir(workspace.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".ra-patch-")
    }));
}

#[cfg(unix)]
#[test]
fn replacement_preserves_file_permissions() {
    use std::os::unix::fs::PermissionsExt as _;
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("script");
    std::fs::write(&path, "original").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
    let filesystem = RootedFileSystem::open(workspace.path()).unwrap();
    filesystem
        .replace_file(Path::new("script"), b"changed")
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o751
    );
}

#[cfg(unix)]
#[test]
fn writes_reject_symlinked_parents_and_targets() {
    let workspace = tempfile::tempdir().unwrap();
    let protected = workspace.path().join("protected");
    std::fs::create_dir(&protected).unwrap();
    std::fs::write(protected.join("file"), "original").unwrap();
    std::os::unix::fs::symlink("protected", workspace.path().join("link")).unwrap();
    std::os::unix::fs::symlink("protected/file", workspace.path().join("file-link")).unwrap();
    let filesystem = RootedFileSystem::open(workspace.path()).unwrap();
    assert!(
        filesystem
            .create_new_file(Path::new("link/new"), b"changed")
            .is_err()
    );
    assert!(
        filesystem
            .replace_file(Path::new("link/file"), b"changed")
            .is_err()
    );
    assert!(
        filesystem
            .replace_file(Path::new("file-link"), b"changed")
            .is_err()
    );
    assert!(
        filesystem
            .remove_file_nofollow(Path::new("link/file"))
            .is_err()
    );
    assert!(
        filesystem
            .rename_nofollow(Path::new("link/file"), Path::new("moved"))
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(protected.join("file")).unwrap(),
        "original"
    );
    assert!(!protected.join("new").exists());
    assert!(!workspace.path().join("moved").exists());
}
