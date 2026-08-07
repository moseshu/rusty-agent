//! # `ra-coding`
//!
//! 编码 agent（产品层）：15 个工具 + 提示词内容 + 编码纪律 + 装配。
//!
//! **边界**：这是框架的消费者，只通过其他 crate 的公开 API 完成装配。它不定义
//! 可复用框架契约；下游最终只消费 `build_agent` 一类装配入口，模块树全部为 crate 内部。
//!
//! **稳定性分级**：`Internal`。**它是参考产品，不是框架契约**——提示词、工具集、
//! 纪律判据都随业务改，任何 crate 都不得依赖它（由 layering 门禁强制）。

pub(crate) mod capabilities;
pub(crate) mod closeout;
pub(crate) mod final_answer;
pub(crate) mod guards;
pub(crate) mod ledger;
pub(crate) mod profile;
pub(crate) mod prompt;
pub(crate) mod tools;
