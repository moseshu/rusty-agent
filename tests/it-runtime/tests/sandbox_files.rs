//! The sandbox file tools against the real local backend.
//!
//! The editor and `view_image` are tested against scripted sessions in `it-tools`; these runs check
//! what only a real workspace shows — files on disk, parent directories created, and a link that
//! leads out of the workspace refused by the session's resolving path check. The first test is the
//! unix-local half of the reference's `tests/sandbox/test_posix_tool_paths.py`.

use std::path::PathBuf;
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ra_core::{
    item::ImageSource,
    sandbox::{
        CreateRequest, ErrorCode, Manifest, SandboxClient, SandboxSession, SandboxWorkspaceScope,
    },
    tool::ToolOutputBlock,
};
use ra_patch::ApplyPatchOperation;
use ra_sandbox::unix_local::UnixLocalSandboxClient;
use ra_tools::sandbox::apply_patch::WorkspaceEditor;
use ra_tools::sandbox::shell_tool::resolve_workdir_command;
use ra_tools::sandbox::view_image::{ViewImageArgs, ViewImageTool};

const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a84QAAAAASUVORK5CYII=";

async fn live_session() -> (tempfile::TempDir, PathBuf, Arc<dyn SandboxSession>) {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path())
        .unwrap()
        .join("workspace");
    let session = UnixLocalSandboxClient::new()
        .create(
            CreateRequest::new()
                .with_manifest(Manifest::new().with_root(root.to_string_lossy().into_owned())),
        )
        .await
        .unwrap();
    session.start().await.unwrap();
    (directory, root, Arc::from(session))
}

// `test_shell_workdir_normalizes_backslashes_before_unix_local_resolution`
#[tokio::test]
async fn a_shell_workdir_reads_backslashes_as_separators_on_the_local_backend() {
    let (_directory, root, session) = live_session().await;

    let command = resolve_workdir_command(
        session.as_ref(),
        &SandboxWorkspaceScope::root(),
        "pwd",
        Some(r"src\project"),
    )
    .await
    .unwrap();

    assert_eq!(
        command,
        format!("cd {}/src/project && pwd", root.to_string_lossy())
    );
}

#[tokio::test]
async fn the_editor_creates_moves_and_deletes_files_in_the_workspace() {
    let (_directory, root, session) = live_session().await;
    let editor = WorkspaceEditor::new(session.as_ref())
        .with_workspace_scope(SandboxWorkspaceScope::from_cwd(Some("tasks/a")).unwrap());

    let created = editor
        .apply_operation(&ApplyPatchOperation::create_file(
            "notes/today.txt",
            "+one\n+two\n",
        ))
        .await
        .unwrap();
    assert_eq!(created.output_text(), Some("Created notes/today.txt"));
    assert_eq!(
        std::fs::read_to_string(root.join("tasks/a/notes/today.txt")).unwrap(),
        "one\ntwo"
    );

    let moved = editor
        .apply_operation(
            &ApplyPatchOperation::update_file("notes/today.txt", "@@\n-one\n+uno\n two")
                .with_move_to("archive/today.txt"),
        )
        .await
        .unwrap();
    assert_eq!(
        moved.output_text(),
        Some("Updated notes/today.txt\nMoved notes/today.txt to archive/today.txt")
    );
    assert!(!root.join("tasks/a/notes/today.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("tasks/a/archive/today.txt")).unwrap(),
        "uno\ntwo"
    );

    let deleted = editor
        .apply_operation(&ApplyPatchOperation::delete_file("archive/today.txt"))
        .await
        .unwrap();
    assert_eq!(deleted.output_text(), Some("Deleted archive/today.txt"));
    assert!(!root.join("tasks/a/archive/today.txt").exists());

    let missing = editor
        .apply_operation(&ApplyPatchOperation::delete_file("archive/today.txt"))
        .await
        .unwrap_err();
    assert_eq!(missing.error_code(), ErrorCode::ApplyPatchFileNotFound);
    assert_eq!(
        missing.message(),
        "apply_patch missing file: archive/today.txt"
    );
}

/// The lexical check passes a path through a link inside the workspace; the local backend resolves
/// the link and refuses where it leads.
#[tokio::test]
async fn a_link_out_of_the_workspace_is_refused_by_the_session() {
    let (directory, root, session) = live_session().await;
    let outside = std::fs::canonicalize(directory.path())
        .unwrap()
        .join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

    let error = WorkspaceEditor::new(session.as_ref())
        .apply_operation(&ApplyPatchOperation::create_file(
            "link/escape.txt",
            "+escaped",
        ))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
    assert!(!outside.join("escape.txt").exists());
}

#[tokio::test]
async fn view_image_reads_an_image_from_the_workspace() {
    let (_directory, root, session) = live_session().await;
    std::fs::create_dir_all(root.join("tasks/a/images")).unwrap();
    std::fs::write(
        root.join("tasks/a/images/dot.png"),
        BASE64.decode(PNG_BASE64).unwrap(),
    )
    .unwrap();
    let tool = ViewImageTool::new(Arc::clone(&session))
        .unwrap()
        .with_workspace_scope(SandboxWorkspaceScope::from_cwd(Some("tasks/a")).unwrap());

    let output = tool
        .run(&ViewImageArgs::new("images/dot.png"))
        .await
        .unwrap();
    match output.blocks() {
        [ToolOutputBlock::Image(block)] => match block.source() {
            ImageSource::Base64(source) => {
                assert_eq!(source.media_type(), "image/png");
                assert_eq!(source.data(), PNG_BASE64);
            }
            other => panic!("expected an inline image, got {other:?}"),
        },
        other => panic!("expected one image block, got {other:?}"),
    }

    let missing = tool
        .run(&ViewImageArgs::new("images/gone.png"))
        .await
        .unwrap();
    assert_eq!(
        missing.as_text(),
        Some("image path `images/gone.png` was not found")
    );
}

/// The local backend's bounded read stops at its limit however large the file is: a 1 GiB sparse
/// file comes back as exactly the bytes asked for, and `view_image` refuses it having read only one
/// byte past its ceiling.
#[tokio::test]
async fn a_huge_file_is_read_only_up_to_the_limit() {
    let (_directory, root, session) = live_session().await;
    let huge = root.join("huge.png");
    let file = std::fs::File::create(&huge).unwrap();
    file.set_len(1 << 30).unwrap();
    drop(file);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&huge)
        .and_then(|mut file| std::io::Write::write_all(&mut file, b"\x89PNG\r\n\x1a\n"))
        .unwrap();

    let limit = u64::try_from(ra_tools::sandbox::view_image::MAX_IMAGE_BYTES).unwrap() + 1;
    let data = session
        .read_up_to(huge.to_str().unwrap(), None, limit)
        .await
        .unwrap();
    assert_eq!(u64::try_from(data.len()).unwrap(), limit);
    assert!(data.starts_with(b"\x89PNG\r\n\x1a\n"));

    let tool = ViewImageTool::new(Arc::clone(&session)).unwrap();
    let output = tool.run(&ViewImageArgs::new("huge.png")).await.unwrap();
    assert_eq!(
        output.as_text(),
        Some(
            "image path `huge.png` exceeded the allowed size of 10MB; resize or compress the \
             image and try again"
        )
    );

    let missing = session
        .read_up_to(root.join("gone.png").to_str().unwrap(), None, limit)
        .await
        .unwrap_err();
    assert_eq!(missing.error_code(), ErrorCode::WorkspaceReadNotFound);
}
