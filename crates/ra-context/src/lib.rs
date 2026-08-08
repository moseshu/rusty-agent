//! # `ra-context`
//!
//! Context budgeting, compaction, eviction, archiving.
//!
//! **Boundary**: it offers protocol-neutral context transforms only. It issues no model requests,
//! persists no session, and does not decide which business content a product should keep; content
//! policy arrives from the product through public inputs.
//!
//! **Stability**: `Evolving`. Compaction triggers and eviction policy will be tuned against real
//! measurements, so the thresholds are not a contract.

pub mod archive;
pub mod budget;
pub mod compaction;
pub mod eviction;
pub mod preflight;
pub mod window;
