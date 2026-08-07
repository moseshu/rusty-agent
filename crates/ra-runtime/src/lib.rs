//! # `ra-runtime`
//!
//! Loop 内核。公开 API 必须能跑任何 agent，不含任何业务词汇。
//!
//! **边界**：只依赖 `ra-core` 的协议中立契约。公开面是 agent 定义、runner 入口、
//! tool profile / registry 与 guard registry；turn 结算、派发、预算执行、熔断和审批
//! 流都是 crate 内实现，不能被下游绕过。
//!
//! **稳定性分级**：`Runner` 的入口签名是 `Stable`（在 Stable API 清单里），
//! turn 结算中间态是 `Internal`——`ProcessedResponse` / `ToolExecutionPlan` /
//! `SingleStepResult` 随时可重构，不要在外部依赖它们。

pub mod agent;
pub(crate) mod budget;
pub(crate) mod capability;
pub(crate) mod circuit;
pub mod guard;
pub(crate) mod hook;
pub(crate) mod permission;
pub mod runner;
pub mod tool;
pub(crate) mod turn;
