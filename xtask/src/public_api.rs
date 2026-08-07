//! `public-api` 门禁：公开面契约。
//!
//! 查三件事，都是「下游能不能安全依赖这个 crate」的组成部分：
//!
//! 1. **基线对账**——公开项被删、签名变了、悄悄多了一个，都要在 diff 里现形；
//! 2. **扩展安全 ①②**——公开枚举 `#[non_exhaustive]`、公开结构体无公开字段；
//! 3. **稳定性分级**——每个 crate 的 `lib.rs` 标注 `Stable` / `Evolving` / `Internal`。
//!
//! 基线变更**不是错误，是需要被看见的决定**：改完跑 `cargo xtask public-api --bless`
//! 重新生成，让那份 diff 出现在 code review 里。门禁拦的是「悄悄变了」，不是「变了」。

use crate::api;
use crate::extension_safety;
use crate::gate::Outcome;

/// 纳入公开面契约的 crate。
///
/// 只放**下游会依赖**的。`ra-cli` / `xtask` 是二进制，`ra-coding` 是参考产品——
/// 产品的公开面不是框架契约，它随业务改是正常的。
const TRACKED: &[&str] = &[
    "ra-core",
    "ra-macros",
    "ra-model",
    "ra-prompt",
    "ra-context",
    "ra-runtime",
    "ra-session",
    "ra-exec",
    "ra-mcp",
    "ra-protocol",
    "ra-eval",
    "ra-patch",
];

/// 执行门禁。`bless` 为真时重写基线而不是对账。
pub(crate) fn run(bless: bool) -> Outcome {
    let mut violations = Vec::new();
    let mut blessed = 0_usize;
    let mut missing = Vec::new();

    for name in TRACKED {
        let items = match api::snapshot(name) {
            Ok(items) => items,
            Err(err) => {
                violations.push(format!("{name}：{err}"));
                continue;
            }
        };

        if bless {
            match api::write_baseline(name, &items) {
                Ok(()) => blessed += 1,
                Err(err) => violations.push(format!("{name}：写基线失败 {err}")),
            }
            continue;
        }

        match api::read_baseline(name) {
            Some(baseline) => violations.extend(diff(name, &baseline, &items)),
            None => missing.push(*name),
        }
    }

    if bless {
        return Outcome::from_violations(violations, format!("已重写 {blessed} 份基线"));
    }

    if !missing.is_empty() {
        violations.push(format!(
            "缺基线快照：{}——跑 `cargo xtask public-api --bless` 生成",
            missing.join(" / ")
        ));
    }

    for name in TRACKED {
        violations.extend(extension_safety::check(name));
    }

    Outcome::from_violations(
        violations,
        format!("{} 个 crate 的公开面与基线一致", TRACKED.len()),
    )
}

/// 逐行对账。**删除与新增分开报**：前者是破坏性的，后者只是要更新基线。
fn diff(name: &str, baseline: &[String], current: &[String]) -> Vec<String> {
    let mut violations = Vec::new();

    for item in baseline {
        if !current.contains(item) {
            violations.push(format!(
                "{name}：公开项消失或签名改变（**破坏性**）— {item}"
            ));
        }
    }
    for item in current {
        if !baseline.contains(item) {
            violations.push(format!("{name}：新增公开项，需 --bless 更新基线 — {item}"));
        }
    }

    violations
}
