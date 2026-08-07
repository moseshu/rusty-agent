//! `OpenAI` provider 家族：Responses 与 Chat 两套协议共享鉴权、错误映射与 SSE 基础。

pub mod auth;
pub mod chat;
pub(crate) mod error;
pub mod responses;
pub(crate) mod sse;
