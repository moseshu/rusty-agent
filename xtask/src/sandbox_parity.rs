//! The `sandbox-parity` gate: one set of runner-level scenarios on every built-in sandbox backend.
//!
//! The scenarios live in `tests/it-runtime/tests/sandbox_parity.rs`, ignored by the ordinary test
//! run because they need real backends. This gate runs them, and reads back the one line each test
//! appends to a report file: `PASS` when the scenario ran, `SKIP` with the reason when the backend's
//! prerequisites are missing on this machine. [`xtask::parity_policy`] judges the report: every
//! scenario, on every backend, exactly once.
//!
//! **A skipped backend is not a passed one.** A machine without a Docker daemon runs the local half
//! and reports the rest as SKIP, counted in the summary like every other gate that did not check
//! its subject. A formal job names the backends it must cover in `RA_SANDBOX_PARITY_REQUIRE` (or
//! `--require`): the tests themselves then fail on a missing prerequisite, so a job that lost its
//! daemon turns red rather than quietly shrinking to one backend. A name the gate does not know is
//! refused before anything runs, so a misspelled requirement cannot pass for no requirement.

use std::process::Command;

use xtask::parity_policy::{Verdict, judge, parse_required};

use crate::gate::Outcome;
use crate::source;

/// The environment variable naming the backends that may not be skipped.
const REQUIRE_VAR: &str = "RA_SANDBOX_PARITY_REQUIRE";

/// Runs the gate. `require` overrides [`REQUIRE_VAR`] for the tests it starts.
pub(crate) fn run(require: Option<&str>) -> Outcome {
    let require = match require {
        Some(value) => value.to_owned(),
        None => std::env::var(REQUIRE_VAR).unwrap_or_default(),
    };
    if let Err(message) = parse_required(&require) {
        return Outcome::Fail(vec![format!("{REQUIRE_VAR}／--require：{message}")]);
    }

    let manifest = source::workspace_root().join("tests").join("Cargo.toml");
    let report = std::env::temp_dir().join(format!("ra-sandbox-parity-{}.tsv", std::process::id()));
    let _ = std::fs::remove_file(&report);

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(&cargo)
        .args(["test", "--manifest-path"])
        .arg(&manifest)
        .args(["-p", "it-runtime", "--test", "sandbox_parity", "--"])
        // One at a time: the lifecycle scenario reads a terminal with a short yield, and several
        // containers starting at once on a small runner can push it past that.
        .args(["--ignored", "--test-threads=1"])
        .env("RA_SANDBOX_PARITY_REPORT", &report)
        .env(REQUIRE_VAR, &require)
        .current_dir(source::workspace_root())
        .status();
    let text = std::fs::read_to_string(&report).unwrap_or_default();
    let _ = std::fs::remove_file(&report);

    let status = match status {
        Ok(status) => status,
        Err(err) => return Outcome::Fail(vec![format!("无法启动 cargo test：{err}")]),
    };
    if !status.success() {
        return Outcome::Fail(vec![format!(
            "场景测试退出码 {status}（缺前置条件而被要求的后端也在此列，见 {REQUIRE_VAR}）"
        )]);
    }
    match judge(&text) {
        Verdict::Pass(detail) => Outcome::pass(detail),
        Verdict::Skip(detail) => Outcome::skip("本机后端前置条件", detail),
        Verdict::Fail(violations) => Outcome::Fail(violations),
    }
}
