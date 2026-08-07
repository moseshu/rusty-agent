//! # `ra-coding`
//!
//! 编码 agent（产品层）：15 个工具 + 提示词内容 + 编码纪律 + 装配。
//!
//! **稳定性分级**：`Internal`。**它是参考产品，不是框架契约**——提示词、工具集、
//! 纪律判据都随业务改，任何 crate 都不得依赖它（由 layering 门禁强制）。

pub mod capabilities;
pub mod closeout;
pub mod final_answer;
pub mod guards;
pub mod ledger;
pub mod profile;
pub mod prompt;
pub mod tools;
