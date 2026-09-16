//! `ra-core::sandbox`: the value types and the error contract, carried over from the reference
//! implementation's `sandbox/types.py` and `sandbox/errors.py`.
//!
//! What is pinned here is behavior the reference states outright and that a port can silently get
//! wrong:
//! - retryability has **three** answers, and an unclassified failure inherits the one it wraps
//! - a broadly-scoped failure stays unclassified rather than defaulting to "no"
//! - permissions are a value: equal by mode, usable as a map key
//! - every error code's wire string is what a host branches on, so it is asserted literally
//!
//! The reference's own tests for hashability exist because pydantic drops `__hash__` when `__eq__`
//! is overridden. Rust has no such trap, but the property those tests protect — these are value
//! types that work as keys — is the thing worth keeping, so it is asserted directly.

use std::collections::{HashMap, HashSet};

use ra_core::sandbox::{
    ErrorCategory, ErrorCode, ExecResult, ExposedPortEndpoint, FileMode, Group, OpName,
    Permissions, SandboxError, SandboxErrorDetails, User,
};

// --- retryability -------------------------------------------------------------------------

#[test]
fn retryable_can_be_set_explicitly() {
    let error = SandboxError::new(
        ErrorCode::ExecTransportError,
        OpName::Exec,
        "backend is unavailable",
    )
    .with_retryable(Some(true));

    assert_eq!(error.retryable(), Some(true));
}

#[test]
fn a_wrapping_error_inherits_retryability_from_its_cause() {
    // A stop that failed because an archive read failed is exactly as retryable as that read. The
    // outer layer has no independent way to tell, so it must not answer on its own.
    let cause = SandboxError::new(
        ErrorCode::WorkspaceArchiveReadError,
        OpName::Read,
        "could not read workspace archive",
    )
    .with_retryable(Some(false));

    let error = SandboxError::new(
        ErrorCode::WorkspaceStopError,
        OpName::Stop,
        "could not persist workspace",
    )
    .with_sandbox_cause(cause);

    assert_eq!(error.retryable(), Some(false));
}

#[test]
fn an_error_that_knows_its_own_retryability_keeps_it_when_wrapping() {
    // Inheritance fills a gap; it does not overwrite a decision the raiser already made.
    let cause = SandboxError::new(
        ErrorCode::WorkspaceArchiveReadError,
        OpName::Read,
        "could not read workspace archive",
    )
    .with_retryable(Some(true));

    let error = SandboxError::new(
        ErrorCode::WorkspaceStopError,
        OpName::Stop,
        "could not persist workspace",
    )
    .with_retryable(Some(false))
    .with_sandbox_cause(cause);

    assert_eq!(error.retryable(), Some(false));
}

#[test]
fn deterministic_failures_are_not_retryable() {
    for code in [
        ErrorCode::WorkspaceReadNotFound,
        ErrorCode::WorkspaceWriteTypeError,
        ErrorCode::ExecTimeout,
    ] {
        assert_eq!(
            SandboxError::new(code, OpName::Exec, "boom").retryable(),
            Some(false),
            "{code} must be non-retryable"
        );
    }
}

#[test]
fn broadly_scoped_failures_stay_unclassified() {
    // These cover both a transient fault and a permanent one. Answering "no" would turn every
    // transient clone failure into a permanent one; answering "yes" would spin forever on a
    // repository that does not exist.
    for code in [
        ErrorCode::WorkspaceArchiveReadError,
        ErrorCode::GitCloneError,
        ErrorCode::GitCopyError,
        ErrorCode::SnapshotPersistError,
        ErrorCode::SnapshotRestoreError,
    ] {
        assert_eq!(
            SandboxError::new(code, OpName::Materialize, "boom").retryable(),
            None,
            "{code} must stay unclassified"
        );
    }
}

#[test]
fn a_non_sandbox_cause_does_not_supply_retryability() {
    let error = SandboxError::new(
        ErrorCode::GitCloneError,
        OpName::Materialize,
        "clone failed",
    )
    .with_cause(std::io::Error::other("connection reset"));

    assert_eq!(error.retryable(), None);
    assert!(std::error::Error::source(&error).is_some());
}

// --- codes --------------------------------------------------------------------------------

/// Every code, so the assertions below cannot silently skip one that was added later.
const ALL_CODES: [ErrorCode; 33] = [
    ErrorCode::InvalidManifestPath,
    ErrorCode::InvalidCompressionScheme,
    ErrorCode::ExposedPortUnavailable,
    ErrorCode::ExecNonzero,
    ErrorCode::ExecTimeout,
    ErrorCode::ExecTransportError,
    ErrorCode::PtySessionNotFound,
    ErrorCode::ApplyPatchInvalidPath,
    ErrorCode::ApplyPatchInvalidDiff,
    ErrorCode::ApplyPatchFileNotFound,
    ErrorCode::ApplyPatchDecodeError,
    ErrorCode::WorkspaceReadNotFound,
    ErrorCode::WorkspaceArchiveReadError,
    ErrorCode::WorkspaceArchiveWriteError,
    ErrorCode::WorkspaceWriteTypeError,
    ErrorCode::WorkspaceStopError,
    ErrorCode::WorkspaceStartError,
    ErrorCode::WorkspaceRootNotFound,
    ErrorCode::LocalFileReadError,
    ErrorCode::LocalDirReadError,
    ErrorCode::LocalChecksumError,
    ErrorCode::GitMissingInImage,
    ErrorCode::GitCloneError,
    ErrorCode::GitSubpathError,
    ErrorCode::GitCopyError,
    ErrorCode::MountMissingTool,
    ErrorCode::MountFailed,
    ErrorCode::MountConfigInvalid,
    ErrorCode::SkillsConfigInvalid,
    ErrorCode::SandboxConfigInvalid,
    ErrorCode::SnapshotPersistError,
    ErrorCode::SnapshotRestoreError,
    ErrorCode::SnapshotNotRestorable,
];

#[test]
fn code_wire_strings_match_the_reference_exactly() {
    // These strings cross process boundaries. A rename is a breaking change, so they are spelled
    // out rather than derived from the variant name.
    let expected = [
        "invalid_manifest_path",
        "invalid_compression_scheme",
        "exposed_port_unavailable",
        "exec_nonzero",
        "exec_timeout",
        "exec_transport_error",
        "pty_session_not_found",
        "apply_patch_invalid_path",
        "apply_patch_invalid_diff",
        "apply_patch_file_not_found",
        "apply_patch_decode_error",
        "workspace_read_not_found",
        "workspace_archive_read_error",
        "workspace_archive_write_error",
        "workspace_write_type_error",
        "workspace_stop_error",
        "workspace_start_error",
        "workspace_root_not_found",
        "local_file_read_error",
        "local_dir_read_error",
        "local_checksum_error",
        "git_missing_in_image",
        "git_clone_error",
        "git_subpath_error",
        "git_copy_error",
        "mount_missing_tool",
        "mount_failed",
        "mount_config_invalid",
        "skills_config_invalid",
        "sandbox_config_invalid",
        "snapshot_persist_error",
        "snapshot_restore_error",
        "snapshot_not_restorable",
    ];

    for (code, wire) in ALL_CODES.iter().zip(expected) {
        assert_eq!(code.as_str(), wire);
        assert_eq!(code.to_string(), wire);
    }
}

#[test]
fn code_wire_strings_survive_a_serde_round_trip() {
    for code in ALL_CODES {
        let json = serde_json::to_string(&code).expect("serialize");
        assert_eq!(json, format!("\"{}\"", code.as_str()));
        let back: ErrorCode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, code);
    }
}

#[test]
fn every_code_has_exactly_one_wire_string_and_one_category() {
    let wires: HashSet<&str> = ALL_CODES.iter().map(|code| code.as_str()).collect();
    assert_eq!(wires.len(), ALL_CODES.len(), "wire strings must be unique");

    // The point of deriving the category is that no code can be filed under two families or none;
    // the match in `category()` is exhaustive, so this only has to confirm the grouping is total.
    for code in ALL_CODES {
        let category = code.category();
        assert!(matches!(
            category,
            ErrorCategory::Configuration
                | ErrorCategory::Runtime
                | ErrorCategory::Artifact
                | ErrorCategory::Snapshot
        ));
    }
}

#[test]
fn op_names_match_the_reference_exactly() {
    let expected = [
        (OpName::Start, "start"),
        (OpName::Stop, "stop"),
        (OpName::Exec, "exec"),
        (OpName::Read, "read"),
        (OpName::Write, "write"),
        (OpName::Shutdown, "shutdown"),
        (OpName::Running, "running"),
        (OpName::PersistWorkspace, "persist_workspace"),
        (OpName::HydrateWorkspace, "hydrate_workspace"),
        (OpName::ResolveExposedPort, "resolve_exposed_port"),
        (OpName::Materialize, "materialize"),
        (OpName::SnapshotPersist, "snapshot_persist"),
        (OpName::SnapshotRestore, "snapshot_restore"),
        (OpName::ApplyPatch, "apply_patch"),
    ];

    for (op, wire) in expected {
        assert_eq!(op.as_str(), wire);
        assert_eq!(
            serde_json::to_string(&op).expect("serialize"),
            format!("\"{wire}\"")
        );
    }
}

#[test]
fn context_is_carried_and_ordered() {
    let error = SandboxError::new(
        ErrorCode::InvalidManifestPath,
        OpName::Materialize,
        "manifest path must be relative: /etc/passwd",
    )
    .with_context("rel", "/etc/passwd")
    .with_context("reason", "absolute");

    // Ordered, so two renderings of the same failure compare equal.
    let keys: Vec<&str> = error.context().keys().map(String::as_str).collect();
    assert_eq!(keys, ["reason", "rel"]);
    assert_eq!(error.op(), OpName::Materialize);
    assert_eq!(error.category(), ErrorCategory::Configuration);
    assert_eq!(
        error.message(),
        "manifest path must be relative: /etc/passwd"
    );
}

// --- value types --------------------------------------------------------------------------

#[test]
fn permissions_are_a_value_usable_as_a_key() {
    let perms = Permissions::from_mode(0o755);
    let other = Permissions::from_mode(0o755);
    let different = Permissions::from_mode(0o644);

    assert_eq!(perms, other);
    assert_ne!(perms, different);

    let set: HashSet<Permissions> = [perms, other, different].into_iter().collect();
    assert_eq!(set.len(), 2);

    let map: HashMap<Permissions, &str> = [(perms, "value")].into_iter().collect();
    assert_eq!(map.get(&other), Some(&"value"));
}

#[test]
fn users_and_groups_are_identified_by_name_alone() {
    assert_eq!(User::new("alice"), User::new("alice"));

    // Membership is data about the group, not part of its identity: a view taken before a member
    // joined and one taken after are the same group.
    let empty = Group::new("admin", Vec::new());
    let populated = Group::new("admin", vec![User::new("alice")]);
    assert_eq!(empty, populated);

    let set: HashSet<Group> = [empty, populated].into_iter().collect();
    assert_eq!(set.len(), 1);
}

#[test]
fn a_mode_round_trips_through_the_triplets() {
    for mode in [0o755, 0o644, 0o600, 0o777, 0o000] {
        assert_eq!(Permissions::from_mode(mode).to_mode(), mode);
    }

    let dir = Permissions::from_mode(0o040_755);
    assert!(dir.directory);
    assert_eq!(dir.to_mode(), 0o040_755);
}

#[test]
fn the_default_permission_is_owner_only() {
    assert_eq!(Permissions::default().to_mode(), 0o700);
}

#[test]
fn builders_set_one_triplet_each() {
    let perms = Permissions::default()
        .owner_can(FileMode::All)
        .group_can(FileMode::Read)
        .others_can(FileMode::None);

    assert_eq!(perms.to_mode(), 0o740);
}

#[test]
fn a_mode_field_parses_the_way_ls_prints_it() {
    assert_eq!(
        Permissions::from_str_mode("-rwxr-xr-x").expect("parse"),
        Permissions::from_mode(0o755)
    );
    assert_eq!(
        Permissions::from_str_mode("drwx------").expect("parse"),
        Permissions::from_mode(0o040_700)
    );

    // setuid, setgid and sticky occupy the execute position. The lowercase forms also mean the
    // execute bit is set; the uppercase ones mean it is not. Only the execute bit differs between
    // these two fields, which is the whole point of the pair.
    assert_eq!(
        Permissions::from_str_mode("-rwsrwsrwt")
            .expect("parse")
            .to_mode(),
        0o777
    );
    assert_eq!(
        Permissions::from_str_mode("-rwSrwSrwT")
            .expect("parse")
            .to_mode(),
        0o666
    );
}

#[test]
fn an_access_method_marker_is_not_a_permission_bit() {
    // `ls` appends one marker for an ACL, macOS extended attributes, or an SELinux context. Left
    // in place it would fail the length check on files that are perfectly ordinary.
    for marked in ["-rw-r--r--+", "-rw-r--r--@", "-rw-r--r--."] {
        assert_eq!(
            Permissions::from_str_mode(marked).expect("parse"),
            Permissions::from_mode(0o644),
            "{marked} must parse"
        );
    }
}

#[test]
fn a_malformed_mode_field_is_refused() {
    for bad in ["-rwxr-xr", "xrwxr-xr-x", "-rwzr-xr-x", ""] {
        assert!(
            Permissions::from_str_mode(bad).is_err(),
            "{bad:?} must be refused"
        );
    }
}

#[test]
fn permissions_render_the_way_they_parse() {
    for field in ["-rwxr-xr-x", "drwx------", "-rw-r--r--"] {
        let parsed = Permissions::from_str_mode(field).expect("parse");
        assert_eq!(parsed.to_string(), field);
    }
}

#[test]
fn an_exec_result_keeps_its_two_streams_apart() {
    let result = ExecResult::new(b"out".to_vec(), b"err".to_vec(), 0);
    assert!(result.ok());
    assert_eq!(result.stdout, b"out");
    assert_eq!(result.stderr, b"err");

    assert!(!ExecResult::new(Vec::new(), Vec::new(), 1).ok());
}

#[test]
fn an_endpoint_builds_urls_for_the_two_schemes_it_serves() {
    let plain = ExposedPortEndpoint::new("example.test", 8080);
    assert_eq!(
        plain.url_for("http").expect("http"),
        "http://example.test:8080/"
    );
    assert_eq!(plain.url_for("ws").expect("ws"), "ws://example.test:8080/");

    let secure = ExposedPortEndpoint::new("example.test", 8443).with_tls(true);
    assert_eq!(
        secure.url_for("http").expect("https"),
        "https://example.test:8443/"
    );
    assert_eq!(
        secure.url_for("ws").expect("wss"),
        "wss://example.test:8443/"
    );

    // Case is not significant.
    assert_eq!(
        plain.url_for("HTTP").expect("http"),
        "http://example.test:8080/"
    );

    assert!(plain.url_for("ftp").is_err());
}

#[test]
fn a_default_port_is_left_out_of_the_url() {
    assert_eq!(
        ExposedPortEndpoint::new("example.test", 80)
            .url_for("http")
            .expect("http"),
        "http://example.test/"
    );
    assert_eq!(
        ExposedPortEndpoint::new("example.test", 443)
            .with_tls(true)
            .url_for("http")
            .expect("https"),
        "https://example.test/"
    );
    // 80 is not the default once TLS is on, so it stays.
    assert_eq!(
        ExposedPortEndpoint::new("example.test", 80)
            .with_tls(true)
            .url_for("http")
            .expect("https"),
        "https://example.test:80/"
    );
}

#[test]
fn an_ipv6_host_is_bracketed_once() {
    assert_eq!(
        ExposedPortEndpoint::new("::1", 8080)
            .url_for("http")
            .expect("http"),
        "http://[::1]:8080/"
    );
    // Already bracketed input is left alone rather than nested.
    assert_eq!(
        ExposedPortEndpoint::new("[::1]", 8080)
            .url_for("http")
            .expect("http"),
        "http://[::1]:8080/"
    );
}

#[test]
fn a_query_is_appended_with_exactly_one_question_mark() {
    let with_mark = ExposedPortEndpoint::new("example.test", 8080).with_query("?token=abc");
    let without = ExposedPortEndpoint::new("example.test", 8080).with_query("token=abc");

    assert_eq!(
        with_mark.url_for("http").expect("http"),
        "http://example.test:8080/?token=abc"
    );
    assert_eq!(
        without.url_for("http").expect("http"),
        "http://example.test:8080/?token=abc"
    );

    // A query that is nothing but the marker adds nothing.
    let empty = ExposedPortEndpoint::new("example.test", 8080).with_query("?");
    assert_eq!(
        empty.url_for("http").expect("http"),
        "http://example.test:8080/"
    );
}

#[test]
fn permissions_accept_combined_flags_and_raw_masks() {
    let permissions = Permissions::default()
        .owner_can(FileMode::Read | FileMode::Write)
        .group_can(FileMode::Read | FileMode::Exec)
        .others_can(0o3_u32);
    assert_eq!(permissions.to_mode(), 0o653);
    assert_eq!(
        Permissions::default()
            .owner_can(FileMode::Read | FileMode::Write | FileMode::Exec)
            .to_mode(),
        0o700
    );
}

#[test]
fn deserializing_partial_permissions_uses_reference_defaults() {
    for (payload, mode) in [
        (serde_json::json!({}), 0o700),
        (serde_json::json!({"group": 5}), 0o750),
        (
            serde_json::json!({"owner": 0, "directory": true}),
            0o040_000,
        ),
    ] {
        let permissions: Permissions = serde_json::from_value(payload).expect("permissions");
        assert_eq!(permissions.to_mode(), mode);
        let round_trip: Permissions =
            serde_json::from_value(serde_json::to_value(permissions).expect("serialize"))
                .expect("deserialize");
        assert_eq!(round_trip, permissions);
    }
    assert!(serde_json::from_value::<Permissions>(serde_json::json!({"owner": null})).is_err());
}

#[test]
fn deserializing_an_endpoint_defaults_only_optional_fields() {
    let endpoint: ExposedPortEndpoint =
        serde_json::from_value(serde_json::json!({"host": "localhost", "port": 80}))
            .expect("endpoint");
    assert_eq!(endpoint.url_for("http").expect("URL"), "http://localhost/");
    assert!(
        serde_json::from_value::<ExposedPortEndpoint>(serde_json::json!({"host": "localhost"}),)
            .is_err()
    );
}

#[test]
fn nonzero_errors_keep_original_bytes_when_diagnostics_are_overridden() {
    let result = ExecResult::new(vec![0xff, b'a'], vec![0xfe], 42);
    let command = vec!["printf".into(), "two words".into()];
    let error = SandboxError::exec_nonzero(result.clone(), command.clone());
    assert_eq!(error.message(), "stdout: \u{fffd}a\nstderr: \u{fffd}");
    assert_eq!(error.context()["command_str"], "printf two words");
    assert_eq!(error.context()["exit_code"], 42);
    assert_eq!(error.retryable(), Some(false));
    let error = error
        .with_context("stdout", "redacted")
        .with_context("exit_code", 1);
    assert_eq!(
        error.details(),
        Some(&SandboxErrorDetails::ExecNonZero { command, result })
    );
}

#[test]
fn nonzero_error_messages_follow_stream_presence() {
    for (stdout, stderr, message) in [
        ("", "", "command exited with code 2"),
        ("out", "", "out"),
        ("", "err", "err"),
        ("out", "err", "stdout: out\nstderr: err"),
    ] {
        let error = SandboxError::exec_nonzero(
            ExecResult::new(stdout.as_bytes().to_vec(), stderr.as_bytes().to_vec(), 2),
            vec!["tool".into()],
        );
        assert_eq!(error.message(), message);
    }
}

#[test]
fn exec_and_pty_errors_preserve_typed_fields() {
    let command = vec!["sleep".into(), "10".into()];
    let error = SandboxError::exec_timeout(command.clone(), Some(1.5));
    assert_eq!(
        error.details(),
        Some(&SandboxErrorDetails::ExecTimeout {
            command: command.clone(),
            timeout_s: Some(1.5),
        })
    );
    assert_eq!(error.context()["timeout_s"], 1.5);
    assert_eq!(error.retryable(), Some(false));
    let error = SandboxError::exec_transport(command.clone(), None);
    assert_eq!(
        error.details(),
        Some(&SandboxErrorDetails::ExecTransport { command })
    );
    assert_eq!(error.retryable(), None);
    assert_eq!(error.message(), "exec transport error");
    let error = SandboxError::pty_session_not_found(1234);
    assert_eq!(
        error.details(),
        Some(&SandboxErrorDetails::PtySessionNotFound { session_id: 1234 })
    );
    assert_eq!(error.context()["session_id"], 1234);
}

#[test]
fn port_error_retryability_depends_on_the_reason() {
    let error = SandboxError::exposed_port_unavailable(80, &[8080], "not_configured");
    assert_eq!(error.retryable(), Some(false));
    assert_eq!(error.context()["exposed_ports"], serde_json::json!([8080]));
    assert_eq!(error.op(), OpName::ResolveExposedPort);
    let error = SandboxError::exposed_port_unavailable(80, &[80], "backend_unavailable");
    assert_eq!(error.retryable(), None);
}

#[test]
fn generic_cause_attachment_inherits_sandbox_retryability() {
    let error = SandboxError::workspace_stop("/workspace")
        .with_cause(SandboxError::workspace_archive_read("/workspace").with_retryable(Some(true)));
    assert_eq!(error.retryable(), Some(true));
    assert_eq!(error.with_retryable(Some(false)).retryable(), Some(false));
}

#[test]
fn clearing_an_explicit_retry_decision_falls_back_to_the_cause() {
    let error = SandboxError::workspace_stop("/workspace")
        .with_retryable(Some(false))
        .with_sandbox_cause(
            SandboxError::workspace_archive_read("/workspace").with_retryable(Some(true)),
        )
        .with_retryable(None);
    assert_eq!(error.retryable(), Some(true));
}

#[test]
fn workspace_error_constructors_keep_reference_fields() {
    let missing = SandboxError::workspace_read_not_found("/workspace/missing");
    assert_eq!(missing.context()["path"], "/workspace/missing");
    assert_eq!(missing.op(), OpName::Read);
    assert_eq!(missing.retryable(), Some(false));
    let invalid = SandboxError::workspace_write_type("/workspace/out", "str");
    assert_eq!(invalid.context()["actual_type"], "str");
    assert_eq!(invalid.op(), OpName::Write);
    assert_eq!(invalid.retryable(), Some(false));
    assert_eq!(
        SandboxError::workspace_archive_write("/workspace").retryable(),
        None
    );
}
