//! `public-api` 门禁：公开 API 的破坏性变更必须是显式的。
//!
//! 拿 `cargo public-api` 导出的当前公开 API 与入库的基线快照逐行对比，有差异就失败，
//! 要求改动者**要么撤销，要么更新基线**——后者是一次显式的、会出现在 diff 里的动作。
//!
//! # 分工
//!
//! 本门禁负责**对账**；基线快照的建立归 R0-8（扩展安全第 7 条）。两个前置任一不满足
//! 就跳过：
//! - `cargo public-api` 未安装（它需要 nightly 的 rustdoc JSON：
//!   `cargo install cargo-public-api` + `rustup toolchain install nightly`）；
//! - `api/<crate>.txt` 基线尚未入库。

use std::path::PathBuf;
use std::process::Command;

use crate::gate::Outcome;
use crate::source;

/// 纳入 API 基线的 crate。**只放对外契约**——`ra-cli` / `xtask` 是二进制，没有下游。
const TRACKED: &[&str] = &["ra-core"];

/// 执行门禁。
pub(crate) fn run() -> Outcome {
    if !tool_available() {
        return Outcome::skip(
            "R0-8",
            "未安装 cargo-public-api（`cargo install cargo-public-api`，需 nightly rustdoc）",
        );
    }

    let baseline_dir = source::workspace_root().join("api");
    let missing: Vec<&str> = TRACKED
        .iter()
        .copied()
        .filter(|name| !baseline_path(&baseline_dir, name).is_file())
        .collect();
    if !missing.is_empty() {
        return Outcome::skip("R0-8", format!("尚无基线快照：{}", missing.join(" / ")));
    }

    let mut violations = Vec::new();
    for name in TRACKED {
        match diff_against_baseline(&baseline_dir, name) {
            Ok(diff) => violations.extend(diff),
            Err(err) => violations.push(format!("{name}：{err}")),
        }
    }

    Outcome::from_violations(
        violations,
        format!("{} 个 crate 的公开 API 与基线一致", TRACKED.len()),
    )
}

fn baseline_path(dir: &std::path::Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.txt"))
}

fn tool_available() -> bool {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    Command::new(cargo)
        .args(["public-api", "--version"])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// 与基线逐行对比，返回差异描述。
fn diff_against_baseline(dir: &std::path::Path, name: &str) -> Result<Vec<String>, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["public-api", "-p", name])
        .current_dir(source::workspace_root())
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }

    let current = String::from_utf8_lossy(&output.stdout);
    let baseline = std::fs::read_to_string(baseline_path(dir, name)).map_err(|e| e.to_string())?;

    let current_items: Vec<&str> = current
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let baseline_items: Vec<&str> = baseline
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let mut diffs = Vec::new();
    for item in &baseline_items {
        if !current_items.contains(item) {
            diffs.push(format!("{name}：公开项消失（破坏性）— {item}"));
        }
    }
    for item in &current_items {
        if !baseline_items.contains(item) {
            diffs.push(format!("{name}：新增公开项，需更新基线 — {item}"));
        }
    }
    Ok(diffs)
}
