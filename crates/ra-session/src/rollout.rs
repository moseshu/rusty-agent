//! Dual-channel event log: `session_meta` / `turn_context` / `response_item` / `event_msg`.

pub mod pairing;
pub mod reader;
pub mod writer;

pub use reader::{RolloutReader, RolloutSummary, UnifiedReplayItem, graft_child_transcripts};
pub use writer::{
    ChildAnchorKind, ROLLOUT_SCHEMA_VERSION, RolloutCheckpoint, RolloutChildAnchor,
    RolloutModelUsage, RolloutPayload, RolloutRecord, RolloutSessionMeta, RolloutSidecar,
    RolloutTurnContext, RolloutWriter,
};
