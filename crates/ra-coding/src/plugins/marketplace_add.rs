// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Error and source declarations reserved for explicit marketplace registration.
//! Source parsing is available to internal consumers; no add or configuration-write operation is installed.
//! The public error type follows the upstream registration API and is shared with the test bridge.
//! Registration consumers will reuse this declaration when their public operations are introduced.

mod source;
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use source::test_api;
#[allow(
    unused_imports,
    reason = "Reexports are retained for marketplace policy and registration consumers not wired yet"
)]
pub(crate) use source::{MarketplaceSource, parse_marketplace_source};

#[derive(Debug, thiserror::Error)]
/// Source parsing or future registration failure.
pub enum MarketplaceAddError {
    #[error("{0}")]
    /// The original source or layout validation diagnostic.
    InvalidRequest(String),
    #[error("{0}")]
    /// A host filesystem or environment failure.
    Internal(String),
}
