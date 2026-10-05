//! # `ra-session`
//!
//! The dual-channel rollout event log, the thread store, resume, fork, checkpoint.
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
#[cfg(feature = "sqlite")]
pub mod sqlite;
pub mod store;

pub use memory::InMemorySession;
pub use ra_core::session::{Session, SessionId};
pub use resume::ResumedThread;
pub use rollout::{
    ChildAnchorKind, ROLLOUT_SCHEMA_VERSION, ReconstructedRun, RolloutCheckpoint,
    RolloutChildAnchor, RolloutFileRecorder, RolloutModelUsage, RolloutPayload, RolloutReader,
    RolloutReconstruction, RolloutRecord, RolloutSessionMeta, RolloutSidecar, RolloutSummary,
    RolloutThreadDirectory, RolloutTurnContext, RolloutWriter, UnifiedReplayItem,
    graft_child_transcripts, is_persisted_rollout_item, reconstruct_history,
    truncate_rollout_after_run, truncate_rollout_before_run,
};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteSession, SqliteSessionBuilder};
pub use store::{
    InMemoryThreadStore, LoadThreadHistoryParams, ReadThreadParams, ResumeThreadParams,
    StoredThread, StoredThreadHistory, ThreadStore,
};
