//! Dual-channel event log: `session_meta` / `turn_context` / `response_item` / `event_msg`.

pub mod pairing;
pub mod reader;
pub mod reconstruction;
pub mod recorder;
pub mod threads;
pub mod truncation;
pub mod writer;
mod writer_lock;

pub use reader::{RolloutReader, RolloutSummary, UnifiedReplayItem, graft_child_transcripts};
pub use reconstruction::{ReconstructedRun, RolloutReconstruction, reconstruct_history};
pub use recorder::{RolloutFileRecorder, is_persisted_rollout_item};
pub use threads::RolloutThreadDirectory;
pub use truncation::{
    UserMessagePosition, truncate_rollout_after_run, truncate_rollout_before_nth_user_message,
    truncate_rollout_before_run, user_message_positions_in_rollout,
};
pub use writer::{
    ChildAnchorKind, ROLLOUT_SCHEMA_VERSION, RolloutCheckpoint, RolloutChildAnchor,
    RolloutModelUsage, RolloutPayload, RolloutRecord, RolloutSessionMeta, RolloutSidecar,
    RolloutTurnContext, RolloutWriter,
};
