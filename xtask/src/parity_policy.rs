//! What the `sandbox-parity` gate accepts: the backends and scenarios it expects, the backend
//! names a job may require, and how a report of results is judged.
//!
//! Kept apart from the gate that runs the tests so that the judgement can be tested on reports
//! written by hand — a report missing a scenario, naming one twice, or naming one nobody declared.

use std::collections::{BTreeMap, BTreeSet};

/// The backends every scenario runs on, by the ids they record in their states.
pub const BACKENDS: [&str; 2] = ["unix_local", "docker"];

/// The scenarios `tests/it-runtime/tests/sandbox_parity.rs` runs on each backend.
///
/// The acceptance contract rather than a copy of the test file: a scenario the suite stops running
/// is missing from the report and fails the gate, instead of shrinking what "all green" means.
pub const SCENARIOS: [&str; 5] = [
    "synchronous_run",
    "streamed_run",
    "handoff",
    "pause_and_resume",
    "caller_owned_session",
];

/// The name that requires every backend.
pub const ALL: &str = "all";

/// Reads a list of required backends: comma-separated backend ids, or [`ALL`].
///
/// An empty value requires nothing. Any other name — a misspelling above all — is refused rather
/// than ignored: ignoring it would quietly turn a required backend back into a skippable one.
///
/// # Errors
///
/// Returns a message naming what was not recognized.
pub fn parse_required(value: &str) -> Result<BTreeSet<&'static str>, String> {
    let mut required = BTreeSet::new();
    if value.trim().is_empty() {
        return Ok(required);
    }
    for name in value.split(',').map(str::trim) {
        if name == ALL {
            required.extend(BACKENDS);
        } else if let Some(backend) = BACKENDS.iter().find(|backend| **backend == name) {
            required.insert(*backend);
        } else {
            return Err(format!(
                "不认识的后端名 `{name}`（可用：{}、{ALL}，逗号分隔）",
                BACKENDS.join("、")
            ));
        }
    }
    Ok(required)
}

/// How a report reads.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every scenario ran on every backend.
    Pass(String),
    /// Every scenario was reported once, and some were skipped for missing prerequisites.
    Skip(String),
    /// The report is incomplete, repeats itself, or says something it may not.
    Fail(Vec<String>),
}

/// Judges a report: one line per test, `STATUS\tbackend\tscenario\tdetail`.
///
/// Every declared scenario must be reported exactly once for every backend, as `PASS` or `SKIP`.
#[must_use]
pub fn judge(report: &str) -> Verdict {
    let mut seen: BTreeMap<(&str, &str), (&str, &str)> = BTreeMap::new();
    let mut violations = Vec::new();
    for line in report.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.splitn(4, '\t');
        let (Some(status), Some(backend), Some(scenario)) =
            (fields.next(), fields.next(), fields.next())
        else {
            violations.push(format!("报告行无法解析：{line}"));
            continue;
        };
        let detail = fields.next().unwrap_or_default();
        let Some(backend) = BACKENDS.iter().copied().find(|known| *known == backend) else {
            violations.push(format!("报告里出现未登记的后端 `{backend}`"));
            continue;
        };
        let Some(scenario) = SCENARIOS.iter().copied().find(|known| *known == scenario) else {
            violations.push(format!(
                "报告里出现未登记的场景 `{scenario}`（后端 `{backend}`）"
            ));
            continue;
        };
        if !matches!(status, "PASS" | "SKIP") {
            violations.push(format!("报告行状态 `{status}` 无法识别：{line}"));
            continue;
        }
        if seen.insert((backend, scenario), (status, detail)).is_some() {
            violations.push(format!(
                "场景 `{scenario}` 在后端 `{backend}` 上报告了不止一次"
            ));
        }
    }
    for backend in BACKENDS {
        for scenario in SCENARIOS {
            if !seen.contains_key(&(backend, scenario)) {
                violations.push(format!(
                    "场景 `{scenario}` 在后端 `{backend}` 上没有报告结果"
                ));
            }
        }
    }
    if !violations.is_empty() {
        return Verdict::Fail(violations);
    }

    let mut summary = Vec::new();
    let mut skipped = Vec::new();
    for backend in BACKENDS {
        let results: Vec<(&str, &str)> = SCENARIOS
            .iter()
            .map(|scenario| seen[&(backend, *scenario)])
            .collect();
        let passed = results
            .iter()
            .filter(|(status, _)| *status == "PASS")
            .count();
        summary.push(format!("{backend} {passed}/{}", SCENARIOS.len()));
        if let Some((_, reason)) = results.iter().find(|(status, _)| *status == "SKIP") {
            skipped.push(format!(
                "{backend} 跳过 {} 个场景：{reason}",
                SCENARIOS.len() - passed
            ));
        }
    }
    let summary = summary.join("、");
    if skipped.is_empty() {
        Verdict::Pass(format!("两个后端同一组场景全绿（{summary}）"))
    } else {
        Verdict::Skip(format!("{summary} 通过；{}", skipped.join("；")))
    }
}
