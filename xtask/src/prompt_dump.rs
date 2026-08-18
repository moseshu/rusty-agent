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

/// Path of the committed snapshot, relative to the workspace root.
const SNAPSHOT: &str = "api/prompt-dump.txt";

/// Runs the gate. When `bless` is true it rewrites the snapshot instead of reconciling.
pub(crate) fn run(bless: bool) -> Outcome {
    let root = source::workspace_root();
    let snapshot = PathBuf::from(&root).join(SNAPSHOT);
    if !bless && !snapshot.exists() {
        return Outcome::Fail(vec![format!(
            "缺 prompt 快照 {SNAPSHOT}——跑 `cargo xtask prompt-dump --bless` 生成"
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
        Ok(status) if status.success() && bless => Outcome::pass(format!("已重写 {SNAPSHOT}")),
        Ok(status) if status.success() => {
            Outcome::pass(format!("装配出的稳定前缀与 {SNAPSHOT} 一致"))
        }
        Ok(_) => Outcome::Fail(vec![format!(
            "装配出的稳定前缀与 {SNAPSHOT} 不一致：每一份已缓存的前缀都会因此失效。\
             确认改动是有意的之后，跑 `cargo xtask prompt-dump --bless` 让 diff 进 review"
        )]),
        Err(err) => Outcome::Fail(vec![format!("无法启动 cargo test：{err}")]),
    }
}
