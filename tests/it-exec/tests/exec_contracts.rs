//! Integration tests for ExecRequest, ExecLimits, StdinCommand, and RootedFileSystem.

use std::{fs as std_fs, path::Path, time::Duration};

use ra_core::{compat::SchemaVersion, event::exec::ExecStreamKind};
use ra_exec::{
    EXEC_SCHEMA_VERSION,
    command::{ExecCursor, ExecLimits, ExecRequest},
    fs::{RootedFileSystem, RootedOpenError},
    output::ExecOutputSummary,
    pty::{StdinCommand, StdinValidationError, TerminalMode},
    session::ExecSessionId,
};
use serde_json::json;
use tempfile::tempdir;

#[test]
fn test_exec_request_and_limits_construction() {
    let req = ExecRequest::new("npm test")
        .with_args(["--", "--watch=false"])
        .with_cwd("/src")
        .with_env("NODE_ENV", "test")
        .with_pty(true);

    assert_eq!(req.schema_version(), EXEC_SCHEMA_VERSION);
    assert_eq!(req.command(), "npm test");
    assert_eq!(req.args(), &["--", "--watch=false"]);
    assert_eq!(req.cwd(), Some(&Path::new("/src").to_path_buf()));
    assert_eq!(req.env().get("NODE_ENV"), Some(&"test".to_owned()));
    assert!(req.pty());
    assert!(req.unknown().is_empty());

    let default_limits = req.limits();
    assert_eq!(default_limits.schema_version(), EXEC_SCHEMA_VERSION);
    assert_eq!(
        default_limits.initial_yield_timeout(),
        Duration::from_secs(10)
    );
    assert_eq!(default_limits.max_capture_bytes(), 1024 * 1024);
    assert_eq!(default_limits.max_sessions(), 64);
    assert!(default_limits.unknown().is_empty());

    let custom_limits = ExecLimits::new()
        .with_initial_yield_timeout(Duration::from_secs(5))
        .with_max_capture_bytes(512 * 1024)
        .with_idle_timeout(Some(Duration::from_secs(60)))
        .with_total_timeout(Some(Duration::from_secs(300)))
        .with_max_sessions(16);

    let req2 = req.with_limits(custom_limits);
    assert_eq!(
        req2.limits().initial_yield_timeout(),
        Duration::from_secs(5)
    );
    assert_eq!(req2.limits().max_sessions(), 16);
}

#[test]
fn test_exec_cursor_progression() {
    let cursor = ExecCursor::stdout_start();
    assert_eq!(cursor.offset(), 0);
    assert_eq!(cursor.stream(), ExecStreamKind::Stdout);

    let advanced = cursor.advance(1024);
    assert_eq!(advanced.offset(), 1024);
    assert_eq!(advanced.stream(), ExecStreamKind::Stdout);
}

#[test]
fn test_exec_cursor_saturates_instead_of_moving_backwards() {
    // A wrapped offset would put the cursor behind where it already was, and the reader that
    // resumed from it would re-deliver output as if it had been produced twice.
    let near_end = ExecCursor::new(ExecStreamKind::Stdout, u64::MAX - 4);

    let saturated = near_end.advance(64);
    assert_eq!(saturated.offset(), u64::MAX);
    assert!(saturated.offset() >= near_end.offset());
    assert_eq!(saturated.stream(), ExecStreamKind::Stdout);

    // Already at the end: still no wrap, and still no panic in a debug build.
    assert_eq!(saturated.advance(1).offset(), u64::MAX);
    assert_eq!(
        ExecCursor::new(ExecStreamKind::Stderr, u64::MAX)
            .advance(u64::MAX)
            .offset(),
        u64::MAX
    );
}

#[test]
fn test_stdin_command_validation_modes() {
    let session_id = ExecSessionId::new("exec-pty-test");

    // Pty mode accepts character input, poll, and interrupt
    let write_cmd = StdinCommand::write(session_id.clone(), "ls -la\n");
    assert_eq!(write_cmd.schema_version(), EXEC_SCHEMA_VERSION);
    assert_eq!(write_cmd.validate(TerminalMode::Pty), Ok(()));
    assert_eq!(write_cmd.chars(), Some("ls -la\n"));
    assert!(!write_cmd.is_interrupt());
    assert!(write_cmd.unknown().is_empty());

    let poll_cmd = StdinCommand::poll(session_id.clone());
    assert_eq!(poll_cmd.validate(TerminalMode::Pty), Ok(()));
    assert_eq!(poll_cmd.chars(), None);

    let interrupt_cmd = StdinCommand::interrupt(session_id.clone());
    assert_eq!(interrupt_cmd.validate(TerminalMode::Pty), Ok(()));
    assert!(interrupt_cmd.is_interrupt());

    // Pipe mode rejects non-empty character input
    assert_eq!(
        write_cmd.validate(TerminalMode::Pipe),
        Err(StdinValidationError::NonTtyInputRejected)
    );

    // Pipe mode accepts poll and interrupt
    assert_eq!(poll_cmd.validate(TerminalMode::Pipe), Ok(()));
    assert_eq!(interrupt_cmd.validate(TerminalMode::Pipe), Ok(()));
}

#[test]
fn test_exec_output_summary() {
    let summary = ExecOutputSummary::new("test output\n", "some warnings\n")
        .with_duration(Duration::from_millis(250))
        .with_exit_code(0);

    assert_eq!(summary.schema_version(), EXEC_SCHEMA_VERSION);
    assert_eq!(summary.stdout(), "test output\n");
    assert_eq!(summary.stderr(), "some warnings\n");
    assert_eq!(summary.stdout_bytes(), 12);
    assert_eq!(summary.stderr_bytes(), 14);
    assert_eq!(summary.total_bytes(), 26);
    assert_eq!(summary.duration(), Duration::from_millis(250));
    assert_eq!(summary.duration_ms(), 250);
    assert_eq!(summary.exit_code(), Some(0));
    assert!(!summary.is_truncated());
    assert!(summary.unknown().is_empty());
}

#[test]
fn test_exec_output_summary_total_saturates_instead_of_wrapping() {
    // The two counts are set independently, so nothing upstream bounds their sum. A wrapped total
    // would report less output than either stream alone — a plausible-looking number rather than a
    // visible fault.
    let summary = ExecOutputSummary::new("", "")
        .with_stdout_bytes(usize::MAX - 3)
        .with_stderr_bytes(64);

    assert_eq!(summary.total_bytes(), usize::MAX);
    assert!(summary.total_bytes() >= summary.stdout_bytes());
    assert!(summary.total_bytes() >= summary.stderr_bytes());

    let both_max = ExecOutputSummary::new("", "")
        .with_stdout_bytes(usize::MAX)
        .with_stderr_bytes(usize::MAX);
    assert_eq!(both_max.total_bytes(), usize::MAX);

    // A summary deserialized from a record this process did not write reaches the same accessor.
    let from_wire: ExecOutputSummary = serde_json::from_value(json!({
        "stdout": "",
        "stderr": "",
        "stdout_bytes": usize::MAX,
        "stderr_bytes": usize::MAX,
        "duration_ms": 0,
        "is_truncated": false
    }))
    .expect("must deserialize");
    assert_eq!(from_wire.total_bytes(), usize::MAX);
}

#[test]
fn test_exec_contracts_forward_compatibility() {
    // 1. ExecRequest with future fields and custom limits
    let future_req_payload = json!({
        "schema_version": 2,
        "command": "cargo build",
        "limits": {
            "schema_version": 2,
            "initial_yield_timeout_ms": 5000,
            "max_capture_bytes": 65536,
            "max_sessions": 32,
            "cgroup_memory_max": 2147483648_u64
        },
        "network_isolation_tier": "sandbox_strict"
    });

    let req: ExecRequest =
        serde_json::from_value(future_req_payload).expect("must deserialize future ExecRequest");
    assert_eq!(req.schema_version(), SchemaVersion::new(2));
    assert_eq!(
        req.unknown().get("network_isolation_tier"),
        Some(&json!("sandbox_strict"))
    );
    assert_eq!(
        req.limits().unknown().get("cgroup_memory_max"),
        Some(&json!(2147483648_u64))
    );

    let req_reserialized = serde_json::to_value(&req).expect("must reserialize ExecRequest");
    assert_eq!(req_reserialized["network_isolation_tier"], "sandbox_strict");
    assert_eq!(
        req_reserialized["limits"]["cgroup_memory_max"],
        2147483648_u64
    );

    // 2. ExecOutputSummary with future telemetry
    let future_summary_payload = json!({
        "schema_version": 2,
        "stdout": "done",
        "stderr": "",
        "stdout_bytes": 4,
        "stderr_bytes": 0,
        "duration_ms": 120,
        "is_truncated": false,
        "peak_rss_bytes": 52428800
    });

    let summary: ExecOutputSummary = serde_json::from_value(future_summary_payload)
        .expect("must deserialize future ExecOutputSummary");
    assert_eq!(summary.schema_version(), SchemaVersion::new(2));
    assert_eq!(
        summary.unknown().get("peak_rss_bytes"),
        Some(&json!(52428800))
    );

    let summary_reserialized =
        serde_json::to_value(&summary).expect("must reserialize ExecOutputSummary");
    assert_eq!(summary_reserialized["peak_rss_bytes"], 52428800);

    // 3. StdinCommand with future priority
    let future_stdin_payload = json!({
        "schema_version": 2,
        "session_id": "exec-10",
        "chars": "y\n",
        "is_interrupt": false,
        "flush_mode": "immediate"
    });

    let stdin_cmd: StdinCommand =
        serde_json::from_value(future_stdin_payload).expect("must deserialize future StdinCommand");
    assert_eq!(stdin_cmd.schema_version(), SchemaVersion::new(2));
    assert_eq!(
        stdin_cmd.unknown().get("flush_mode"),
        Some(&json!("immediate"))
    );

    let stdin_reserialized =
        serde_json::to_value(&stdin_cmd).expect("must reserialize StdinCommand");
    assert_eq!(stdin_reserialized["flush_mode"], "immediate");
}

#[test]
fn test_rooted_file_system_operations_and_containment() {
    let temp = tempdir().expect("must create tempdir");
    let fs = RootedFileSystem::open(temp.path()).expect("must open rooted fs");

    // Write file inside root
    let test_file = Path::new("sub/dir/test.txt");
    assert!(!fs.exists(test_file));
    fs.write_file(test_file, b"Hello Rooted World!")
        .expect("must write file");
    assert!(fs.exists(test_file));

    // Read file with limit
    let (content, truncated) = fs
        .read_to_string(test_file, 1024)
        .expect("must read string");
    assert_eq!(content, "Hello Rooted World!");
    assert!(!truncated);

    // Boundary condition: read_to_string with usize::MAX must not overflow or panic
    let (max_content, max_truncated) = fs
        .read_to_string(test_file, usize::MAX)
        .expect("must read with usize::MAX limit without overflow");
    assert_eq!(max_content, "Hello Rooted World!");
    assert!(!max_truncated);

    // Read with small limit triggering truncation
    let (short_content, is_short_truncated) =
        fs.read_to_string(test_file, 5).expect("must read prefix");
    assert_eq!(short_content, "Hello");
    assert!(is_short_truncated);

    // Remove file
    fs.remove_file(test_file).expect("must remove file");
    assert!(!fs.exists(test_file));

    // Boundary check: absolute or escaping paths must be rejected without syscall escape
    let escaping_path = Path::new("../outside.txt");
    let err = fs.open_read(escaping_path).unwrap_err();
    assert!(matches!(err, RootedOpenError::OutsideRoot));

    let absolute_path = Path::new("/etc/passwd");
    let err2 = fs.open_read(absolute_path).unwrap_err();
    assert!(matches!(err2, RootedOpenError::OutsideRoot));
}

#[test]
fn test_rooted_file_system_symlink_escape_protection() {
    let temp = tempdir().expect("must create tempdir");
    let workspace_dir = temp.path().join("workspace");
    let outside_dir = temp.path().join("outside");
    std_fs::create_dir(&workspace_dir).expect("must create workspace dir");
    std_fs::create_dir(&outside_dir).expect("must create outside dir");

    let outside_target = outside_dir.join("secret.txt");
    std_fs::write(&outside_target, b"secret content").expect("must write outside target");

    // Create a symlink inside workspace pointing outside
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let symlink_in_ws = workspace_dir.join("symlink_to_outside");
        symlink(&outside_dir, &symlink_in_ws).expect("must create symlink");

        let fs = RootedFileSystem::open(&workspace_dir).expect("must open rooted fs");

        let escaping_file = Path::new("symlink_to_outside/secret.txt");
        assert!(matches!(
            fs.open_read(escaping_file),
            Err(RootedOpenError::OutsideRoot)
        ));
        assert!(matches!(
            fs.write_file(escaping_file, b"overwrite"),
            Err(RootedOpenError::OutsideRoot)
        ));
        assert!(matches!(
            fs.remove_file(escaping_file),
            Err(RootedOpenError::OutsideRoot)
        ));
    }
}

#[test]
fn test_rooted_file_system_read_to_string_multibyte_utf8_truncation() {
    let temp = tempdir().expect("must create tempdir");
    let fs = RootedFileSystem::open(temp.path()).expect("must open rooted fs");

    let file_path = Path::new("multibyte.txt");
    // "Hello, " is 7 bytes
    // "世界" is 6 bytes (3 bytes each)
    // "🦀" is 4 bytes
    // Total = 17 bytes
    fs.write_file(file_path, "Hello, 世界🦀".as_bytes())
        .expect("must write multibyte file");

    // 1. Read entire file
    let (full, tr_full) = fs.read_to_string(file_path, 100).expect("read full");
    assert_eq!(full, "Hello, 世界🦀");
    assert!(!tr_full);

    // 2. Truncate at 8 bytes (cuts inside the first multibyte character "世")
    let (c8, tr8) = fs.read_to_string(file_path, 8).expect("read 8");
    assert!(tr8);
    assert_eq!(c8, "Hello, "); // cleanly sliced at character boundary, no \u{FFFD}

    // 3. Truncate at 15 bytes (cuts inside the 4-byte emoji "🦀")
    let (c15, tr15) = fs.read_to_string(file_path, 15).expect("read 15");
    assert!(tr15);
    assert_eq!(c15, "Hello, 世界"); // cleanly sliced before broken emoji
}
