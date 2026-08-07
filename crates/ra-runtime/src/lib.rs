//! # `ra-runtime`
//!
//! Loop 内核。公开 API 必须能跑任何 agent，不含任何业务词汇。
//!
//! **稳定性分级**：`Runner` 的入口签名是 `Stable`（在 Stable API 清单里），
//! [`turn`] 下的结算中间态是 `Internal`——`ProcessedResponse` / `ToolExecutionPlan` /
//! `SingleStepResult` 随时可重构，不要在外部依赖它们。

pub mod agent;
pub mod budget;
pub mod capability;
pub mod circuit;
pub mod guard;
pub mod hook;
pub mod permission;
pub mod runner;
pub mod tool;
pub mod turn;
