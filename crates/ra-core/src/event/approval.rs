//! Host events recording how the host answered a tool call's approval.
//!
//! A run that stops on an approval keeps the question in its checkpoint, and the host's answer
//! settles it there. What the answer was is a fact about the call that outlives the checkpoint: a
//! host shows a declined call as declined, and must show it the same way when the thread is read
//! back, even when the turn was cancelled and the call never produced a result. The answer is
//! therefore recorded beside the run's other host events, which the rollout keeps.
//!
//! The resolution says how the question was settled, not what the host does next: a declined call
//! is still refused through the run's own rejection, and a cancelled one is stopped by cancelling
//! its run.

use serde::{Deserialize, Serialize};

use crate::{
    compat::{SchemaVersion, Unknown},
    item::CallId,
};

/// The current schema version for approval events.
pub const APPROVAL_EVENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

const fn default_schema_version() -> SchemaVersion {
    APPROVAL_EVENT_SCHEMA_VERSION
}

/// How the host settled a call's approval.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolution {
    /// The call was approved.
    Approved,
    /// Whoever was asked refused the call.
    Declined,
    /// Whoever was asked cancelled the call, and with it the run.
    Cancelled,
    /// No answer could be obtained, and the call was refused for it.
    Failed,
}

/// The host's answer to one call's approval.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalEvent {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    call_id: CallId,
    resolution: ApprovalResolution,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ApprovalEvent {
    /// The approval of `call_id` was settled as `resolution`.
    #[must_use]
    pub fn resolved(call_id: CallId, resolution: ApprovalResolution) -> Self {
        Self {
            schema_version: APPROVAL_EVENT_SCHEMA_VERSION,
            call_id,
            resolution,
            unknown: Unknown::new(),
        }
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// The call whose approval was settled.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// How it was settled.
    #[must_use]
    pub const fn resolution(&self) -> ApprovalResolution {
        self.resolution
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}
