//! `no-inline-tests` 门禁：`crates/` 下不允许任何测试代码。
//!
//! 约束本身见[项目结构](../../Docs/Rusty_Agent_Project_Structure.md) §5.5：行为测试
//! 一律在独立的 `tests/` workspace 里。这条**靠人自觉守不住**——写实现时顺手加个
//! `#[cfg(test)] mod tests` 是最自然的动作，所以要有门禁。
//!
//! 查两样：
//! - `crates/**/*.rs` 里的 `cfg(test` 与 `#[test]`；
//! - `crates/*/tests/` 目录（Cargo 的集成测试位）。
//!
//! **明确例外**：`#[cfg(feature = "test-api")]` 不在此列——`ra-patch` 的 fuzz 匹配与
//! `ra-model` 的 chat convert/stream 允许用它把 test-only 入口暴露给宿主 crate。它
//! 不含 `cfg(test`，因此天然不会被误伤。

use crate::gate::Outcome;
use crate::source;

/// 执行门禁。
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

    // Cargo 的集成测试位同样禁止：它绕过 tests/ workspace，且会被主 workspace 构建。
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
