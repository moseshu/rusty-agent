//! # `ra-model`
//!
//! Provider 实现、流式、重试、usage 明细。
//!
//! 三套线上协议都是第一等公民：OpenAI Responses、OpenAI Chat Completions、
//! Anthropic Messages。它们的能力差异在 [`protocol`] 里显式建模，
//! 上层（`ra-runtime`）不得假设任何一种协议的语义。
//!
//! **稳定性分级**：`Evolving`。provider 注册项与 quirks 会随接入的厂商增加而扩，
//! 可加不可删；**各 provider 的 codec 是 `Internal`**——协议 lowering 的中间态泄漏
//! 出去，这一层就重构不动了。

pub mod anthropic;
pub mod compat;
pub mod fallback;
pub mod openai;
pub mod protocol;
pub mod provider;
pub mod retry;
pub mod usage;
