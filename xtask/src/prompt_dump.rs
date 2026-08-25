//! The `prompt-dump` gate: the assembled stable prefix agrees with its committed snapshot.
//!
//! The prefix is the span every provider's prompt cache holds, and cache hit rate is the dominant
//! cost driver of a long run. A one-word edit to a section, or two sections swapping order,
//! invalidates that span for every subsequent call — and a prefix that exists only at runtime
//! cannot be reviewed. So the assembled dump is written down and diffed, exactly as `api/*.txt` is
//! for the public surface.
//!
//! **The artifact is the point, not the test run.** A gate that merely executed a test would
//! report `PASS` while producing nothing to diff, and the test workspace already runs it.

use std::path::PathBuf;
use std::process::Command;

use crate::gate::Outcome;
use crate::source;

/// Paths of the committed snapshots, relative to the workspace root.
const SNAPSHOTS: [&str; 2] = ["api/prompt-dump.txt", "api/tool-surface.txt"];

/// Runs the gate. When `bless` is true it rewrites the snapshot instead of reconciling.
pub(crate) fn run(bless: bool) -> Outcome {
    let root = source::workspace_root();
    let missing = SNAPSHOTS
        .iter()
        .find(|snapshot| !PathBuf::from(&root).join(snapshot).is_file());
    if !bless && let Some(snapshot) = missing {
        return Outcome::Fail(vec![format!(
            "缺 prompt 快照 {snapshot}——跑 `cargo xtask prompt-dump --bless` 生成"
        )]);
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut command = Command::new(cargo);
    command
        .arg("test")
        .arg("--manifest-path")
        .arg(PathBuf::from(&root).join("tests/Cargo.toml"))
        .arg("-p")
        .arg("it-coding")
        .arg("--test")
        .arg("prompt_dump")
        .current_dir(&root);
    if bless {
        command.env("BLESS_PROMPT_DUMP", "1");
    }

    match command.status() {
        Ok(status) if status.success() && bless => {
            Outcome::pass(format!("已重写 {} 和 {}", SNAPSHOTS[0], SNAPSHOTS[1]))
        }
        Ok(status) if status.success() => Outcome::pass(format!(
            "稳定前缀与 {}、工具面与 {} 一致",
            SNAPSHOTS[0], SNAPSHOTS[1]
        )),
        Ok(_) => Outcome::Fail(vec![format!(
            "稳定前缀或工具面快照不一致：每一份已缓存的前缀都会因此失效。\
             工具 schema 变更还必须提升 TOOL_SCHEMA_REVISION；确认改动后，运行 \
             `cargo xtask prompt-dump --bless` 让 diff 进 review"
        )]),
        Err(err) => Outcome::Fail(vec![format!("无法启动 cargo test：{err}")]),
    }
}
