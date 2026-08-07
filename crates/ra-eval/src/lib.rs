//! # `ra-eval`
//!
//! eval / replay / trace 断言 / 回归飞轮。
//!
//! **边界**：消费框架的公开结果与 trace，不参与线上 run 的控制流，不向框架内核
//! 注入产品特例，也不作为生产持久化后端。
//!
//! **稳定性分级**：`Evolving`。断言与报表 API 随 eval 需求调整。

pub mod assert;
pub mod fixture;
pub mod replay;
pub mod report;
