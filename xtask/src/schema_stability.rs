//! The byte-stability gate for the model-facing tool table.

use std::{path::PathBuf, process::Command};

use crate::gate::Outcome;
use crate::source;

/// Path of the committed provider-wire snapshot, relative to the workspace root.
const SNAPSHOT: &str = "api/tool-schemas.txt";

/// Runs the real built-in-tool contract instead of reproducing the schemas in this binary.
///
/// `xtask` is an assembler, not a tool host. Keeping the construction in `it-coding` means this
/// gate exercises the same public dependency path as a consumer while preserving the dependency
/// direction that prevents the build helper from becoming a second product runtime.
pub(crate) fn run(bless: bool) -> Outcome {
    let root = source::workspace_root();
    let manifest = PathBuf::from(&root).join("tests").join("Cargo.toml");
    let snapshot = PathBuf::from(&root).join(SNAPSHOT);
    if !manifest.is_file() {
        return Outcome::Fail(vec![format!(
            "{} 不存在，无法运行工具 schema 稳定性契约",
            source::relative(&manifest)
        )]);
    }
    if !bless && !snapshot.is_file() {
        return Outcome::Fail(vec![format!(
            "缺工具 schema 快照 {SNAPSHOT}——跑 `cargo xtask schema-stability --bless` 生成"
        )]);
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut command = Command::new(&cargo);
    command
        .args(["test", "--manifest-path"])
        .arg(&manifest)
        .args(["-p", "it-coding", "--test", "tool_schema_dump"])
        .current_dir(&root);
    if bless {
        command.env("BLESS_TOOL_SCHEMA_DUMP", "1");
    }

    match command.status() {
        Ok(status) if status.success() && bless => Outcome::pass(format!("已重写 {SNAPSHOT}")),
        Ok(status) if status.success() => Outcome::pass(format!(
            "真实 provider 工具 schema 与 {SNAPSHOT} 一致，且连续 100 次渲染字节一致"
        )),
        Ok(status) => Outcome::Fail(vec![format!(
            "provider 工具 schema 与 {SNAPSHOT} 不一致或稳定性契约失败（退出码 {status}）；\
             确认改动有意后运行 `cargo xtask schema-stability --bless` 让 diff 进入 review"
        )]),
        Err(error) => Outcome::Fail(vec![format!("无法启动工具 schema 稳定性契约：{error}")]),
    }
}
