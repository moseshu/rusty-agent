//! The `test` gate: runs the separate test workspace.
//!
//! Tests do not live in the main workspace (see
//! [the project structure](../../Docs/Rusty_Agent_Project_Structure.md) §5.5), so
//! `cargo test --workspace` reaches no behavioral assertion at all. This gate is that entry point.
//!
//! `tests/` **is committed**, so its absence does not mean "not on this machine" but a broken
//! checkout — that is a FAIL, not a skip.
//!
//! # Which runner
//!
//! `cargo test` by default; `cargo nextest run` when `RA_TEST_RUNNER=nextest` asks for it. Both run
//! every case here — the one thing nextest does not run is doctests, and the hosts under `tests/`
//! hold no product code to document, their `src/lib.rs` files being empty by design.
//!
//! **Opting in rather than detecting** is deliberate. On the machine this was developed on, a full
//! `cargo nextest run` over this workspace repeatedly stopped in its list phase: the binaries it had
//! spawned to enumerate tests sat sleeping at zero percent CPU indefinitely, while the same command
//! against a single host finished instantly. The cause is not established — it reproduces at scale
//! and not below it — so the default stays on the runner that demonstrably completes. A gate that
//! hangs is worse than a gate that is merely slower, and a runner nobody can explain should not be
//! the one a fresh checkout gets silently.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::gate::Outcome;
use crate::source;

/// The host for cross-crate contracts; it corresponds to no single crate under test.
const CROSS_CRATE_HOST: &str = "it-e2e";

/// Which runner executed the suite.
///
/// The gate reports it. A gate that quietly switches runners is one whose green cannot be compared
/// with yesterday's — and the two differ in what they *skip*, not only in how fast they are.
enum Runner {
    /// `cargo nextest run`: one process per test, parallel across binaries.
    Nextest,
    /// `cargo test`: the built-in harness, which runs the binaries one after another.
    Cargo,
}

impl Runner {
    /// Reads the requested runner, falling back to the built-in harness.
    ///
    /// An unavailable nextest is **not** silently downgraded: someone who asked for it and got the
    /// slow path anyway would conclude the tool does nothing, so the absence is reported and the
    /// suite still runs.
    fn select(cargo: &str) -> (Self, Option<String>) {
        let requested = std::env::var("RA_TEST_RUNNER").unwrap_or_default();
        if !requested.eq_ignore_ascii_case("nextest") {
            return (Self::Cargo, None);
        }
        let available = Command::new(cargo)
            .args(["nextest", "--version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if available {
            (Self::Nextest, None)
        } else {
            (
                Self::Cargo,
                Some(
                    "RA_TEST_RUNNER=nextest 但没装 cargo-nextest，已退回 cargo test\
                     （装：cargo install cargo-nextest --locked）"
                        .to_owned(),
                ),
            )
        }
    }

    /// The subcommand this runner is invoked with.
    fn command(&self) -> &'static [&'static str] {
        match self {
            Self::Nextest => &["nextest", "run"],
            Self::Cargo => &["test"],
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Nextest => "nextest",
            Self::Cargo => "cargo test",
        }
    }
}

/// Runs the gate. `args` passes through to the runner (for example `-p it-core`).
pub(crate) fn run(args: &[String]) -> Outcome {
    let manifest = source::workspace_root().join("tests").join("Cargo.toml");
    if !manifest.is_file() {
        return Outcome::Fail(vec![format!(
            "{} 不存在——tests/ 是版本库的一部分，缺失说明 checkout 不完整",
            source::relative(&manifest),
        )]);
    }

    let violations = validate_layout(&manifest);
    if !violations.is_empty() {
        return Outcome::Fail(violations);
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let (runner, note) = Runner::select(&cargo);
    if let Some(note) = &note {
        eprintln!("{note}");
    }
    let status = Command::new(&cargo)
        .args(runner.command())
        .arg("--manifest-path")
        .arg(&manifest)
        .args(args)
        .current_dir(source::workspace_root())
        .status();

    match status {
        Ok(status) if status.success() => {
            Outcome::pass(format!("测试 workspace 全绿（{}）", runner.label()))
        }
        Ok(status) => Outcome::Fail(vec![format!("{} 退出码 {status}", runner.label())]),
        Err(err) => Outcome::Fail(vec![format!("无法启动 {}：{err}", runner.label())]),
    }
}

/// Crate under test -> its test host name. `ra-core` -> `it-core`.
fn host_of(crate_name: &str) -> String {
    format!("it-{}", crate_name.trim_start_matches("ra-"))
}

/// Rejects two kinds of false green before Cargo even starts: a missing host, and a test file that
/// holds comments but no assertion.
///
/// The host list is **derived from `crates/`** rather than hard-coded: a hard-coded table only
/// ever gets forgotten on the day a crate is created, which is exactly when the gate should
/// speak.
fn validate_layout(manifest: &Path) -> Vec<String> {
    let tests_root = manifest.parent().unwrap_or(manifest);
    let workspace = std::fs::read_to_string(manifest).unwrap_or_default();
    let mut violations = Vec::new();
    let mut hosts = Vec::new();

    for crate_name in source::library_crates() {
        let host = host_of(&crate_name);
        if !workspace.contains(&format!("\"{host}\"")) {
            violations.push(format!(
                "tests/Cargo.toml 缺测试宿主 `{host}`——`crates/{crate_name}` 是库 crate，必须有一个"
            ));
            continue;
        }
        hosts.push(host.clone());

        let host_manifest = tests_root.join(&host).join("Cargo.toml");
        let text = std::fs::read_to_string(&host_manifest).unwrap_or_default();
        let expected = format!("{crate_name} = {{ path = \"../../crates/{crate_name}\"");
        if !text.contains(&expected) {
            violations.push(format!(
                "{}：宿主 `{host}` 没有通过 path 依赖 `{crate_name}`",
                source::relative(&host_manifest),
            ));
        }
    }

    if workspace.contains(&format!("\"{CROSS_CRATE_HOST}\"")) {
        hosts.push(CROSS_CRATE_HOST.to_owned());
    } else {
        violations.push(format!(
            "tests/Cargo.toml 缺跨 crate 契约宿主 `{CROSS_CRATE_HOST}`"
        ));
    }

    let mut test_files = 0_usize;
    let mut test_cases = 0_usize;
    for host in &hosts {
        // Only Cargo's external test slot counts: `src/` is an empty lib and `fixtures/` holds
        // compile-time negative fixtures, and neither should read as "a test file with no
        // assertions".
        for file in source::rust_files(&tests_root.join(host).join("tests")) {
            test_files += 1;
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            let count = text.lines().filter(|line| is_test_attribute(line)).count();
            if count == 0 {
                violations.push(format!(
                    "{}：空占位测试没有任何测试函数——实现行为时再创建，不能用零测试制造假绿",
                    source::relative(&file),
                ));
            }
            test_cases += count;
        }
    }

    if test_files == 0 || test_cases == 0 {
        violations.push("独立测试 workspace 没有任何有效行为断言".to_owned());
    }

    violations
}

/// Whether a line is a test-marking attribute.
///
/// **Match the attribute name, do not compare whole lines**: `#[tokio::test(flavor =
/// "multi_thread")]` and `#[rstest(case(1), case(2))]` are both real tests, and an exact
/// whole-line comparison would judge a file of parameterized tests to be an empty placeholder — a
/// false positive is worse than a miss (same reasoning as [`source::is_comment`]).
fn is_test_attribute(line: &str) -> bool {
    const MARKERS: &[&str] = &["test", "tokio::test", "rstest", "test_case"];
    let Some(rest) = line.trim().strip_prefix("#[") else {
        return false;
    };
    MARKERS.iter().any(|marker| {
        rest.strip_prefix(marker)
            // `]` means an attribute without arguments, `(` one with them. This is what tells
            // `#[test]` apart from `#[test_case(..)]` and `#[should_panic]`; without it, prefix
            // matching would count them all as tests.
            .is_some_and(|tail| tail.starts_with(']') || tail.starts_with('('))
    })
}
