//! `test` 门禁：跑独立测试 workspace。
//!
//! 测试不在主 workspace 里（见[项目结构](../../Docs/Rusty_Agent_Project_Structure.md)
//! §5.5），`cargo test --workspace` 跑不到任何行为断言。这条门禁就是那个入口。
//!
//! `tests/` **不进版本库**，因此在新克隆的仓库与 CI 上它不存在——那种情况跳过而不是
//! 失败。这是本仓库的刻意取舍：断言只在本地与开发者机器上跑。

use std::process::Command;

use crate::gate::Outcome;
use crate::source;

/// 执行门禁。`args` 透传给 `cargo test`（如 `-p it-core`）。
pub(crate) fn run(args: &[String]) -> Outcome {
    let manifest = source::workspace_root().join("tests").join("Cargo.toml");
    if !manifest.is_file() {
        return Outcome::skip(
            "R0-7",
            "tests/ 不在版本库里，本机没有这份 workspace（`git clone` 后需要另行获取）",
        );
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(cargo)
        .arg("test")
        .arg("--manifest-path")
        .arg(&manifest)
        .args(args)
        .current_dir(source::workspace_root())
        .status();

    match status {
        Ok(status) if status.success() => Outcome::pass("测试 workspace 全绿"),
        Ok(status) => Outcome::Fail(vec![format!("cargo test 退出码 {status}")]),
        Err(err) => Outcome::Fail(vec![format!("无法启动 cargo test：{err}")]),
    }
}
