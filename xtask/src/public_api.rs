//! The `public-api` gate: the public-surface contract.
//!
//! It checks three things, each part of "can downstream safely depend on this crate":
//!
//! 1. **Baseline reconciliation** — a deleted public item, a changed signature, or one that
//!    quietly appeared must all show up in the diff;
//! 2. **Extension safety 1 and 2** — public enums are `#[non_exhaustive]`, public structs have no
//!    public fields;
//! 3. **Stability grades** — every crate's `lib.rs` states `Stable` / `Evolving` / `Internal`.
//!
//! A baseline change **is not an error, it is a decision that has to be seen**: regenerate with
//! `cargo xtask public-api --bless` and let the diff appear in code review. The gate stops
//! "changed quietly", not "changed".

use crate::api;
use crate::extension_safety;
use crate::gate::Outcome;
use crate::source;

/// The crates covered by the public-surface contract.
///
/// Only the ones **downstream depends on**. `ra-cli` and `xtask` are binaries and `ra-coding` is
/// the reference product — a product's public surface is not a framework contract, and it changing
/// with the business is normal.
const TRACKED: &[&str] = &[
    "ra-core",
    "ra-macros",
    "ra-model",
    "ra-prompt",
    "ra-context",
    "ra-runtime",
    "ra-session",
    "ra-exec",
    "ra-sandbox",
    "ra-mcp",
    "ra-protocol",
    "ra-eval",
    "ra-patch",
    "ra-tools",
];

/// Accepts the current items the `fragments` name into their crates' baselines, and nothing else.
///
/// The reviewed alternative to `--bless`, which accepts every pending change at once: see
/// [`xtask::baseline_policy`]. A crate with no baseline yet is left alone — accepting a few of its
/// items would turn the rest of its surface into pending additions nobody asked to review — and a
/// fragment that names nothing to reconcile fails the command.
pub(crate) fn accept(fragments: &[String]) -> Outcome {
    let fragments: Vec<&str> = fragments.iter().map(String::as_str).collect();
    let mut changed = Vec::new();
    let mut report = Vec::new();
    let mut violations = Vec::new();
    for name in TRACKED {
        let Some(baseline) = api::read_baseline(name) else {
            continue;
        };
        let items = match api::snapshot(name) {
            Ok(items) => items,
            Err(err) => {
                violations.push(format!("{name}：{err}"));
                continue;
            }
        };
        let accepted = xtask::baseline_policy::accept(&baseline, &items, &fragments);
        if accepted.added.is_empty() && accepted.removed.is_empty() {
            continue;
        }
        if let Err(err) = api::write_baseline(name, &accepted.baseline) {
            violations.push(format!("{name}：写基线失败 {err}"));
            continue;
        }
        for item in &accepted.added {
            report.push(format!("{name} + {item}"));
        }
        for item in &accepted.removed {
            report.push(format!("{name} - {item}"));
        }
        changed.extend(accepted.added);
        changed.extend(accepted.removed);
    }
    for fragment in xtask::baseline_policy::unused_fragments(&fragments, &changed) {
        violations.push(format!(
            "`{fragment}` 没有匹配到任何待定的公开项（有基线的 crate 中）——拼写或已接受？"
        ));
    }
    for line in &report {
        println!("  {line}");
    }
    Outcome::from_violations(violations, format!("已接受 {} 项", report.len()))
}

/// Runs the gate. When `bless` is true it rewrites the baseline instead of reconciling.
pub(crate) fn run(bless: bool) -> Outcome {
    // The extension-safety and stability-grade halves would still run against the source alone, but
    // reporting PASS for a gate whose headline half never ran is the manufactured confidence this
    // outcome type exists to refuse.
    if !bless && !source::baselines_present() {
        return Outcome::skip(
            "cargo xtask public-api --bless",
            "api/ 不进版本库，这次 checkout 没有公开面基线可对账",
        );
    }
    let mut violations = Vec::new();
    let mut blessed = 0_usize;
    let mut missing = Vec::new();

    for name in TRACKED {
        let items = match api::snapshot(name) {
            Ok(items) => items,
            Err(err) => {
                violations.push(format!("{name}：{err}"));
                continue;
            }
        };

        if bless {
            match api::write_baseline(name, &items) {
                Ok(()) => blessed += 1,
                Err(err) => violations.push(format!("{name}：写基线失败 {err}")),
            }
            continue;
        }

        match api::read_baseline(name) {
            Some(baseline) => violations.extend(diff(name, &baseline, &items)),
            None => missing.push(*name),
        }
    }

    if bless {
        return Outcome::from_violations(violations, format!("已重写 {blessed} 份基线"));
    }

    if !missing.is_empty() {
        violations.push(format!(
            "缺基线快照：{}——跑 `cargo xtask public-api --bless` 生成",
            missing.join(" / ")
        ));
    }

    for name in TRACKED {
        violations.extend(extension_safety::check(name));
    }

    Outcome::from_violations(
        violations,
        format!("{} 个 crate 的公开面与基线一致", TRACKED.len()),
    )
}

/// Reconciles line by line. **Removals and additions are reported separately**: the first is
/// breaking, the second only needs a baseline refresh.
fn diff(name: &str, baseline: &[String], current: &[String]) -> Vec<String> {
    let mut violations = Vec::new();

    for item in baseline {
        if !current.contains(item) {
            violations.push(format!(
                "{name}：公开项消失或签名改变（**破坏性**）— {item}"
            ));
        }
    }
    for item in current {
        if !baseline.contains(item) {
            violations.push(format!("{name}：新增公开项，需 --bless 更新基线 — {item}"));
        }
    }

    violations
}
