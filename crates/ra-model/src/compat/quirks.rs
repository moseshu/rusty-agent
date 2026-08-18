//! Known per-gateway differences: tool-call shape, SSE terminator, missing usage fields, and so on.
//!
//! These are **codec-level** differences — how a response is parsed. Request-field capabilities
//! that the wire protocol cannot answer, such as whether an endpoint accepts a top-level
//! `prompt_cache_key`, live on the provider registration instead, in
//! [`ProviderQuirks`](crate::provider::quirks::ProviderQuirks), because they apply to every
//! protocol and not only to compatible gateways.
