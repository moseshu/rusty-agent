//! # `ra-model`
//!
//! Provider 实现、流式、重试、usage 明细。
//!
//! 三套线上协议都是第一等公民：OpenAI Responses、OpenAI Chat Completions、
//! Anthropic Messages。它们的能力差异在 [`protocol`] 里显式建模，
//! 上层（`ra-runtime`）不得假设任何一种协议的语义。

pub mod anthropic;
pub mod compat;
pub mod fallback;
pub mod openai;
pub mod protocol;
pub mod provider;
pub mod retry;
pub mod usage;
