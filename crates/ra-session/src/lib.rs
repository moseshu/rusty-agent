//! # `ra-session`
//!
//! The dual-channel rollout event log, `SessionStore`, resume, fork, checkpoint.
//!
//! **Boundary**: it stores and replays session events that are already defined. It runs no model
//! or tool, interprets no product semantics, and does not expose a concrete local storage layout
//! as a `ra-core` contract.
//!
//! **Stability**: `Evolving`. Rollout event payloads and the storage schema may grow but not
//! shrink, and must stay readable across versions — resume depends on it.

pub mod chain;
pub mod checkpoint;
pub mod file_history;
pub mod lite;
pub mod memory;
pub mod mutate;
pub mod resume;
pub mod rollout;
pub mod store;

pub use memory::InMemorySession;
pub use ra_core::session::{Session, SessionId};
pub use rollout::{
    ChildAnchorKind, ROLLOUT_SCHEMA_VERSION, RolloutChildAnchor, RolloutModelUsage, RolloutPayload,
    RolloutReader, RolloutRecord, RolloutSessionMeta, RolloutSidecar, RolloutSummary,
    RolloutTurnContext, RolloutWriter, UnifiedReplayItem, graft_child_transcripts,
};
