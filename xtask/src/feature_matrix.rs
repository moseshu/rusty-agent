//! `feature-matrix` 门禁：每个声明了 feature 的 crate，在两个极端下都必须编译。
//!
//! # 为什么不能用 `cargo check --workspace --no-default-features`
//!
//! **它不穿透 crate 之间的依赖边。** `--no-default-features` 只作用于命令行选中的
//! 包；`ra-eval → ra-model` 这条边上仍然写着 `default-features = true`，于是
//! `ra-model` 照样带着 `openai` 被编译进来。整个 workspace 一起关默认 feature，看着
//! 覆盖面最大，实际上恰恰漏掉了要验的那一格。
//!
//! 所以这里**逐 crate** 跑 `-p <crate> --no-default-features` —— 只有这种形式才真的
//! 让被测 crate 在零 feature 下过一遍编译器。
//!
//! # 为什么只测两个极端
//!
//! 全排列是 2^n 次编译，`ra-exec` 一个 crate 就 8 种。两个极端（全关 / 全开）能抓住
//! 绝大多数漏掉的 `#[cfg]`：漏加 cfg 的代码在全关时找不到符号，漏写 cfg 的 feature
//! 在全开时撞冲突。默认组合由 `cargo check --workspace` 覆盖，不重复跑。

use std::process::Command;

use crate::gate::Outcome;
use crate::source;

/// 执行门禁。
pub(crate) fn run() -> Outcome {
    let crates: Vec<String> = source::crate_names()
        .into_iter()
        .filter(|name| declares_features(name))
        .collect();

    if crates.is_empty() {
        return Outcome::skip("R0-1", "还没有 crate 声明 feature，无矩阵可测");
    }

    let mut violations = Vec::new();
    for name in &crates {
        for flag in ["--no-default-features", "--all-features"] {
            if let Err(violation) = check(name, flag) {
                violations.push(violation);
            }
        }
    }

    Outcome::from_violations(
        violations,
        format!("{} 个带 feature 的 crate × 全关/全开两个极端", crates.len()),
    )
}

/// crate 是否声明了 `[features]`。没有 feature 的 crate 不需要进矩阵。
fn declares_features(name: &str) -> bool {
    let manifest = source::workspace_root()
        .join("crates")
        .join(name)
        .join("Cargo.toml");
    std::fs::read_to_string(manifest)
        .unwrap_or_default()
        .lines()
        .any(|line| line.trim() == "[features]")
}

fn check(name: &str, flag: &str) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["check", "--quiet", "-p", name, flag])
        .current_dir(source::workspace_root())
        .output();

    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "`cargo check -p {name} {flag}` 未通过：\n{}",
            String::from_utf8_lossy(&output.stderr).trim(),
        )),
        Err(err) => Err(format!("无法为 {name} 启动 cargo：{err}")),
    }
}
