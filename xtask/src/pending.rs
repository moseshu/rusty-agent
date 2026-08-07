//! 被测对象尚未存在的四条门禁。
//!
//! 它们不是「待实现」的占位——**检查逻辑无处施加**：没有工具 schema 就没有字节可比，
//! 没有 prompt 分段就没有 hash 可导。写成假装通过的空函数比不写更糟，所以这里如实
//! 返回 [`Outcome::Skip`] 并带上挡着它的任务号，每次 `cargo xtask all` 都会把它们
//! 列出来。
//!
//! 每条都注明了**启用条件**：那个任务落地时，实现挪进各自的模块，这里删掉一条。

use crate::gate::Outcome;

/// 工具 schema 字节级稳定。
///
/// 启用条件：R2-9 有可渲染的 `ToolSchema`。届时的检查是——同一配置连续渲染 100 次，
/// 字节全同（实证：Codex 16 个工具 schema 共 19,786 B，82 个请求里一字节不差）。
pub(crate) fn schema_stability() -> Outcome {
    Outcome::skip("R2-9", "尚无可渲染的工具 schema")
}

/// prompt 分段、hash、token、缓存断点的导出与对账。
///
/// 启用条件：R4 的 `PromptSection` 装配可用。届时导出各 provider 的分段快照，
/// 稳定前缀变了就要在 diff 里看得见——缓存命中率是成本主因。
pub(crate) fn prompt_dump() -> Outcome {
    Outcome::skip("R4", "尚无 prompt 装配可导出")
}

/// `Guard_Registry.md` 与代码一致，且硬阻断 guard ≤ 8。
///
/// 启用条件：R7-0 建立 guard 登记表。上限 8 是从实测来的——AgentForge 堆到 44 个
/// guard 仍不如 Codex 的零硬 gate，guard 是安全网不是行为塑造工具。
pub(crate) fn guard_registry() -> Outcome {
    Outcome::skip("R7-0", "尚无 guard 登记表")
}

/// 工具 schema ≤ 20 KB、各 prompt 段 token 上限。
///
/// 启用条件：R2-9 与 R4 都落地（两者都是被测对象）。口径是**单次请求 advertise**
/// 的那一份，不是全量可达工具数。
pub(crate) fn token_budget() -> Outcome {
    Outcome::skip("R2-9 / R4", "尚无 schema 与 prompt 可计量")
}
