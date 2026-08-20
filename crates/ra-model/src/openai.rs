//! The `OpenAI` provider family: the Responses and Chat protocols share auth, error mapping, and SSE plumbing.

pub mod auth;
pub mod chat;
pub(crate) mod content;
pub(crate) mod error;
pub mod responses;
pub(crate) mod sse;
