//! Executor filesystem and capability-discovery protocol.
//!
//! **Stability**: `Internal`. Wire fields retain the pinned executor contract.

#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
pub mod capabilities;
mod protocol;
mod rpc;
pub use protocol::*;
pub use rpc::*;

/// Distributed tracing metadata carried by executor requests.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct W3cTraceContext {
    /// Optional W3C trace parent header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    /// Optional W3C trace state header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}
