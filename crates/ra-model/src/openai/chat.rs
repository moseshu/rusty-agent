//! `/v1/chat/completions` protocol implementation. A first-class citizen, not an appendage of the compat layer.

pub(crate) mod convert;
pub(crate) mod reasoning;
pub(crate) mod request;
pub(crate) mod stream;
