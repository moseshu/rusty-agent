//! The `no-inline-tests` gate: no test code is allowed under `crates/`.
//!
//! The constraint itself is in [the project structure](../../Docs/Rusty_Agent_Project_Structure.md)
//! §5.5: behavioral tests all live in the separate `tests/` workspace. **Discipline alone cannot
//! hold this** — dropping a `#[cfg(test)] mod tests` next to the implementation is the most
//! natural thing to do while writing it, which is why there is a gate.
//!
//! Two things are checked:
//! - `cfg(test` and `#[test]` inside `crates/**/*.rs`;
//! - the `crates/*/tests/` directory (Cargo's integration-test slot).
//!
//! **An explicit exception**: `#[cfg(feature = "test-api")]` is not covered — `ra-patch`'s fuzzy
//! matcher and `ra-model`'s chat convert/stream may use it to expose a test-only entry to their
//! host crate. It contains no `cfg(test`, so it is never caught by accident.

use crate::gate::Outcome;
use crate::source;

/// Runs the gate.
pub(crate) fn run() -> Outcome {
    let crates_dir = source::workspace_root().join("crates");
    let mut violations = Vec::new();
    let mut scanned = 0_usize;

    for file in source::rust_files(&crates_dir) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        scanned += 1;

        for (index, line) in text.lines().enumerate() {
            if source::is_comment(line) {
                continue;
            }
            let found = if line.contains("cfg(test") {
                Some("`#[cfg(test)]` 内联测试模块")
            } else if line.contains("#[test]") || line.contains("#[tokio::test]") {
                Some("`#[test]` 测试函数")
            } else {
                None
            };
            if let Some(what) = found {
                violations.push(format!(
                    "{}:{}：{what} —— 测试一律放 tests/it-<crate>/tests/",
                    source::relative(&file),
                    index + 1,
                ));
            }
        }
    }

    // Cargo's integration-test slot is banned too: it bypasses the tests/ workspace and gets
    // built by the main workspace.
    if let Ok(entries) = std::fs::read_dir(&crates_dir) {
        for entry in entries.flatten() {
            let tests_dir = entry.path().join("tests");
            if tests_dir.is_dir() {
                violations.push(format!(
                    "{}：crates/*/tests/ 是 Cargo 集成测试位，同样禁止",
                    source::relative(&tests_dir),
                ));
            }
        }
    }

    Outcome::from_violations(violations, format!("扫了 {scanned} 个源文件，无内联测试"))
}
