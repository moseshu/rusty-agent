//! The reference's `tests/sandbox/test_session_utils.py`: the helpers a session is built from.
//!
//! Its seventeen cases, and where each is kept:
//!
//! - `safe_decode`, the single-line JSON form, the phase discriminator, raw bytes staying out of a
//!   dump, and `FileEntry.is_dir` — here.
//! - `_best_effort_stream_len` (two cases) — not applicable: a write here takes the bytes, so there
//!   is no stream whose remaining length has to be guessed (the same finding as
//!   `test_workspace_payloads.py`).
//! - Quoting a multi-argument command for the shell, and passing a single snippet through — here,
//!   against `remote::prepare_exec_command`, the base session's `sh -lc` shape. The local backend's
//!   `sh -c` variant is pinned in `unix_local_command_shape`.
//! - The mkdir and rm checks run as the requested account — `unix_local_as_user`, whose stand-in
//!   `sudo` records the exact argument vector.
//! - How a failed read is classified, a check that failed in an unexpected way, the partial output
//!   kept out of the error, and the probe script's own answers — here, against `remote`'s
//!   `read_error_from_exec`, the port of `_raise_read_error_from_exec`.
//! - The persist skip path against mounts (two cases) — `it-core/sandbox_session_resources`.

#[path = "support/memory_session.rs"]
mod memory_session;

use std::path::{Path, PathBuf};
use std::process::Command;

use memory_session::MemorySession;
use ra_core::sandbox::{
    EntryKind, ErrorCode, EventPhase, ExecRequest, ExecResult, FileEntry, OpName, Permissions,
    SandboxSessionEvent, SandboxSessionFinishEvent, SandboxSessionStartEvent, ShellInvocation,
    User, event_to_json_line, safe_decode, validate_sandbox_session_event,
};
use ra_sandbox::remote::{prepare_exec_command, read_error_from_exec};
use uuid::Uuid;

// --- utilities ------------------------------------------------------------------------------------

/// `test_safe_decode_truncates_and_appends_ellipsis`.
#[test]
fn decoded_output_past_the_limit_is_cut_and_marked() {
    assert_eq!(safe_decode(b"abcdef", 3), "abc…");
}

/// `test_event_to_json_line_is_single_line`.
#[test]
fn an_event_line_is_one_line_ending_in_a_newline() {
    let mut data = serde_json::Map::new();
    data.insert("x".to_owned(), 1.into());
    let event: SandboxSessionEvent = SandboxSessionStartEvent::from_base(
        ra_core::sandbox::SandboxSessionEventBase::new(
            Uuid::new_v4(),
            1,
            OpName::Write,
            "span_write",
        )
        .with_data(data),
    )
    .into();

    let line = event_to_json_line(&event);

    assert!(line.ends_with('\n'), "{line:?}");
    assert!(!line[..line.len() - 1].contains('\n'), "{line:?}");
}

/// `test_validate_sandbox_session_event_uses_phase_discriminator`.
#[test]
fn a_dumped_event_is_read_back_as_the_phase_it_names() {
    let event = SandboxSessionStartEvent::new(Uuid::new_v4(), 1, OpName::Read, "span_read");

    let restored =
        validate_sandbox_session_event(serde_json::to_value(&event).expect("dump")).expect("read");

    assert_eq!(restored.phase(), EventPhase::Start);
    assert!(restored.as_finish().is_none());
    assert_eq!(restored.op(), OpName::Read);
}

/// `test_sandbox_session_finish_event_excludes_raw_bytes_from_json_dump`.
#[test]
fn a_finish_events_raw_output_is_not_part_of_its_dump() {
    let event =
        SandboxSessionFinishEvent::new(Uuid::new_v4(), 1, OpName::Exec, "span_exec", true, 0.0)
            .with_output(Some(b"secret".to_vec()), Some(b"secret2".to_vec()));

    let dumped = serde_json::to_value(&event).expect("dump");

    assert!(dumped.get("stdout_bytes").is_none(), "{dumped}");
    assert!(dumped.get("stderr_bytes").is_none(), "{dumped}");
    assert!(!dumped.to_string().contains("secret"), "{dumped}");
}

/// `test_file_entry_is_dir_uses_kind`.
#[test]
fn an_entry_is_a_directory_by_its_kind() {
    let directory = FileEntry::new(
        "/workspace/dir",
        Permissions::from_str_mode("drwxr-xr-x").expect("mode"),
    )
    .with_ownership("root", "root")
    .with_kind(EntryKind::Directory);
    let file = FileEntry::new(
        "/workspace/file.txt",
        Permissions::from_str_mode("-rw-r--r--").expect("mode"),
    )
    .with_ownership("root", "root")
    .with_size(3)
    .with_kind(EntryKind::File);

    assert!(directory.is_dir());
    assert!(!file.is_dir());
}

// --- shaping a command -------------------------------------------------------------------------

/// `test_exec_shell_true_quotes_multi_arg_commands`: several arguments are quoted into one command
/// line for the login shell, the way Python's `shlex.join` quotes them.
#[test]
fn several_arguments_are_quoted_into_one_login_shell_command_line() {
    let shaped = prepare_exec_command(&ExecRequest::new(
        ["printf", "%s\n", "hello world", "$(whoami)", "semi;colon"].map(str::to_owned),
    ));

    assert_eq!(
        shaped,
        [
            "sh",
            "-lc",
            "printf '%s\n' 'hello world' '$(whoami)' 'semi;colon'"
        ]
    );
}

/// `test_exec_shell_true_preserves_single_shell_snippet`.
#[test]
fn a_single_shell_snippet_is_passed_through_as_it_is() {
    let shaped = prepare_exec_command(&ExecRequest::new(["echo hello && echo goodbye".to_owned()]));

    assert_eq!(shaped, ["sh", "-lc", "echo hello && echo goodbye"]);
}

// --- classifying a failed read ---------------------------------------------------------------------

/// The read command the error reports, as the reference writes it.
fn read_command() -> Vec<String> {
    ["sh", "-lc", "<read_access_check>", "/workspace/target.txt"]
        .map(str::to_owned)
        .to_vec()
}

fn failed_read(stdout: &[u8], exit_code: i32) -> ExecResult {
    ExecResult::new(stdout.to_vec(), b"not readable".to_vec(), exit_code)
}

/// `test_check_read_with_exec_classifies_failure_as_requested_user`: after a read the account was
/// refused, the probe runs as that account, with no shell of the session's own and a ten-second
/// limit; only its exit of 1 means the file is missing.
#[tokio::test]
async fn a_refused_read_is_classified_by_a_probe_run_as_the_same_account() {
    for (probe_exit, expected) in [
        (0, ErrorCode::WorkspaceArchiveReadError),
        (1, ErrorCode::WorkspaceReadNotFound),
        (2, ErrorCode::WorkspaceArchiveReadError),
    ] {
        let session = MemorySession::empty();
        session.answer_exec_with(ExecResult::new(Vec::new(), Vec::new(), probe_exit));

        let error = read_error_from_exec(
            session.as_ref(),
            "target.txt",
            "/workspace/target.txt",
            read_command(),
            &failed_read(b"", 1),
            Some(User::new("sandbox-user")),
        )
        .await;

        assert_eq!(error.error_code(), expected, "probe exit {probe_exit}");
        let requests = session.exec_requests.lock().expect("requests").clone();
        assert_eq!(requests.len(), 1);
        let probe = &requests[0];
        assert_eq!(probe.command()[..2], ["sh", "-c"]);
        assert!(probe.command()[2].starts_with("# READ_PATH_PROBE_V3\n"));
        assert_eq!(probe.command()[3..], ["sh", "/workspace/target.txt"]);
        assert_eq!(probe.shell(), &ShellInvocation::None);
        assert_eq!(probe.user().cloned(), Some(User::new("sandbox-user")));
        assert_eq!(probe.timeout_s(), Some(10.0));
        assert_eq!(error.context()["existence_probe_exit_code"], probe_exit);
    }
}

/// `test_check_read_with_exec_treats_nonstandard_check_exit_as_archive_error`: a check that did not
/// answer "no" is a failure to ask, and nothing more is run.
#[tokio::test]
async fn a_check_that_failed_in_an_unexpected_way_is_not_probed() {
    let session = MemorySession::empty();

    let error = read_error_from_exec(
        session.as_ref(),
        "target.txt",
        "/workspace/target.txt",
        read_command(),
        &ExecResult::new(Vec::new(), b"check failed".to_vec(), 127),
        None,
    )
    .await;

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    assert!(session.exec_requests.lock().expect("requests").is_empty());
    assert!(!error.context().contains_key("existence_probe_exit_code"));
}

/// `test_read_error_context_does_not_retain_partial_stdout`: what the failed read printed is
/// counted, never kept.
#[tokio::test]
async fn a_failed_reads_partial_output_is_counted_not_kept() {
    let partial = b"sensitive partial contents".repeat(1024);
    let session = MemorySession::empty();
    session.answer_exec_with(ExecResult::new(Vec::new(), Vec::new(), 2));

    let error = read_error_from_exec(
        session.as_ref(),
        "target.txt",
        "/workspace/target.txt",
        read_command(),
        &failed_read(&partial, 1),
        None,
    )
    .await;

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    assert!(!error.context().contains_key("stdout"));
    assert_eq!(error.context()["stdout_bytes"], partial.len());
    assert!(!format!("{error:?}").contains("sensitive partial contents"));
}

// --- the probe script itself -----------------------------------------------------------------------

/// The probe script exactly as a session sends it, taken from the request it makes.
async fn probe_script() -> String {
    let session = MemorySession::empty();
    session.answer_exec_with(ExecResult::new(Vec::new(), Vec::new(), 1));
    let _ = read_error_from_exec(
        session.as_ref(),
        "target.txt",
        "/workspace/target.txt",
        read_command(),
        &failed_read(b"", 1),
        None,
    )
    .await;
    let requests = session.exec_requests.lock().expect("requests").clone();
    requests[0].command()[2].clone()
}

/// Runs the probe on `path` with `sh`, as the session does, and answers its exit status.
fn probe(script: &str, path: &Path, search_path: Option<&str>) -> i32 {
    let mut command = Command::new("sh");
    command.arg("-c").arg(script).arg("sh").arg(path);
    if let Some(search_path) = search_path {
        command.env("PATH", search_path);
    }
    command
        .output()
        .expect("the probe ran")
        .status
        .code()
        .expect("an exit status")
}

/// `test_read_path_probe_resolves_symlinks_before_classifying_missing`: a path is missing (1) only
/// when resolving it shows nothing is there; a link to nothing is missing, a link whose resolution
/// fails — a loop, a file used as a directory, too many links, a name too long — is not known to
/// be missing (2); glob characters are names, not patterns; and a failing or lying `find` never
/// makes a path missing.
#[tokio::test]
async fn the_probe_resolves_links_before_it_calls_a_path_missing() {
    let script = probe_script().await;
    let directory = tempfile::tempdir().expect("temp");
    let top = std::fs::canonicalize(directory.path()).expect("resolve");
    let workspace = top.join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let link = |name: &str, target: &str| {
        std::os::unix::fs::symlink(target, workspace.join(name)).expect("link");
    };
    link("dangling", "missing");
    std::fs::write(workspace.join("not-a-directory"), "content").expect("write");
    link("invalid-target", "not-a-directory/child");
    link("dangling-parent", "missing-directory");
    link("invalid-parent", "not-a-directory");
    link("loop", "loop");
    std::fs::write(workspace.join("newline-target\n"), "content").expect("write");
    link("newline-link", "newline-target\n");
    std::fs::write(workspace.join("a"), "sibling").expect("write");
    let mut chain = workspace.clone();
    let mut through_links = workspace.clone();
    for index in 0..41 {
        let real = chain.join(format!("real-{index}"));
        std::fs::create_dir(&real).expect("real");
        std::os::unix::fs::symlink(format!("real-{index}"), chain.join(format!("link-{index}")))
            .expect("chain link");
        through_links = through_links.join(format!("link-{index}"));
        chain = real;
    }

    let cases: [(PathBuf, i32); 11] = [
        (workspace.join("dangling"), 1),
        (workspace.join("invalid-target"), 2),
        (workspace.join("dangling-parent/child"), 1),
        (workspace.join("invalid-parent/child"), 2),
        (workspace.join("loop"), 2),
        (workspace.join("newline-link"), 0),
        (workspace.join("[a]"), 1),
        (workspace.join("?"), 1),
        (workspace.join("*"), 1),
        (through_links.join("missing"), 2),
        (workspace.join("x".repeat(256)), 2),
    ];
    for (path, expected) in &cases {
        assert_eq!(probe(&script, path, None), *expected, "{}", path.display());
    }

    // A `find` that fails is not an answer, and neither is one that prints something unexpected.
    let bin = top.join("fake-bin");
    std::fs::create_dir(&bin).expect("bin");
    let log = top.join("find-args.log");
    let find = bin.join("find");
    std::fs::write(
        &find,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'find: %s: Input/output error\\n' \"$1\" >&2\nexit 1\n",
            log.display()
        ),
    )
    .expect("find");
    std::fs::set_permissions(&find, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("mode");
    let search_path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let missing = workspace.join("missing");
    assert_eq!(probe(&script, &missing, Some(&search_path)), 2);
    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("find was asked")
            .lines()
            .collect::<Vec<_>>(),
        [missing.to_string_lossy().as_ref(), "-prune", "-print"]
    );
    std::fs::write(&find, "#!/bin/sh\nprintf match\n").expect("find");
    assert_eq!(probe(&script, &missing, Some(&search_path)), 2);
}
