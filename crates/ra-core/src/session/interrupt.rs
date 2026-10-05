//! The model-visible marker a thread's history carries where a run was interrupted.
//!
//! Ported from Codex's `TurnAborted` context fragment (`core/src/context/turn_aborted.rs`) and its
//! `InterruptedTurnHistoryMarker` (`core/src/tasks/mod.rs`). When Codex interrupts a turn it
//! records, after whatever the turn had produced, a message telling the model that the previous
//! turn was interrupted on purpose and that what it was running may have partly happened; a fork
//! taken from a thread in the middle of a turn appends the same message. Both go through the
//! marker here, so a cancelled run and a fork of a run in progress leave the same history behind.
//!
//! Codex records the message unless its `agents.interrupt_message` setting is off, and words it
//! for the model as user context, or — in a thread running its multi-agent v2 tools — as a
//! developer message. A provider-neutral history has no developer role; the developer form is a
//! [`MessageRole::System`] message here.

use crate::{
    cancel::CancelReason,
    item::{ItemId, Message, MessageRole, RunItem, RunItemKind},
};

/// The guidance the user-context marker carries: Codex's `TurnAborted::INTERRUPTED_GUIDANCE`.
pub const INTERRUPTED_GUIDANCE: &str = "The user interrupted the previous turn on purpose. Any running unified exec processes may still be running in the background. If any tools/commands were aborted, they may have partially executed.";

/// The guidance the developer marker carries: Codex's
/// `TurnAborted::INTERRUPTED_DEVELOPER_GUIDANCE`.
pub const INTERRUPTED_DEVELOPER_GUIDANCE: &str = "The previous turn was interrupted on purpose. Any running unified exec processes may still be running in the background. If any tools/commands were aborted, they may have partially executed.";

/// The item id the marker is recorded under in the run it ends. Item ids are only unique within a
/// run, and a run is interrupted at most once.
pub const INTERRUPTED_TURN_MARKER_ITEM_ID: &str = "turn-aborted";

const START_MARKER: &str = "<turn_aborted>";
const END_MARKER: &str = "</turn_aborted>";

/// Which message, if any, marks an interrupted run in its thread's history: Codex's
/// `InterruptedTurnHistoryMarker`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterruptedTurnHistoryMarker {
    /// No message is recorded; only the run's end says it was cancelled.
    Disabled,
    /// A user message wrapped in `<turn_aborted>` tags, as Codex records outside multi-agent v2.
    #[default]
    ContextualUser,
    /// A system message wrapped in `<turn_aborted>` tags, standing for the developer message Codex
    /// records in a thread running its multi-agent v2 tools.
    Developer,
}

impl InterruptedTurnHistoryMarker {
    /// Codex's `from_config_and_version`: nothing when `enabled` — Codex's
    /// `agents.interrupt_message`, on by default — is off; otherwise the developer form for a
    /// thread in an agent tree, which runs Codex's multi-agent v2 tools, and the user form for any
    /// other.
    #[must_use]
    pub const fn from_settings(enabled: bool, in_agent_tree: bool) -> Self {
        if !enabled {
            Self::Disabled
        } else if in_agent_tree {
            Self::Developer
        } else {
            Self::ContextualUser
        }
    }

    /// The message this marker records, or `None` when it is disabled: Codex's
    /// `interrupted_turn_history_marker`.
    #[must_use]
    pub fn message(self) -> Option<Message> {
        let (role, guidance) = match self {
            Self::Disabled => return None,
            Self::ContextualUser => (MessageRole::User, INTERRUPTED_GUIDANCE),
            Self::Developer => (MessageRole::System, INTERRUPTED_DEVELOPER_GUIDANCE),
        };
        Some(Message::text(
            role,
            format!("{START_MARKER}\n{guidance}\n{END_MARKER}"),
        ))
    }

    /// The record this marker adds to the interrupted run, under
    /// [`INTERRUPTED_TURN_MARKER_ITEM_ID`], or `None` when it is disabled.
    #[must_use]
    pub fn item(self) -> Option<RunItem> {
        self.message().map(|message| {
            RunItem::new(
                ItemId::new(INTERRUPTED_TURN_MARKER_ITEM_ID),
                RunItemKind::Message(message),
            )
        })
    }
}

/// Whether a run cancelled for `reason` was interrupted, and so records the marker: Codex's
/// `TurnAbortReason::Interrupted`, which both its interrupt and its shutdown abort with.
///
/// A run superseded by new input is Codex's `Replaced`, and one stopped by its deadline is closest
/// to its `BudgetLimited`; Codex records no marker for either. Nor is one recorded for a timeout,
/// a failed peer, or a reason this framework or a product defines elsewhere, which Codex has no
/// counterpart of.
#[must_use]
pub const fn is_interrupt(reason: &CancelReason) -> bool {
    matches!(reason, CancelReason::UserInterrupt | CancelReason::Shutdown)
}

/// Whether `message` is an interrupted-run marker, in either form: Codex's
/// `TurnAborted::matches_text`. Like Codex's other context fragments it is not a message from the
/// user, though the user form is sent with the user's role.
#[must_use]
pub fn is_interrupted_turn_marker(message: &Message) -> bool {
    let text = message.text_content();
    let text = text.trim();
    let starts = text
        .get(..START_MARKER.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(START_MARKER));
    let ends = text
        .get(text.len().saturating_sub(END_MARKER.len())..)
        .is_some_and(|end| end.eq_ignore_ascii_case(END_MARKER));
    starts && ends
}
