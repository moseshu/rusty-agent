//! `ra-core::sandbox::pty`: the limits and choices every terminal-capable backend shares.
//!
//! Ported from the reference's `tests/sandbox/test_pty_types.py`, one test per upstream test, plus
//! the boundaries those tests leave implicit.

use std::collections::HashSet;

use ra_core::sandbox::{
    PTY_EMPTY_YIELD_TIME_MS_MIN, PTY_PROCESS_ID_MAX_EXCLUSIVE, PTY_PROCESS_ID_MIN,
    PTY_YIELD_TIME_MS_MAX, PTY_YIELD_TIME_MS_MIN, PtyProcessId, PtyProcessMeta, PtyStartRequest,
    ShellInvocation, allocate_pty_process_id, clamp_pty_yield_time_ms,
    process_id_to_prune_from_meta, resolve_pty_write_yield_time_ms,
};

fn meta(process_id: i64, last_used: f64, exited: bool) -> PtyProcessMeta<f64> {
    PtyProcessMeta::new(PtyProcessId(process_id), last_used, exited)
}

// `test_clamp_pty_yield_time_ms_enforces_minimum`
#[test]
fn a_wait_below_the_minimum_is_raised_to_it() {
    assert_eq!(clamp_pty_yield_time_ms(0), PTY_YIELD_TIME_MS_MIN);
}

// `test_resolve_pty_write_yield_time_ms_uses_longer_poll_for_empty_input`
#[test]
fn a_write_that_sends_nothing_waits_longer() {
    assert_eq!(
        resolve_pty_write_yield_time_ms(PTY_YIELD_TIME_MS_MIN, true),
        PTY_EMPTY_YIELD_TIME_MS_MIN
    );
    assert_eq!(
        resolve_pty_write_yield_time_ms(PTY_YIELD_TIME_MS_MIN, false),
        PTY_YIELD_TIME_MS_MIN
    );
}

// `test_allocate_pty_process_id_avoids_used_ids`
#[test]
fn an_allocated_id_is_never_one_in_use() {
    let used: HashSet<PtyProcessId> = [1000, 1001, 1002].into_iter().map(PtyProcessId).collect();
    let allocated = allocate_pty_process_id(&used);
    assert!(!used.contains(&allocated));
}

// `test_process_id_to_prune_from_meta_prefers_exited_unprotected_sessions`
#[test]
fn pruning_prefers_an_exited_process_outside_the_protected_recent_set() {
    let mut entries: Vec<_> = (0..8)
        .map(|index| meta(1001 + index, 100.0 - index as f64, false))
        .collect();
    entries.push(meta(2001, 1.0, true));
    entries.push(meta(2002, 2.0, false));

    assert_eq!(
        process_id_to_prune_from_meta(&entries),
        Some(PtyProcessId(2001))
    );
}

// Beyond the upstream file.

#[test]
fn a_wait_above_the_maximum_is_lowered_to_it() {
    assert_eq!(clamp_pty_yield_time_ms(u64::MAX), PTY_YIELD_TIME_MS_MAX);
    assert_eq!(
        resolve_pty_write_yield_time_ms(u64::MAX, true),
        PTY_YIELD_TIME_MS_MAX
    );
    assert_eq!(clamp_pty_yield_time_ms(1_234), 1_234);
}

#[test]
fn allocated_ids_stay_in_range() {
    let used = HashSet::new();
    for _ in 0..1_000 {
        let PtyProcessId(id) = allocate_pty_process_id(&used);
        assert!((PTY_PROCESS_ID_MIN..PTY_PROCESS_ID_MAX_EXCLUSIVE).contains(&id));
    }
}

#[test]
fn pruning_falls_back_to_the_least_recent_running_process() {
    let mut entries: Vec<_> = (0..8)
        .map(|index| meta(1001 + index, 100.0 - index as f64, false))
        .collect();
    entries.push(meta(2001, 5.0, false));
    entries.push(meta(2002, 2.0, false));

    assert_eq!(
        process_id_to_prune_from_meta(&entries),
        Some(PtyProcessId(2002))
    );
}

#[test]
fn nothing_is_pruned_when_every_process_is_protected() {
    let entries: Vec<_> = (0..8)
        .map(|index| meta(1001 + index, index as f64, index % 2 == 0))
        .collect();
    assert_eq!(process_id_to_prune_from_meta(&entries), None);
    assert_eq!(process_id_to_prune_from_meta::<f64>(&[]), None);
}

/// The reference's `pty_exec_start` defaults to `shell=True`, the login shell.
#[test]
fn a_start_request_defaults_to_the_login_shell() {
    let request = PtyStartRequest::new(["pwd".to_owned()]);
    assert_eq!(request.shell, ShellInvocation::Login);
    assert!(!request.tty);

    let request = request.with_shell(ShellInvocation::None);
    assert_eq!(request.shell, ShellInvocation::None);
}

/// Writing to a process started without a terminal: the reference's exact words, a transport
/// failure that retrying will not fix, and a detail a caller can match without reading the text.
#[test]
fn input_to_a_process_without_a_terminal_is_refused_with_the_reference_wording() {
    use ra_core::sandbox::{
        ErrorCode, PTY_STDIN_UNAVAILABLE_MESSAGE, SandboxError, SandboxErrorDetails,
    };

    let error = SandboxError::pty_stdin_unavailable(1_337);
    assert_eq!(error.message(), "stdin is not available for this process");
    assert_eq!(error.message(), PTY_STDIN_UNAVAILABLE_MESSAGE);
    assert_eq!(error.error_code(), ErrorCode::ExecTransportError);
    assert_eq!(error.retryable(), Some(false));
    assert_eq!(
        error.details(),
        Some(&SandboxErrorDetails::PtyStdinUnavailable { session_id: 1_337 })
    );
}
