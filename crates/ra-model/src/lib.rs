//! # `ra-model`
//!
//! Provider 实现、流式、重试、usage 明细。
//!
//! 三套线上协议都是第一等公民：OpenAI Responses、OpenAI Chat Completions、
//! Anthropic Messages。它们的能力差异在 [`protocol`] 里显式建模，
//! 上层（`ra-runtime`）不得假设任何一种协议的语义。
//!
//! **边界**：公开面只暴露 provider 构造 / 注册、鉴权与凭据获取（[`openai::auth`]，
//! CLI 的 login 流程要走它）、协议能力、重试事实与 usage；请求 lowering、响应转换、
//! SSE 拼装和厂商错误体映射全部是 crate 内 codec。它不运行 agent loop，也不把线上
//! 协议专有字段放进 `ra-core`。
//!
//! **稳定性分级**：`Evolving`。provider 注册项与 quirks 会随接入的厂商增加而扩，
//! 可加不可删；**各 provider 的 codec 是 `Internal`**——协议 lowering 的中间态泄漏
//! 出去，这一层就重构不动了。

#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(feature = "compat")]
pub mod compat;
pub mod fallback;
#[cfg(feature = "openai")]
pub mod openai;
pub mod protocol;
pub mod provider;
pub mod retry;
pub mod usage;
