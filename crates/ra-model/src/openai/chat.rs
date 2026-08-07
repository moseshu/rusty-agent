//! `/v1/chat/completions` 协议实现。第一等公民，不是兼容层的附属品。

pub(crate) mod convert;
pub(crate) mod reasoning;
pub(crate) mod request;
pub(crate) mod stream;
