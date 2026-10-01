//! Dual-channel event log: `session_meta` / `turn_context` / `response_item` / `event_msg`.

pub mod pairing;
pub mod reader;
pub mod reconstruction;
pub mod recorder;
pub mod truncation;
pub mod writer;

pub use reader::{RolloutReader, RolloutSummary, UnifiedReplayItem, graft_child_transcripts};
pub use reconstruction::{ReconstructedRun, RolloutReconstruction, reconstruct_history};
pub use recorder::{RolloutFileRecorder, is_persisted_rollout_item};
pub use truncation::{truncate_rollout_after_run, truncate_rollout_before_run};
pub use writer::{
    ChildAnchorKind, ROLLOUT_SCHEMA_VERSION, RolloutCheckpoint, RolloutChildAnchor,
    RolloutModelUsage, RolloutPayload, RolloutRecord, RolloutSessionMeta, RolloutSidecar,
    RolloutTurnContext, RolloutWriter,
};
