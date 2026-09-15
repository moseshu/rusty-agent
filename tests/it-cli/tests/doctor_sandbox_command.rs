//! The `ra doctor sandbox` command line.
//!
//! What the report checks is `ra-coding`'s to get right, and the backends themselves are tested in
//! `it-exec`. What is tested here is the layer a script touches: that the subcommand exists, that
//! the report says which backend and which level are in force, and that a machine whose sandbox
//! does not work is a non-zero exit rather than a paragraph of prose nobody reads.

use clap::Parser as _;
use ra_cli::{Cli, CommandOutcome, execute};

async fn run(args: &[&str]) -> ra_cli::CommandOutput {
    execute(Cli::parse_from(args))
        .await
        .expect("the command must carry out")
}

#[tokio::test]
async fn test_the_report_names_the_backend_the_level_and_every_check() {
    let output = run(&["ra", "doctor", "sandbox"]).await;
    let report = output.stdout();

    assert!(report.starts_with("sandbox\n"), "report was: {report}");
    assert!(report.contains("backend"), "report was: {report}");
    assert!(report.contains("delivers"), "report was: {report}");
    for level in ["isolated", "workspace-write", "unconfined"] {
        assert!(
            report.contains(level),
            "every level a host can ask for is spelled out: {report}"
        );
    }
    for check in [
        "write inside the workspace root",
        "write outside every writable root",
        "reach the network",
    ] {
        assert!(report.contains(check), "missing `{check}` in: {report}");
    }
}

/// The exit status is the whole point of running this in a pipeline.
#[tokio::test]
async fn test_the_exit_status_reports_whether_this_machine_confines_a_command() {
    let output = run(&["ra", "doctor", "sandbox"]).await;

    let healthy = output
        .stdout()
        .contains("are confined as this report describes");
    assert_eq!(
        output.outcome(),
        if healthy {
            CommandOutcome::Succeeded
        } else {
            CommandOutcome::CheckFailed
        },
        "the verdict in the text and the status a script reads must be the same answer: {}",
        output.stdout()
    );
    assert_eq!(
        CommandOutcome::CheckFailed.exit_code(),
        std::process::ExitCode::from(2),
        "a failed check is its own code, distinct from a prefix that moved"
    );
}

/// A machine that claims a level has to prove it.
///
/// **Without this the suite stays green while the sandbox is completely broken.** Every other
/// assertion here checks that the report's text and the exit status agree — and they agree just as
/// well when every single check failed. This is the one that fails when the backend stops working,
/// which on Linux includes the syscall filter failing to compile or to reach bubblewrap.
#[tokio::test]
async fn test_a_machine_that_reports_a_level_must_pass_its_own_checks() {
    let output = run(&["ra", "doctor", "sandbox"]).await;
    let report = output.stdout();

    if report.contains("delivers     nothing") || report.contains("skipped") {
        // Either no backend here, or something could not be checked. Both are covered above, and
        // neither is the case this test is about.
        return;
    }
    assert_eq!(
        output.outcome(),
        CommandOutcome::Succeeded,
        "this machine reports a deliverable level, so every check must have passed: {report}"
    );
}

/// This machine has a backend, so the report must be a live one rather than the skipped form.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn test_a_platform_with_a_backend_actually_tries_the_escape() {
    let output = run(&["ra", "doctor", "sandbox"]).await;

    assert!(
        !output
            .stdout()
            .contains("there is no backend to confine it with"),
        "a default build on this platform has a backend, so the checks must have run: {}",
        output.stdout()
    );
}
