//! Native filesystem contract and sandbox-helper integration.
use futures::StreamExt;
use ra_executor::{
    CopyOptions, ExecServerRuntimeOptions, ExecutorFileSystem, FileSystemSandboxContext,
    GetMetadataOptions, LocalFileSystem, ReadFileOptions, WalkOptions, WriteFileOptions,
};
use ra_filesystem::{
    models::PermissionProfile,
    permissions::{
        FileSystemAccessMode, FileSystemSandboxEntry, FileSystemSandboxPolicy, NetworkSandboxPolicy,
    },
};
use ra_paths::PathUri;

#[tokio::test]
async fn native_operations_preserve_walk_limits_and_copy_containment() {
    let temp = tempfile::tempdir().unwrap();
    let root = PathUri::from_host_native_path(temp.path()).unwrap();
    let file = root.join("source.txt").unwrap();
    let copy = root.join("copy.txt").unwrap();
    let fs = LocalFileSystem::unsandboxed();
    fs.write_file(
        &file,
        b"content".to_vec(),
        WriteFileOptions::default(),
        None,
    )
    .await
    .unwrap();
    fs.copy(&file, &copy, CopyOptions { recursive: false }, None)
        .await
        .unwrap();
    assert_eq!(
        fs.read_file_text(&copy, ReadFileOptions::default(), None)
            .await
            .unwrap(),
        "content"
    );
    assert_eq!(
        fs.get_metadata(&file, GetMetadataOptions::default(), None)
            .await
            .unwrap()
            .size,
        7
    );
    let walk = fs
        .walk(
            &root,
            WalkOptions {
                max_entries: 1,
                max_depth: 6,
                max_directories: 100,
                follow_directory_symlinks: false,
                prune_hidden_directories: false,
            },
            None,
        )
        .await
        .unwrap();
    assert!(walk.truncated);
    assert_eq!(walk.entries.len(), 1);
    let err = fs
        .copy(
            &root,
            &root.join("child").unwrap(),
            CopyOptions { recursive: true },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

fn restricted_context(root: &PathUri) -> FileSystemSandboxContext {
    let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry::new(
        root.clone().into(),
        FileSystemAccessMode::Write,
    )]);
    let permissions =
        PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted);
    FileSystemSandboxContext::from_permission_profile(permissions, root.clone())
}

#[tokio::test]
async fn missing_runtime_never_falls_back_to_unsandboxed_access() {
    let temp = tempfile::tempdir().unwrap();
    let root = PathUri::from_host_native_path(temp.path()).unwrap();
    std::fs::write(temp.path().join("secret"), "secret").unwrap();
    let fs = LocalFileSystem::unsandboxed();
    let err = fs
        .read_file(
            &root.join("secret").unwrap(),
            ReadFileOptions::default(),
            Some(&restricted_context(&root)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("configured runtime paths"));
}

#[cfg(unix)]
#[tokio::test]
async fn no_follow_and_non_regular_files_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("data"), "bytes").unwrap();
    std::os::unix::fs::symlink(temp.path().join("data"), temp.path().join("link")).unwrap();
    let root = PathUri::from_host_native_path(temp.path()).unwrap();
    let fs = LocalFileSystem::unsandboxed();
    assert!(
        fs.read_file(
            &root.join("link").unwrap(),
            ReadFileOptions {
                follow_symlinks: false
            },
            None
        )
        .await
        .is_err()
    );
    assert!(
        fs.read_file(&root, ReadFileOptions::default(), None)
            .await
            .is_err()
    );
    let mut stream = fs
        .read_file_stream(&root.join("data").unwrap(), None)
        .await
        .unwrap();
    assert_eq!(&stream.next().await.unwrap().unwrap()[..], b"bytes");
    assert!(stream.next().await.is_none());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn seatbelt_helper_enforces_roots_and_transfers_stream_descriptors() {
    let temp = tempfile::tempdir().unwrap();
    let allowed = temp.path().join("allowed");
    std::fs::create_dir(&allowed).unwrap();
    let denied = temp.path().join("denied.txt");
    std::fs::write(&denied, "private").unwrap();
    let root = PathUri::from_host_native_path(&allowed).unwrap();
    let context = restricted_context(&root);
    let helper = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/ra-fs-helper")
        .canonicalize()
        .expect("build ra-fs-helper before running this target");
    let fs =
        LocalFileSystem::with_runtime_paths(ExecServerRuntimeOptions::new(helper, None).unwrap());
    let file = root.join("data.txt").unwrap();
    fs.write_file(
        &file,
        b"sandboxed".to_vec(),
        WriteFileOptions::default(),
        Some(&context),
    )
    .await
    .unwrap();
    assert_eq!(
        fs.read_file_text(&file, ReadFileOptions::default(), Some(&context))
            .await
            .unwrap(),
        "sandboxed"
    );
    assert!(
        fs.read_file(
            &PathUri::from_host_native_path(&denied).unwrap(),
            ReadFileOptions::default(),
            Some(&context)
        )
        .await
        .is_err()
    );
    let mut stream = fs.read_file_stream(&file, Some(&context)).await.unwrap();
    assert_eq!(&stream.next().await.unwrap().unwrap()[..], b"sandboxed");
    assert!(stream.next().await.is_none());
}
