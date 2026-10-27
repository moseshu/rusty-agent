// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_exec::filesystem::test_api::local_file_system::*;
use ra_filesystem::models::PermissionProfile;
use ra_filesystem::permissions::FileSystemSandboxPolicy;
use ra_filesystem::permissions::NetworkSandboxPolicy;
use ra_filesystem::*;
use ra_paths::PathUri;
use std::io;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn sync_walk_rejects_sandbox_context() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = PathUri::from_host_native_path(temp.path())?;
    let sandbox = FileSystemSandboxContext::from_permission_profile(
        PermissionProfile::from_runtime_permissions(
            &FileSystemSandboxPolicy::restricted(Vec::new()),
            NetworkSandboxPolicy::Restricted,
        ),
        root.clone(),
    );
    let options = WalkOptions {
        max_depth: 1,
        max_directories: 1,
        max_entries: 1,
        follow_directory_symlinks: false,
        prune_hidden_directories: false,
    };
    let direct_error = DirectFileSystem
        .walk(&root, options, Some(&sandbox))
        .await
        .expect_err("direct walk must reject sandbox contexts");
    let wrapper_error = UnsandboxedFileSystem::default()
        .walk(&root, options, Some(&sandbox))
        .await
        .expect_err("unsandboxed walk must reject restricted contexts");
    assert_eq!(direct_error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(wrapper_error.kind(), io::ErrorKind::InvalidInput);
    Ok(())
}
#[test]
fn sync_walk_cancellation_stops_before_io() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let missing = PathUri::from_host_native_path(temp.path().join("missing"))?;
    let options = WalkOptions {
        max_depth: 1,
        max_directories: 1,
        max_entries: 1,
        follow_directory_symlinks: true,
        prune_hidden_directories: false,
    };
    let cancelled = CancellationToken::new();
    let cancel_on_drop = cancelled.clone().drop_guard();
    drop(cancel_on_drop);
    for result in [
        DirectFileSystem::sync_walk(&missing, options, &cancelled).map(|_| ()),
        walk_read_directory(&missing, &cancelled).map(|_| ()),
    ] {
        assert_eq!(
            result
                .expect_err("cancelled walks must stop before I/O")
                .kind(),
            io::ErrorKind::Interrupted,
        );
    }
    Ok(())
}
#[test]
fn sync_walk_response_budget_counts_entries_and_errors() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = PathUri::from_host_native_path(temp.path())?;
    let mut outcome = WalkOutcome::default();
    let mut response_bytes =
        MAX_WALK_RESPONSE_BYTES - WALK_RESPONSE_ITEM_OVERHEAD_BYTES - root.to_string().len();
    assert!(push_walk_error(
        &mut outcome,
        &mut response_bytes,
        root.clone(),
        String::new()
    ));
    assert!(!reserve_walk_response_bytes(
        &mut outcome,
        &mut response_bytes,
        0
    ));
    assert_eq!(
        outcome,
        WalkOutcome {
            entries: Vec::new(),
            errors: vec![WalkError {
                path: root,
                message: String::new()
            }],
            truncated: true,
        },
    );
    Ok(())
}
