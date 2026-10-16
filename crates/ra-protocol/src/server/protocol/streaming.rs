//! Incremental item notifications ported from Codex protocol/v2/item.rs.

use serde::{Deserialize, Serialize};

/// The method of `item/agentMessage/delta`.
pub const AGENT_MESSAGE_DELTA_METHOD: &str = "item/agentMessage/delta";

/// Codex's `AgentMessageDeltaNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageDeltaNotification {
    thread_id: String,
    turn_id: String,
    item_id: String,
    delta: String,
}

impl AgentMessageDeltaNotification {
    pub(crate) fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        delta: impl Into<String>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            delta: delta.into(),
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// Text appended by this event.
    #[must_use]
    pub fn delta(&self) -> &str {
        &self.delta
    }
}

/// The method of `item/reasoning/summaryTextDelta`.
pub const REASONING_SUMMARY_TEXT_DELTA_METHOD: &str = "item/reasoning/summaryTextDelta";

/// Codex's `ReasoningSummaryTextDeltaNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningSummaryTextDeltaNotification {
    thread_id: String,
    turn_id: String,
    item_id: String,
    delta: String,
    summary_index: i64,
}

impl ReasoningSummaryTextDeltaNotification {
    pub(crate) fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        delta: impl Into<String>,
        summary_index: i64,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            delta: delta.into(),
            summary_index,
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// Text appended by this event.
    #[must_use]
    pub fn delta(&self) -> &str {
        &self.delta
    }

    /// Zero-based part index.
    #[must_use]
    pub const fn summary_index(&self) -> i64 {
        self.summary_index
    }
}

/// The method of `item/reasoning/summaryPartAdded`.
pub const REASONING_SUMMARY_PART_ADDED_METHOD: &str = "item/reasoning/summaryPartAdded";

/// Codex's `ReasoningSummaryPartAddedNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningSummaryPartAddedNotification {
    thread_id: String,
    turn_id: String,
    item_id: String,
    summary_index: i64,
}

impl ReasoningSummaryPartAddedNotification {
    pub(crate) fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        summary_index: i64,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            summary_index,
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// Zero-based part index.
    #[must_use]
    pub const fn summary_index(&self) -> i64 {
        self.summary_index
    }
}

/// The method of `item/reasoning/textDelta`.
pub const REASONING_TEXT_DELTA_METHOD: &str = "item/reasoning/textDelta";

/// Codex's `ReasoningTextDeltaNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningTextDeltaNotification {
    thread_id: String,
    turn_id: String,
    item_id: String,
    delta: String,
    content_index: i64,
}

impl ReasoningTextDeltaNotification {
    pub(crate) fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        delta: impl Into<String>,
        content_index: i64,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            delta: delta.into(),
            content_index,
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// Text appended by this event.
    #[must_use]
    pub fn delta(&self) -> &str {
        &self.delta
    }

    /// Zero-based part index.
    #[must_use]
    pub const fn content_index(&self) -> i64 {
        self.content_index
    }
}

/// The method of `item/commandExecution/outputDelta`.
pub const COMMAND_EXECUTION_OUTPUT_DELTA_METHOD: &str = "item/commandExecution/outputDelta";

/// Codex's `CommandExecutionOutputDeltaNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionOutputDeltaNotification {
    thread_id: String,
    turn_id: String,
    item_id: String,
    delta: String,
}

impl CommandExecutionOutputDeltaNotification {
    pub(crate) fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        delta: impl Into<String>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item_id: item_id.into(),
            delta: delta.into(),
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// Text appended by this event.
    #[must_use]
    pub fn delta(&self) -> &str {
        &self.delta
    }
}
