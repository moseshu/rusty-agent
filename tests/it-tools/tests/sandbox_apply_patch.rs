//! `ra-tools::sandbox::apply_patch`: the workspace editor against an in-memory session.
//!
//! Ported from the reference's `tests/sandbox/test_apply_patch.py`, one test per upstream test in
//! the upstream order, followed by the editor tests of
//! `tests/sandbox/capabilities/test_apply_patch_tool.py` — the ones that drive
//! `SandboxApplyPatchTool.editor` rather than the tool's custom-tool entry.
//!
//! The session is the Rust counterpart of the reference's `ApplyPatchSession`: files in a map keyed
//! by the lexically normalized absolute path, every `mkdir` and `rm` recorded, and the user of every
//! file operation recorded, as `UserRecordingApplyPatchSession` does. The reference's
//! `session.apply_patch(...)` is `WorkspaceEditor::new(session).apply_patch(...)` here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, ErrorCode, ExecRequest, ExecResult, FileEntry, Manifest, OpName, SandboxError,
    SandboxResult, SandboxSession, SandboxSessionState, SandboxWorkspaceScope, SessionResources,
    Snapshot, User,
};
use ra_patch::{ApplyDiffError, ApplyDiffMode, ApplyPatchOperation, ApplyPatchOperationType};
use ra_tools::sandbox::apply_patch::{PatchFormat, WorkspaceEditor, operations_from_json};
use serde_json::json;

// ---- the in-memory session -----------------------------------------------------------------

#[derive(Default)]
struct Record {
    files: BTreeMap<String, Vec<u8>>,
    mkdir_calls: Vec<(String, bool)>,
    rm_calls: Vec<(String, bool)>,
    read_users: Vec<Option<String>>,
    write_users: Vec<Option<String>>,
    mkdir_users: Vec<Option<String>>,
    rm_users: Vec<Option<String>>,
}

struct PatchSession {
    state: SandboxSessionState,
    resources: SessionResources,
    /// Reports a missing file under a provider-private root, as `ProviderNotFoundApplyPatchSession`.
    provider_not_found: bool,
    record: Mutex<Record>,
}

impl PatchSession {
    fn new() -> Arc<Self> {
        Self::with_root("/workspace", false)
    }

    fn provider_not_found() -> Arc<Self> {
        Self::with_root("/workspace", true)
    }

    fn with_root(root: &str, provider_not_found: bool) -> Arc<Self> {
        Arc::new(Self {
            state: SandboxSessionState::new(
                "patch",
                Snapshot::noop(),
                Manifest::new().with_root(root),
            ),
            resources: SessionResources::new(),
            provider_not_found,
            record: Mutex::new(Record::default()),
        })
    }

    fn put(&self, path: &str, contents: &[u8]) {
        self.record
            .lock()
            .unwrap()
            .files
            .insert(path.to_owned(), contents.to_vec());
    }

    fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.record.lock().unwrap().files.get(path).cloned()
    }

    fn files(&self) -> BTreeMap<String, Vec<u8>> {
        self.record.lock().unwrap().files.clone()
    }

    fn with<T>(&self, read: impl FnOnce(&Record) -> T) -> T {
        read(&self.record.lock().unwrap())
    }

    /// The reference's lexical `normalize_path`.
    fn normalize(&self, path: &str) -> SandboxResult<String> {
        Ok(self
            .workspace_path_policy()?
            .normalize_sandbox_path(path, false)?
            .to_string())
    }
}

fn user_name(user: &AsUser) -> Option<String> {
    user.as_ref().map(|user| user.name.clone())
}

fn not_scripted() -> SandboxError {
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        OpName::Exec,
        "not scripted",
    )
}

#[async_trait]
impl SandboxSession for PatchSession {
    fn backend_id(&self) -> &str {
        "patch"
    }

    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    async fn exec(&self, _request: ExecRequest) -> SandboxResult<ExecResult> {
        panic!("the editor never executes commands");
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(true)
    }

    async fn ls(&self, _path: &str, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        Err(not_scripted())
    }

    async fn rm(&self, path: &str, recursive: bool, user: AsUser) -> SandboxResult<()> {
        let normalized = self.normalize(path)?;
        let mut record = self.record.lock().unwrap();
        record.rm_users.push(user_name(&user));
        record.rm_calls.push((normalized.clone(), recursive));
        record.files.remove(&normalized);
        Ok(())
    }

    async fn mkdir(&self, path: &str, parents: bool, user: AsUser) -> SandboxResult<()> {
        let normalized = self.normalize(path)?;
        let mut record = self.record.lock().unwrap();
        record.mkdir_users.push(user_name(&user));
        record.mkdir_calls.push((normalized, parents));
        Ok(())
    }

    async fn read(&self, path: &str, user: AsUser) -> SandboxResult<Vec<u8>> {
        let normalized = self.normalize(path)?;
        let mut record = self.record.lock().unwrap();
        record.read_users.push(user_name(&user));
        match record.files.get(&normalized) {
            Some(contents) => Ok(contents.clone()),
            None if self.provider_not_found => Err(SandboxError::workspace_read_not_found(
                &format!("/provider/private/root{normalized}"),
            )),
            None => Err(SandboxError::workspace_read_not_found(&normalized)),
        }
    }

    async fn write(&self, path: &str, data: Vec<u8>, user: AsUser) -> SandboxResult<()> {
        let normalized = self.normalize(path)?;
        let mut record = self.record.lock().unwrap();
        record.write_users.push(user_name(&user));
        record.files.insert(normalized, data);
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Ok(())
    }
}

fn editor(session: &Arc<PatchSession>) -> WorkspaceEditor<'_> {
    WorkspaceEditor::new(session.as_ref())
}

fn scoped<'a>(session: &'a Arc<PatchSession>, cwd: &str) -> WorkspaceEditor<'a> {
    editor(session).with_workspace_scope(
        SandboxWorkspaceScope::from_cwd(Some(cwd)).expect("working directory"),
    )
}

async fn apply(
    session: &Arc<PatchSession>,
    operation: ApplyPatchOperation,
) -> SandboxResult<String> {
    editor(session).apply_patch(&[operation]).await
}

fn update(path: &str, diff: &str) -> ApplyPatchOperation {
    ApplyPatchOperation::update_file(path, diff)
}

fn create(path: &str, diff: &str) -> ApplyPatchOperation {
    ApplyPatchOperation::create_file(path, diff)
}

// ---- test_apply_patch.py -------------------------------------------------------------------

#[tokio::test]
async fn an_update_whose_context_is_not_there_is_an_invalid_diff() {
    let session = PatchSession::new();
    session.put("/workspace/bad.txt", b"alpha\nbeta\n");

    let error = apply(&session, update("bad.txt", "@@\n missing\n-beta\n+gamma\n"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidDiff);
    assert_eq!(error.message(), "Invalid Context 0:\nmissing\nbeta");
    assert_eq!(error.context().get("path"), Some(&json!("bad.txt")));
}

#[tokio::test]
async fn an_update_jumps_to_its_anchor() {
    let session = PatchSession::new();
    session.put("/workspace/anchor.txt", b"a\nb\nmarker\nc\nd\n");

    apply(&session, update("anchor.txt", "@@ marker\n c\n-d\n+e\n"))
        .await
        .unwrap();

    assert_eq!(
        session.file("/workspace/anchor.txt").unwrap(),
        b"a\nb\nmarker\nc\ne\n"
    );
}

#[tokio::test]
async fn an_update_follows_stacked_anchors() {
    let session = PatchSession::new();
    session.put(
        "/workspace/stacked.py",
        b"class First\n    def target():\n        return 0\n\nclass Second\n    def helper():\n        \
          pass\n\n    def target():\n        pass\n",
    );

    apply(
        &session,
        update(
            "stacked.py",
            "@@ class Second\n@@     def target():\n-        pass\n+        return 1\n",
        ),
    )
    .await
    .unwrap();

    assert_eq!(
        session.file("/workspace/stacked.py").unwrap(),
        b"class First\n    def target():\n        return 0\n\nclass Second\n    def helper():\n        \
          pass\n\n    def target():\n        return 1\n"
    );
}

#[tokio::test]
async fn partially_matched_stacked_anchors_leave_the_file_alone() {
    let session = PatchSession::new();
    let original: &[u8] =
        b"class Target\n    def helper():\n        pass\n\n    def desired():\n        return 1\n";
    session.put("/workspace/stacked.py", original);

    let error = apply(
        &session,
        update(
            "stacked.py",
            "@@ class Target\n@@     def missing():\n-        pass\n+        return 99\n",
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidDiff);
    assert!(error.message().contains("Invalid Anchor"), "{error}");
    assert_eq!(session.file("/workspace/stacked.py").unwrap(), original);
}

#[tokio::test]
async fn an_update_matches_end_of_file_context() {
    let session = PatchSession::new();
    session.put("/workspace/tail.txt", b"one\ntwo\nthree\n");

    apply(
        &session,
        update("tail.txt", "@@\n two\n-three\n+four\n*** End of File\n"),
    )
    .await
    .unwrap();

    assert_eq!(
        session.file("/workspace/tail.txt").unwrap(),
        b"one\ntwo\nfour\n"
    );
}

#[tokio::test]
async fn an_update_without_a_diff_is_an_invalid_diff() {
    let session = PatchSession::new();

    let error = apply(
        &session,
        ApplyPatchOperation::new(ApplyPatchOperationType::UpdateFile, "file.txt"),
    )
    .await
    .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidDiff);
    assert_eq!(
        error.message(),
        "Missing diff for operation type update_file on path file.txt"
    );
}

#[tokio::test]
async fn updating_a_missing_file_is_a_missing_file() {
    let session = PatchSession::new();

    let error = apply(&session, update("missing.txt", "@@\n-old\n+new\n"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchFileNotFound);
}

#[tokio::test]
async fn deleting_a_missing_file_is_a_missing_file() {
    let session = PatchSession::new();

    let error = apply(&session, ApplyPatchOperation::delete_file("nope.txt"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchFileNotFound);
    assert!(session.with(|record| record.rm_calls.is_empty()));
}

#[tokio::test]
async fn a_missing_file_is_named_by_its_workspace_path() {
    let session = PatchSession::provider_not_found();

    let update_error = apply(&session, update("missing.txt", "@@\n-old\n+new\n"))
        .await
        .unwrap_err();
    assert_eq!(
        update_error.message(),
        "apply_patch missing file: missing.txt"
    );
    assert_eq!(
        update_error.context().get("path"),
        Some(&json!("missing.txt"))
    );
    assert!(!update_error.message().contains("/provider/private/root"));

    let delete_error = apply(
        &session,
        ApplyPatchOperation::delete_file("missing-delete.txt"),
    )
    .await
    .unwrap_err();
    assert_eq!(
        delete_error.message(),
        "apply_patch missing file: missing-delete.txt"
    );
    assert_eq!(
        delete_error.context().get("path"),
        Some(&json!("missing-delete.txt"))
    );
    assert!(!delete_error.message().contains("/provider/private/root"));
}

#[tokio::test]
async fn a_path_climbing_out_of_the_workspace_is_refused() {
    let session = PatchSession::new();

    let error = apply(&session, create("../escape.txt", "+nope"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidPath);
    assert_eq!(
        error.message(),
        "apply_patch path must not escape root: ../escape.txt"
    );
    assert_eq!(error.context().get("reason"), Some(&json!("escape_root")));
    assert!(session.files().is_empty());
}

#[tokio::test]
async fn an_empty_path_is_refused() {
    let session = PatchSession::new();

    let error = apply(&session, create("", "+nope")).await.unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidPath);
    assert_eq!(error.message(), "apply_patch path must be non-empty");
    assert_eq!(error.context().get("reason"), Some(&json!("empty")));
}

#[tokio::test]
async fn backslashes_in_a_path_are_separators() {
    let session = PatchSession::new();

    apply(&session, create(r"nested\new.txt", "+hello"))
        .await
        .unwrap();

    assert_eq!(session.file("/workspace/nested/new.txt").unwrap(), b"hello");
}

#[tokio::test]
async fn backslashes_in_a_move_target_are_separators() {
    let session = PatchSession::new();
    session.put("/workspace/source.txt", b"alpha\n");

    apply(
        &session,
        update("source.txt", "@@\n-alpha\n+beta\n").with_move_to(r"nested\moved.txt"),
    )
    .await
    .unwrap();

    assert_eq!(
        session.file("/workspace/nested/moved.txt").unwrap(),
        b"beta\n"
    );
    assert_eq!(session.file("/workspace/source.txt"), None);
}

#[tokio::test]
async fn an_absolute_path_inside_the_workspace_is_allowed() {
    let session = PatchSession::new();

    apply(&session, create("/workspace/abs-ok.txt", "+hello"))
        .await
        .unwrap();

    assert_eq!(session.file("/workspace/abs-ok.txt").unwrap(), b"hello");
}

#[tokio::test]
async fn an_absolute_path_outside_the_workspace_is_refused() {
    let session = PatchSession::new();

    let error = apply(&session, create("/tmp/outside.txt", "+nope"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidPath);
    // The reference calls every policy refusal an escape, absolute or not.
    assert_eq!(error.context().get("reason"), Some(&json!("escape_root")));
}

#[tokio::test]
async fn a_created_file_needs_plus_lines() {
    let session = PatchSession::new();

    let error = apply(&session, create("new.txt", "oops"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidDiff);
    assert_eq!(error.message(), "Invalid Add File Line: oops");
}

#[tokio::test]
async fn a_diff_line_without_a_prefix_is_an_invalid_diff() {
    let session = PatchSession::new();
    session.put("/workspace/oops.txt", b"alpha\nbeta\n");

    let error = apply(&session, update("oops.txt", "oops"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidDiff);
    assert_eq!(error.message(), "Invalid Line: oops");
}

#[tokio::test]
async fn updating_a_file_that_is_not_utf8_is_a_decode_error() {
    let session = PatchSession::new();
    session.put("/workspace/binary.txt", b"\xff\xfe\xfd");

    let error = apply(&session, update("binary.txt", "@@\n+\n"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchDecodeError);
    // Without a working directory the reference names the resolved destination.
    assert_eq!(
        error.message(),
        "apply_patch could not decode file: /workspace/binary.txt"
    );
}

/// The reference's `StubFormat`: ignores the diff and writes the mode it was called with.
struct StubFormat;

impl PatchFormat for StubFormat {
    fn apply_diff(
        &self,
        input: &str,
        _diff: &str,
        mode: ApplyDiffMode,
    ) -> Result<String, ApplyDiffError> {
        let mode = match mode {
            ApplyDiffMode::Create => "create",
            _ => "default",
        };
        Ok(input.replace("world", mode))
    }
}

#[tokio::test]
async fn a_custom_patch_format_is_used() {
    let session = PatchSession::new();
    session.put("/workspace/custom.txt", b"hello\nworld\n");

    let result = editor(&session)
        .apply_patch_with_format(
            &[update("custom.txt", "@@\n hello\n-world\n+ignored\n")],
            &StubFormat,
        )
        .await
        .unwrap();

    assert_eq!(result, "Done!");
    assert_eq!(
        session.file("/workspace/custom.txt").unwrap(),
        b"hello\ndefault\n"
    );
}

#[tokio::test]
async fn a_workspace_root_other_than_the_default_is_honoured() {
    let session = PatchSession::with_root("/custom-workspace", false);

    apply(&session, create("new.txt", "+hello")).await.unwrap();

    assert_eq!(session.file("/custom-workspace/new.txt").unwrap(), b"hello");
}

#[tokio::test]
async fn a_json_operation_moves_a_file() {
    let session = PatchSession::new();
    session.put("/workspace/old.txt", b"alpha\n");

    let operations = operations_from_json(&json!({
        "type": "update_file",
        "path": "old.txt",
        "diff": "@@\n-alpha\n+beta\n",
        "move_to": "renamed/new.txt",
    }))
    .unwrap();
    let result = editor(&session).apply_patch(&operations).await.unwrap();

    assert_eq!(result, "Done!");
    assert_eq!(
        session.file("/workspace/renamed/new.txt").unwrap(),
        b"beta\n"
    );
    assert_eq!(session.file("/workspace/old.txt"), None);
}

#[tokio::test]
async fn a_json_operation_without_a_move_updates_in_place() {
    let session = PatchSession::new();
    session.put("/workspace/keep.txt", b"alpha\n");

    let operations = operations_from_json(&json!({
        "type": "update_file",
        "path": "keep.txt",
        "diff": "@@\n-alpha\n+beta\n",
    }))
    .unwrap();
    editor(&session).apply_patch(&operations).await.unwrap();

    assert_eq!(session.file("/workspace/keep.txt").unwrap(), b"beta\n");
    assert!(session.with(|record| record.rm_calls.is_empty()));
}

#[tokio::test]
async fn a_json_operation_with_a_move_target_that_is_not_a_string_is_refused() {
    let session = PatchSession::new();
    session.put("/workspace/old.txt", b"alpha\n");

    let error = operations_from_json(&json!({
        "type": "update_file",
        "path": "old.txt",
        "diff": "@@\n-alpha\n+beta\n",
        "move_to": 5,
    }))
    .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidDiff);
    assert_eq!(error.message(), "Invalid apply_patch move_to type: int");
    assert_eq!(session.file("/workspace/old.txt").unwrap(), b"alpha\n");
}

// ---- test_apply_patch_tool.py, the editor --------------------------------------------------

#[tokio::test]
async fn the_editor_creates_updates_and_deletes() {
    let session = PatchSession::new();
    let editor = editor(&session);

    let created = editor
        .apply_operation(&create("notes.txt", "+hello\n+world\n"))
        .await
        .unwrap();
    assert_eq!(created.output_text(), Some("Created notes.txt"));
    assert_eq!(
        session.file("/workspace/notes.txt").unwrap(),
        b"hello\nworld"
    );

    let updated = editor
        .apply_operation(&update("notes.txt", "@@\n-hello\n+hi\n world\n"))
        .await
        .unwrap();
    assert_eq!(updated.output_text(), Some("Updated notes.txt"));
    assert_eq!(session.file("/workspace/notes.txt").unwrap(), b"hi\nworld");

    let deleted = editor
        .apply_operation(&ApplyPatchOperation::delete_file("notes.txt"))
        .await
        .unwrap();
    assert_eq!(deleted.output_text(), Some("Deleted notes.txt"));
    assert_eq!(session.file("/workspace/notes.txt"), None);
}

#[tokio::test]
async fn the_editor_measures_paths_and_reports_moves_from_the_working_directory() {
    let session = PatchSession::new();
    let editor = scoped(&session, "tasks/a");

    let created = editor
        .apply_operation(&create("notes.txt", "+hello\n"))
        .await
        .unwrap();
    let moved = editor
        .apply_operation(
            &update("notes.txt", "@@\n-hello\n+hi\n").with_move_to("archive/notes.txt"),
        )
        .await
        .unwrap();

    assert_eq!(created.output_text(), Some("Created notes.txt"));
    assert_eq!(
        moved.output_text(),
        Some("Updated notes.txt\nMoved notes.txt to archive/notes.txt")
    );
    assert_eq!(session.file("/workspace/tasks/a/notes.txt"), None);
    assert_eq!(
        session
            .file("/workspace/tasks/a/archive/notes.txt")
            .unwrap(),
        b"hi"
    );
}

#[tokio::test]
async fn the_editor_takes_an_absolute_path_at_face_value_under_a_working_directory() {
    let session = PatchSession::new();

    let result = scoped(&session, "tasks/a")
        .apply_operation(&create("/workspace/root.txt", "+root\n"))
        .await
        .unwrap();

    assert_eq!(result.output_text(), Some("Created root.txt"));
    assert_eq!(session.file("/workspace/root.txt").unwrap(), b"root");
}

#[tokio::test]
async fn a_missing_file_under_a_working_directory_is_named_as_the_model_named_it() {
    let session = PatchSession::provider_not_found();

    let error = scoped(&session, "tasks/a")
        .apply_operation(&update("missing.txt", "@@\n-old\n+new\n"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchFileNotFound);
    assert_eq!(error.message(), "apply_patch missing file: missing.txt");
    assert_eq!(error.context().get("path"), Some(&json!("missing.txt")));
    assert!(!error.message().contains("/provider/private/root"));
}

/// The reference swaps in a Windows path type to check that the name stays POSIX; sandbox paths
/// here are POSIX text whatever the host, so the same assertion runs directly.
#[tokio::test]
async fn an_undecodable_file_under_a_working_directory_is_named_relative_to_it() {
    let session = PatchSession::new();
    session.put("/workspace/tasks/a/nested/binary.txt", b"\xff\xfe\xfd");

    let error = scoped(&session, "tasks/a")
        .apply_operation(&update("nested/binary.txt", "@@\n+replacement\n"))
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchDecodeError);
    assert_eq!(
        error.message(),
        "apply_patch could not decode file: nested/binary.txt"
    );
    assert_eq!(
        error.context().get("path"),
        Some(&json!("nested/binary.txt"))
    );
    assert!(!error.message().contains("/workspace/tasks/a"));
}

#[tokio::test]
async fn an_undecodable_file_named_absolutely_keeps_its_absolute_name() {
    let session = PatchSession::new();
    session.put("/workspace/root/nested/binary.txt", b"\xff\xfe\xfd");

    let error = scoped(&session, "tasks/a")
        .apply_operation(&update(
            "/workspace/root/nested/binary.txt",
            "@@\n+replacement\n",
        ))
        .await
        .unwrap_err();

    let expected = "/workspace/root/nested/binary.txt";
    assert_eq!(
        error.message(),
        format!("apply_patch could not decode file: {expected}")
    );
    assert_eq!(error.context().get("path"), Some(&json!(expected)));
}

#[tokio::test]
async fn the_editor_without_a_scope_stays_relative_to_the_workspace_root() {
    let session = PatchSession::new();

    WorkspaceEditor::new(session.as_ref())
        .apply_patch(&[create("direct.txt", "+root\n")])
        .await
        .unwrap();
    scoped(&session, "tasks/a")
        .apply_operation(&create("tool.txt", "+scoped\n"))
        .await
        .unwrap();

    assert_eq!(session.file("/workspace/direct.txt").unwrap(), b"root");
    assert_eq!(
        session.file("/workspace/tasks/a/tool.txt").unwrap(),
        b"scoped"
    );
}

#[tokio::test]
async fn the_editor_acts_as_the_bound_user() {
    let session = PatchSession::new();
    session.put("/workspace/existing.txt", b"old\n");
    let editor = editor(&session).with_user(Some(User::new("sandbox-user")));

    editor
        .apply_operation(&update("existing.txt", "@@\n-old\n+new\n"))
        .await
        .unwrap();
    editor
        .apply_operation(&create("created.txt", "+created\n"))
        .await
        .unwrap();
    editor
        .apply_operation(&ApplyPatchOperation::delete_file("existing.txt"))
        .await
        .unwrap();

    let user = Some("sandbox-user".to_owned());
    session.with(|record| {
        assert_eq!(record.read_users, vec![user.clone(), user.clone()]);
        assert_eq!(record.mkdir_users, vec![user.clone(), user.clone()]);
        assert_eq!(record.write_users, vec![user.clone(), user.clone()]);
        assert_eq!(record.rm_users, vec![user.clone()]);
    });
}

#[tokio::test]
async fn a_move_removes_its_source_as_the_bound_user() {
    let session = PatchSession::new();
    session.put("/workspace/existing.txt", b"old\n");

    let result = editor(&session)
        .with_user(Some(User::new("sandbox-user")))
        .apply_operation(&update("existing.txt", "@@\n-old\n+new\n").with_move_to("moved.txt"))
        .await
        .unwrap();

    assert_eq!(
        result.output_text(),
        Some("Updated existing.txt\nMoved existing.txt to moved.txt")
    );
    let user = Some("sandbox-user".to_owned());
    session.with(|record| {
        assert_eq!(record.read_users, vec![user.clone()]);
        assert_eq!(record.mkdir_users, vec![user.clone()]);
        assert_eq!(record.write_users, vec![user.clone()]);
        assert_eq!(record.rm_users, vec![user.clone()]);
    });
    assert_eq!(session.file("/workspace/moved.txt").unwrap(), b"new\n");
    assert_eq!(session.file("/workspace/existing.txt"), None);
}

#[tokio::test]
async fn a_move_onto_itself_removes_nothing() {
    let session = PatchSession::new();
    session.put("/workspace/existing.txt", b"old\n");

    editor(&session)
        .with_user(Some(User::new("sandbox-user")))
        .apply_operation(&update("existing.txt", "@@\n-old\n+new\n").with_move_to("existing.txt"))
        .await
        .unwrap();

    assert!(session.with(|record| record.rm_users.is_empty()));
    assert_eq!(session.file("/workspace/existing.txt").unwrap(), b"new\n");
}

// ---- beyond the reference's tests ----------------------------------------------------------

/// `normalize_operation` is what the tool shows an approval check: root-relative, anchored under
/// the working directory, backslashes read as separators, the diff untouched.
#[tokio::test]
async fn a_normalized_operation_names_the_files_it_will_touch() {
    let session = PatchSession::new();

    let normalized = editor(&session)
        .normalize_operation(&create(r"sensitive\secret.txt", "+secret\n"))
        .unwrap();
    assert_eq!(normalized.path(), "sensitive/secret.txt");
    assert_eq!(normalized.diff(), Some("+secret\n"));

    let normalized = scoped(&session, "tasks/a")
        .normalize_operation(&update("notes.txt", "@@\n-old\n+new\n").with_move_to("moved.txt"))
        .unwrap();
    assert_eq!(normalized.path(), "tasks/a/notes.txt");
    assert_eq!(normalized.move_to(), Some("tasks/a/moved.txt"));

    let error = editor(&session)
        .normalize_operation(&create("../escape.txt", "+x"))
        .unwrap_err();
    assert_eq!(error.error_code(), ErrorCode::ApplyPatchInvalidPath);
    assert!(session.files().is_empty());
}

/// Each operation is applied in turn, so one that fails leaves the ones before it applied.
#[tokio::test]
async fn a_failing_operation_keeps_the_ones_applied_before_it() {
    let session = PatchSession::new();

    let error = editor(&session)
        .apply_patch(&[
            create("first.txt", "+first"),
            update("missing.txt", "@@\n-a\n+b"),
            create("third.txt", "+third"),
        ])
        .await
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::ApplyPatchFileNotFound);
    assert_eq!(session.file("/workspace/first.txt").unwrap(), b"first");
    assert_eq!(session.file("/workspace/third.txt"), None);
}

/// A write creates its parent directories first, with parents.
#[tokio::test]
async fn a_write_creates_its_parent_directory() {
    let session = PatchSession::new();

    apply(&session, create("a/b/c.txt", "+x")).await.unwrap();

    session.with(|record| {
        assert_eq!(
            record.mkdir_calls,
            vec![("/workspace/a/b".to_owned(), true)]
        );
    });
}

/// The JSON coercion refuses what the reference refuses, naming values by their Python type.
#[test]
fn json_operations_are_coerced_as_the_reference_coerces_them() {
    let operations = operations_from_json(&json!([
        {"type": "create_file", "path": "a.txt", "diff": "+a"},
        {"type": "delete_file", "path": "b.txt", "diff": null},
    ]))
    .unwrap();
    assert_eq!(
        operations,
        vec![
            create("a.txt", "+a"),
            ApplyPatchOperation::delete_file("b.txt")
        ]
    );

    let refusal = |value: serde_json::Value| {
        operations_from_json(&value)
            .unwrap_err()
            .message()
            .to_owned()
    };
    assert_eq!(
        refusal(json!({"type": "rename", "path": "a"})),
        "Invalid apply_patch operation type: str"
    );
    assert_eq!(
        refusal(json!({"path": "a"})),
        "Invalid apply_patch operation type: NoneType"
    );
    assert_eq!(
        refusal(json!({"type": "create_file", "path": ["a"]})),
        "Invalid apply_patch path type: list"
    );
    assert_eq!(
        refusal(json!({"type": "create_file", "path": "a", "diff": 1.5})),
        "Invalid apply_patch diff type: float"
    );
    assert_eq!(
        refusal(json!([{"type": "create_file", "path": "a"}, "b"])),
        "Invalid apply_patch operation type: str"
    );
    assert_eq!(
        refusal(json!("a")),
        "Invalid apply_patch operations payload: str"
    );
}
