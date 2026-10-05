//! Authoritative conversation session port and session identity.
//!
//! A [`Session`] is the provider-neutral, storage-layout-independent contract for reading and
//! appending authoritative session records ([`RunItem`](crate::item::RunItem)).
//!
//! It is identified by an opaque [`SessionId`].

pub mod callback;
pub mod compaction;
pub mod id;
pub mod interrupt;
pub mod port;
pub mod rollout;
pub mod settings;

pub use callback::SessionInputCallback;
pub use compaction::{
    CompactionSnapshot, CompactionSnapshotReplacement, SessionCompaction, SessionCompactionContext,
    SessionCompactionOutcome,
};
pub use id::SessionId;
pub use interrupt::InterruptedTurnHistoryMarker;
pub use port::Session;
pub use settings::{SessionSettings, resolve_session_limit};
