//! Whether a confined command most likely failed because its sandbox refused it: Codex's
//! `is_likely_sandbox_denied` (`sandboxing/src/denial.rs`).
//!
//! A heuristic over the command's result, as Codex's is: an unconfined command or one that
//! succeeded never counts; a command whose output names a refusal does; the exit codes a shell
//! uses for usage errors and unrunnable or missing commands do not; and a command killed by
//! `SIGSYS` under the Linux sandbox, whose seccomp filter refuses system calls that way, does.

use super::SandboxLevel;
use crate::output::ExecOutputSummary;

/// Phrases a refused operation leaves in a command's output, matched without regard to case.
const SANDBOX_DENIED_KEYWORDS: [&str; 7] = [
    "operation not permitted",
    "permission denied",
    "read-only file system",
    "seccomp",
    "sandbox",
    "landlock",
    "failed to write file",
];

/// Exit codes a shell reports for its own reasons: a usage error, a command it could not run, and
/// a command it could not find.
const QUICK_REJECT_EXIT_CODES: [i32; 3] = [2, 126, 127];

/// The exit status of a process killed by a signal: this base plus the signal number.
const EXIT_CODE_SIGNAL_BASE: i32 = 128;

/// `SIGSYS` on Linux, the signal a seccomp filter kills a refused system call's process with.
const SIGSYS: i32 = 31;

/// The name the Linux sandbox reports, `bwrap::BACKEND_NAME`, which builds only with its feature.
const LINUX_SANDBOX_BACKEND: &str = "bwrap";

/// Whether `summary` most likely records a command its sandbox refused.
#[must_use]
pub fn is_likely_sandbox_denied(summary: &ExecOutputSummary) -> bool {
    if summary.termination_reason().is_some() {
        return false;
    }
    let Some(report) = summary.sandbox() else {
        return false;
    };
    if report.level() == SandboxLevel::Unconfined {
        return false;
    }
    // A process that ended without an exit code was killed, which Codex reads as -1.
    let exit_code = summary.exit_code().unwrap_or(-1);
    if exit_code == 0 {
        return false;
    }

    if [summary.stderr(), summary.stdout()]
        .into_iter()
        .any(|section| {
            let lower = section.to_lowercase();
            SANDBOX_DENIED_KEYWORDS
                .iter()
                .any(|needle| lower.contains(needle))
        })
    {
        return true;
    }

    if QUICK_REJECT_EXIT_CODES.contains(&exit_code) {
        return false;
    }

    report.backend() == Some(LINUX_SANDBOX_BACKEND) && exit_code == EXIT_CODE_SIGNAL_BASE + SIGSYS
}
