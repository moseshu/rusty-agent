//! The `feature-matrix` gate: every crate that declares features must compile at both extremes.
//!
//! # Why `cargo check --workspace --no-default-features` will not do
//!
//! **It does not travel along the edges between crates.** `--no-default-features` applies only to
//! the packages named on the command line; the `ra-eval -> ra-model` edge still says
//! `default-features = true`, so `ra-model` is compiled with `openai` anyway. Turning default
//! features off across the whole workspace looks like the broadest coverage while missing exactly
//! the cell under test.
//!
//! So this runs `-p <crate> --no-default-features` **one crate at a time** — only that form
//! actually puts the crate under test through the compiler with zero features.
//!
//! # Why only the two extremes
//!
//! The full permutation is 2^n compilations, and `ra-exec` alone has 8. The two extremes (all off,
//! all on) catch the vast majority of missing `#[cfg]`s: code that forgot a cfg fails to resolve
//! symbols with everything off, and a feature that forgot a cfg collides with everything on. The
//! default combination is covered by `cargo check --workspace` and is not repeated here.

use std::process::Command;

use crate::gate::Outcome;
use crate::source;

/// Runs the gate.
pub(crate) fn run() -> Outcome {
    let crates: Vec<String> = source::crate_names()
        .into_iter()
        .filter(|name| declares_features(name))
        .collect();

    if crates.is_empty() {
        return Outcome::skip("R0-1", "还没有 crate 声明 feature，无矩阵可测");
    }

    let mut violations = Vec::new();
    for name in &crates {
        for flag in ["--no-default-features", "--all-features"] {
            if let Err(violation) = check(name, flag) {
                violations.push(violation);
            }
        }
    }

    Outcome::from_violations(
        violations,
        format!("{} 个带 feature 的 crate × 全关/全开两个极端", crates.len()),
    )
}

/// Whether a crate declares `[features]`. One without features needs no matrix.
fn declares_features(name: &str) -> bool {
    let manifest = source::workspace_root()
        .join("crates")
        .join(name)
        .join("Cargo.toml");
    std::fs::read_to_string(manifest)
        .unwrap_or_default()
        .lines()
        .any(|line| line.trim() == "[features]")
}

fn check(name: &str, flag: &str) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["check", "--quiet", "-p", name, flag])
        .current_dir(source::workspace_root())
        .output();

    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "`cargo check -p {name} {flag}` 未通过：\n{}",
            String::from_utf8_lossy(&output.stderr).trim(),
        )),
        Err(err) => Err(format!("无法为 {name} 启动 cargo：{err}")),
    }
}
