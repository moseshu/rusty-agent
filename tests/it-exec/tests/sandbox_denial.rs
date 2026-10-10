//! Codex's `is_likely_sandbox_denied` heuristic over a command's result, ported with its
//! `exec_tests.rs` cases. Codex's `SandboxType::None` is a result without a sandbox report, or
//! with an unconfined one; `LinuxSeccomp` is the Linux sandbox's report.

use ra_exec::{output::ExecOutputSummary, sandbox::is_likely_sandbox_denied};
use serde_json::json;

const EXIT_CODE_SIGNAL_BASE: i32 = 128;
const SIGSYS: i32 = 31;

/// A confined result, reported by `backend`.
fn sandboxed(backend: &str, exit_code: i32, stdout: &str, stderr: &str) -> ExecOutputSummary {
    let report = serde_json::from_value(json!({"backend": backend, "level": "workspace-write"}))
        .expect("a sandbox report");
    ExecOutputSummary::new(stdout, stderr)
        .with_exit_code(exit_code)
        .with_sandbox(report)
}

fn linux_seccomp(exit_code: i32, stdout: &str, stderr: &str) -> ExecOutputSummary {
    sandboxed("bwrap", exit_code, stdout, stderr)
}

#[test]
fn sandbox_detection_requires_keywords() {
    assert!(!is_likely_sandbox_denied(&linux_seccomp(1, "", "")));
}

#[test]
fn sandbox_detection_identifies_keyword_in_stderr() {
    assert!(is_likely_sandbox_denied(&linux_seccomp(
        1,
        "",
        "Operation not permitted"
    )));
}

#[test]
fn sandbox_detection_respects_quick_reject_exit_codes() {
    assert!(!is_likely_sandbox_denied(&linux_seccomp(
        127,
        "",
        "command not found"
    )));
}

#[test]
fn sandbox_detection_ignores_non_sandbox_mode() {
    let unreported = ExecOutputSummary::new("", "Operation not permitted").with_exit_code(1);
    assert!(!is_likely_sandbox_denied(&unreported));

    let report = serde_json::from_value(json!({"level": "unconfined"})).unwrap();
    let unconfined = ExecOutputSummary::new("", "Operation not permitted")
        .with_exit_code(1)
        .with_sandbox(report);
    assert!(!is_likely_sandbox_denied(&unconfined));
}

#[test]
fn sandbox_detection_ignores_network_policy_text_in_non_sandbox_mode() {
    let output = ExecOutputSummary::new(
        r#"CODEX_NETWORK_POLICY_DECISION {"decision":"ask","reason":"not_allowed","source":"decider","protocol":"http","host":"google.com","port":80}"#,
        "",
    )
    .with_exit_code(0);
    assert!(!is_likely_sandbox_denied(&output));
}

/// Codex reads the combined output; this executor keeps the two streams, either of which carries
/// the phrase.
#[test]
fn sandbox_detection_uses_aggregated_output() {
    assert!(is_likely_sandbox_denied(&sandboxed(
        "seatbelt",
        101,
        "cargo failed: Read-only file system when writing target",
        "",
    )));
}

#[test]
fn sandbox_detection_ignores_network_policy_text_with_zero_exit_code() {
    assert!(!is_likely_sandbox_denied(&linux_seccomp(
        0,
        r#"CODEX_NETWORK_POLICY_DECISION {"decision":"ask","source":"decider","protocol":"http","host":"google.com","port":80}"#,
        "",
    )));
}

#[test]
fn sandbox_detection_flags_sigsys_exit_code() {
    assert!(is_likely_sandbox_denied(&linux_seccomp(
        EXIT_CODE_SIGNAL_BASE + SIGSYS,
        "",
        ""
    )));
    // Only the Linux sandbox's seccomp filter kills with `SIGSYS`.
    assert!(!is_likely_sandbox_denied(&sandboxed(
        "seatbelt",
        EXIT_CODE_SIGNAL_BASE + SIGSYS,
        "",
        ""
    )));
}

#[test]
fn host_termination_is_not_a_sandbox_refusal() {
    use ra_exec::output::ExecEvictionReason;
    for reason in [
        ExecEvictionReason::TotalTimeout,
        ExecEvictionReason::IdleTimeout,
        ExecEvictionReason::custom("cancelled"),
        ExecEvictionReason::HostShutdown,
    ] {
        let summary =
            linux_seccomp(1, "", "operation not permitted").with_termination_reason(reason);
        let restored = serde_json::from_str(&serde_json::to_string(&summary).unwrap()).unwrap();
        assert!(!is_likely_sandbox_denied(&restored));
    }
}
