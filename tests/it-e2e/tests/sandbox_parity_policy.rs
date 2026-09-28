//! How the `sandbox-parity` gate judges a report and reads the backends a job requires.

use xtask::parity_policy::{BACKENDS, SCENARIOS, Verdict, judge, parse_required};

/// One line for every scenario on every backend, with `status` for each.
fn full_report(status: impl Fn(&str, &str) -> &'static str) -> String {
    let mut report = String::new();
    for backend in BACKENDS {
        for scenario in SCENARIOS {
            let detail = if status(backend, scenario) == "SKIP" {
                "no daemon"
            } else {
                ""
            };
            report.push_str(&format!(
                "{}\t{backend}\t{scenario}\t{detail}\n",
                status(backend, scenario)
            ));
        }
    }
    report
}

fn violations(report: &str) -> Vec<String> {
    match judge(report) {
        Verdict::Fail(violations) => violations,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn every_scenario_passing_on_every_backend_passes() {
    let verdict = judge(&full_report(|_, _| "PASS"));
    assert_eq!(
        verdict,
        Verdict::Pass("两个后端同一组场景全绿（unix_local 5/5、docker 5/5）".to_owned())
    );
}

#[test]
fn a_skipped_backend_is_counted_as_a_skip_not_a_pass() {
    let verdict = judge(&full_report(|backend, _| {
        if backend == "docker" { "SKIP" } else { "PASS" }
    }));
    assert_eq!(
        verdict,
        Verdict::Skip(
            "unix_local 5/5、docker 0/5 通过；docker 跳过 5 个场景：no daemon".to_owned()
        )
    );
}

/// The review's reproduction: one scenario reported on each backend, and different ones.
#[test]
fn a_report_missing_scenarios_fails_even_when_both_backends_appear() {
    let found = violations("PASS\tunix_local\tsynchronous_run\t\nPASS\tdocker\thandoff\t\n");
    assert_eq!(found.len(), 8, "{found:?}");
    assert!(found.contains(&"场景 `handoff` 在后端 `unix_local` 上没有报告结果".to_owned()));
    assert!(found.contains(&"场景 `synchronous_run` 在后端 `docker` 上没有报告结果".to_owned()));
}

#[test]
fn a_scenario_reported_twice_fails() {
    let mut report = full_report(|_, _| "PASS");
    report.push_str("PASS\tdocker\thandoff\t\n");
    assert_eq!(
        violations(&report),
        ["场景 `handoff` 在后端 `docker` 上报告了不止一次"]
    );
}

#[test]
fn an_undeclared_scenario_backend_or_status_fails() {
    for (line, expected) in [
        (
            "PASS\tdocker\tsomething_else\t",
            "报告里出现未登记的场景 `something_else`（后端 `docker`）",
        ),
        ("PASS\tmodal\thandoff\t", "报告里出现未登记的后端 `modal`"),
        (
            "FAIL\tdocker\thandoff\t",
            "报告行状态 `FAIL` 无法识别：FAIL\tdocker\thandoff\t",
        ),
    ] {
        let report = format!("{}{line}\n", full_report(|_, _| "PASS"));
        assert_eq!(violations(&report), [expected], "{line}");
    }
}

#[test]
fn required_backends_are_read_by_name() {
    assert!(parse_required("").unwrap().is_empty());
    assert!(parse_required("  ").unwrap().is_empty());
    assert_eq!(
        parse_required("docker")
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        ["docker"]
    );
    assert_eq!(
        parse_required(" unix_local , docker ")
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        ["docker", "unix_local"]
    );
    assert_eq!(parse_required("all").unwrap().len(), BACKENDS.len());
}

/// The review's reproduction: a misspelled backend is refused rather than ignored.
#[test]
fn an_unknown_or_empty_backend_name_is_refused() {
    for value in ["dockre", "docker,dockre", "docker,,unix_local", "docker,"] {
        assert!(parse_required(value).is_err(), "{value}");
    }
    assert!(parse_required("dockre").unwrap_err().contains("`dockre`"));
}
