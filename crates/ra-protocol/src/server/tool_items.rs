//! How a host shows its tools' calls as the items of a turn.
//!
//! Codex's tools announce their own items: its shell reports a command execution, its patch tool
//! a file change, its MCP client a tool call — each built by the tool's runtime from what only that
//! runtime knows — and a function tool with no runtime of its own shows no item at all. A run here
//! records every call alike, as a call and its output, and which of them a product shows, and how,
//! is the product's to say: the server asks the host's [`ThreadToolItems`], and a call it answers
//! nothing for shows no item, as in Codex.
//!
//! A call's item starts when the call is recorded, before its tool runs. Where the tool's runtime
//! reports its own boundaries — a command's start and exit, as host events naming the call — the
//! host may let them drive the item instead, as Codex's tools drive theirs: a command then ends
//! when it exits, however long after its call returned, in the turn it began in.

use std::fmt::Debug;

use ra_core::{
    event::HostEvent,
    item::{ToolCall, ToolCallOutput},
};

use super::protocol::ThreadItem;

/// A tool runtime's item boundary, shared by live delivery and rollout replay.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadToolItemEvent {
    /// Execution started.
    Started(ThreadItem),
    /// Execution finished, possibly after the turn ended.
    Completed(ThreadItem),
}

impl ThreadToolItemEvent {
    pub(crate) const fn item(&self) -> &ThreadItem {
        match self {
            Self::Started(item) | Self::Completed(item) => item,
        }
    }
}

/// Projects tool records and runtime events into the items displayed by a product.
///
/// Projection must be deterministic for live events and replay, with no mutable adapter state.
/// Items carry their tool call's id, as Codex's items do.
pub trait ThreadToolItems: Debug + Send + Sync {
    /// The item `call` shows while it runs — Codex's `item/started` — or `None` if it shows none.
    fn started(&self, call: &ToolCall) -> Option<ThreadItem>;

    /// The item `call` shows once `output` is in — Codex's `item/completed` — or `None` if it
    /// shows none.
    fn completed(&self, call: &ToolCall, output: &ToolCallOutput) -> Option<ThreadItem>;

    /// The item a runtime boundary `event` gives, read against `items`, the items so far of the
    /// turn the event belongs to — or `None` if it gives none. Asked live and again when the
    /// history is read back. Once it gives an item for a call, the call's result no longer
    /// completes or replaces that item.
    fn event(&self, _event: &HostEvent, _items: &[ThreadItem]) -> Option<ThreadToolItemEvent> {
        None
    }
}
