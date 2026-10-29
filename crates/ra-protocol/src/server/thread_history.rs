//! A thread's turns and items, rebuilt from what its turns recorded: Codex's
//! `ThreadHistoryBuilder` (`app-server-protocol/src/protocol/thread_history.rs`).
//!
//! The same reducer serves both of Codex's uses. Fed a thread's rollout, it rebuilds the turns a
//! client reads back with `thread/turns/list`. Fed a loaded thread's events as its listener hears
//! them, it follows the turn running, so a read can show what the rollout may not hold yet; and the
//! changes each record makes are the `item/started` and `item/completed` the listener sends.
//!
//! # What a turn is made of
//!
//! A record becomes an item as Codex's turn items do (see [`ItemProjector`]): a user message is the
//! user's input, an assistant message the agent's, reasoning stays reasoning, and a compaction is a
//! context compaction. A tool call becomes whatever the host's [`ThreadToolItems`] shows for it.
//! Runtime events can take ownership of its completion; background commands finish on exit,
//! independently of when the model receives their result. What Codex keeps
//! out of a turn's items is kept out here too: system and developer messages, and the marker an
//! interrupt leaves for the model, and selected-skill context fragments.
//!
//! A record recorded again under its id replaces the item it made, in place, as Codex upserts an
//! item by id; the runner records a record again when settlement changes it.
//!
//! # Differences from Codex
//!
//! Codex's rollout holds the events its live thread sent, so replay sees the same events a client
//! saw. A rollout here holds the run's records, and replay derives the same items from them that
//! the listener derives from the thread's events: a run's start opens its turn and its new input is
//! the user's message, under the id the thread reported it with
//! ([`turn_input_item_id`](ra_runtime::thread::turn_input_item_id)); a segment that continues a run
//! after approval continues its turn; its end closes the turn — completed, failed with the error it
//! recorded, or interrupted if it was cancelled — and a run paused for approval keeps its turn in
//! progress. Turn times come from the records' timestamps. A run started by another run records
//! into the rollout only as the run of its own thread — a spawned agent's — so its start opens a
//! turn like any other; an agent used as a tool records nowhere.
//!
//! A tool runtime's own boundaries — the start and exit of a command, correlated with the call
//! that ran it — are kept in the rollout and replayed into the turn of that call, so a command that
//! outlives its call, and even its turn, ends where it began, as Codex's exit watcher updates the
//! original turn. Codex's legacy rollouts drop its exec events and have no command items at all;
//! its paginated rollouts keep the completed items instead. The boundaries are this port's
//! counterpart of the latter, since its history is rebuilt from records rather than kept as items.
//! A call of a tool the client provides records its request and its end the same way, and is
//! replayed from them (see [`super::dynamic_tool_calls`]).
//!
//! Codex's legacy replay gives messages and reasoning sequential ids of its own (`item-1`, …),
//! unlike the ids its live items carry. Records here have ids of their own, and replay uses them, so
//! an item read back has the id it was sent with.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, PoisonError, RwLock},
};

use ra_core::{
    event::HostEvent,
    item::{
        ContentBlock, ImageDetail as CoreImageDetail, Message, MessageRole, ModelInputItem,
        OutputPhase, RunItem, RunItemKind, ToolCall,
    },
    session::{
        contextual::is_contextual_user_message,
        rollout::{RolloutRunEnd, RolloutRunEnded, RolloutRunStarted},
    },
};
use ra_runtime::thread::turn_input_item_id;
use ra_session::{
    rollout::{RolloutPayload, RolloutRecord},
    store::ThreadStore,
};

use super::{
    dynamic_tool_calls::dynamic_tool_item,
    protocol::{
        CommandExecutionStatus, ImageDetail, ImageReference, MessagePhase, PatchApplyStatus,
        ThreadItem, Turn, TurnError, TurnItemsView, TurnStatus, UserInput,
    },
    tool_items::ThreadToolItems,
};

pub(crate) use super::tool_items::ThreadToolItemEvent as ItemChange;

/// Turns records into items: the part of Codex's `parse_turn_item` and of its tools' item events
/// that applies to the records a run here produces.
///
/// A record that is already whole — a message, reasoning, a compaction — starts and completes at
/// once, as Codex reports a user message; recorded again once settlement changes it, it completes
/// again with what it settled as, and does not start twice. A tool call starts its item, and its
/// output completes it, so the projector remembers the calls whose output has not come in.
#[derive(Debug, Default)]
pub(crate) struct ItemProjector {
    calls: HashMap<String, ToolCall>,
    runtime_items: HashSet<String>,
    /// The whole items of the turn already started, so a record recorded again only completes
    /// its item again, with what it settled as.
    started: HashSet<String>,
}

impl ItemProjector {
    /// What `item` does to its turn's items.
    pub(crate) fn project(
        &mut self,
        item: &RunItem,
        tool_items: Option<&dyn ThreadToolItems>,
    ) -> Vec<ItemChange> {
        let id = item.id().as_str();
        let whole_item = match item.kind() {
            RunItemKind::Message(message) => match message.role() {
                MessageRole::User => user_message(id, message),
                MessageRole::Assistant => agent_message(id, message, self.started.contains(id)),
                _ => None,
            },
            RunItemKind::Reasoning(reasoning) => Some(ThreadItem::reasoning(
                id,
                reasoning.summary().to_vec(),
                reasoning.content().to_vec(),
            )),
            RunItemKind::Compaction(_) | RunItemKind::ProviderCompaction(_) => {
                Some(ThreadItem::context_compaction(id))
            }
            _ => None,
        };
        if let Some(whole_item) = whole_item {
            if self.started.insert(id.to_owned()) {
                return vec![
                    ItemChange::Started(whole_item.clone()),
                    ItemChange::Completed(whole_item),
                ];
            }
            return vec![ItemChange::Completed(whole_item)];
        }
        match item.kind() {
            RunItemKind::ToolCall(call) => {
                if self.calls.contains_key(call.call_id().as_str())
                    || self.runtime_items.contains(call.call_id().as_str())
                {
                    return Vec::new();
                }
                self.calls
                    .insert(call.call_id().as_str().to_owned(), call.clone());
                tool_items
                    .and_then(|tool_items| tool_items.started(call))
                    .map(ItemChange::Started)
                    .into_iter()
                    .collect()
            }
            RunItemKind::ToolCallOutput(output) => {
                if self.runtime_items.contains(output.call_id().as_str()) {
                    self.calls.remove(output.call_id().as_str());
                    return Vec::new();
                }
                let Some(call) = self.calls.remove(output.call_id().as_str()) else {
                    return Vec::new();
                };
                tool_items
                    .and_then(|tool_items| tool_items.completed(&call, output))
                    .map(ItemChange::Completed)
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Forgets the calls of a turn that has ended.
    fn clear(&mut self) {
        self.calls.clear();
        self.runtime_items.clear();
        self.started.clear();
    }
}

/// The user's input a user message carries, or `None` for one that is not the user's own: the
/// skill context, the marker an interrupt leaves, or a message with nothing to show.
fn user_message(id: &str, message: &Message) -> Option<ThreadItem> {
    if let Some(fragments) = hook_prompt_fragments(message) {
        return Some(ThreadItem::HookPrompt {
            id: id.to_owned(),
            fragments,
        });
    }
    if is_contextual_user_message(message) {
        return None;
    }
    let content: Vec<UserInput> = message.content().iter().filter_map(user_input).collect();
    (!content.is_empty()).then(|| ThreadItem::user_message(id, content))
}

/// One block of a user message as the input it was given as: the reverse of how a turn's input
/// becomes a message. Codex keeps the client's input as it was sent, with the spans the client
/// marked in its text; a message here keeps only what reaches the model, so the spans are gone.
fn user_input(block: &ContentBlock) -> Option<UserInput> {
    match block {
        ContentBlock::Skill { name, path } => {
            return Some(UserInput::Skill {
                name: name.clone(),
                path: path.clone(),
            });
        }
        ContentBlock::Mention { name, path } => {
            return Some(UserInput::Mention {
                name: name.clone(),
                path: path.clone(),
            });
        }
        _ => {}
    }
    if let Some(text) = block.as_text() {
        return Some(UserInput::text(text));
    }
    let image = block.as_image()?;
    let source = image.source();
    let input = if let Some(path) = source.as_local_path() {
        UserInput::LocalImage {
            detail: None,
            path: path.path().to_path_buf(),
        }
    } else {
        let reference = if let Some(url) = source.as_url() {
            ImageReference::Inline {
                url: url.url().to_owned(),
            }
        } else if let Some(file) = source.as_provider_file() {
            ImageReference::File {
                file_id: file.file_id().to_owned(),
            }
        } else {
            let data = source.as_base64()?;
            ImageReference::Inline {
                url: format!("data:{};base64,{}", data.media_type(), data.data()),
            }
        };
        UserInput::Image {
            image: reference,
            detail: None,
        }
    };
    let detail = image.detail().map(|detail| match detail {
        CoreImageDetail::Low => ImageDetail::Low,
        CoreImageDetail::High => ImageDetail::High,
        _ => ImageDetail::Auto,
    });
    Some(match input {
        UserInput::LocalImage { path, .. } => UserInput::LocalImage { detail, path },
        UserInput::Image { image, .. } => UserInput::Image { image, detail },
        other => other,
    })
}

/// The agent's message. An empty message still completes an item already shown as started.
fn agent_message(id: &str, message: &Message, started: bool) -> Option<ThreadItem> {
    let text = message.text_content();
    if text.is_empty() && !started {
        return None;
    }
    let phase = message.phase().map(|phase| match phase {
        OutputPhase::Commentary => MessagePhase::Commentary,
        _ => MessagePhase::FinalAnswer,
    });
    Some(ThreadItem::agent_message(id, text, phase))
}

/// A turn being rebuilt: Codex's `PendingTurn`.
#[derive(Debug, Clone)]
struct PendingTurn {
    id: String,
    items: Vec<ThreadItem>,
    status: TurnStatus,
    error: Option<TurnError>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    /// When its run started, in milliseconds, for the duration replay computes.
    started_at_ms: Option<u64>,
    /// Opened by a turn's start rather than by a record outside any turn.
    opened_explicitly: bool,
}

impl PendingTurn {
    fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            items: Vec::new(),
            status: TurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            started_at_ms: None,
            opened_explicitly: false,
        }
    }

    /// Adds `item`, or replaces the item with its id in place.
    fn upsert(&mut self, item: ThreadItem) {
        match self
            .items
            .iter_mut()
            .find(|existing| existing.id() == item.id())
        {
            Some(existing) => *existing = item,
            None => self.items.push(item),
        }
    }

    fn snapshot(&self, items_view: TurnItemsView) -> Turn {
        let mut turn = Turn::new(self.id.clone(), self.status)
            .with_items(items_view.project_items(&self.items), items_view)
            .with_started_at(self.started_at);
        if let Some(error) = &self.error {
            turn = turn.with_error(error.clone());
        }
        if let (Some(completed_at), Some(duration_ms)) = (self.completed_at, self.duration_ms) {
            turn = turn.with_completion(completed_at, duration_ms);
        }
        turn
    }
}

/// How a turn ended.
#[derive(Debug, Clone)]
pub(crate) struct TurnEnding {
    pub(crate) status: TurnStatus,
    pub(crate) error: Option<TurnError>,
    pub(crate) completed_at: Option<i64>,
    pub(crate) duration_ms: Option<i64>,
}

/// Rebuilds a thread's turns: Codex's `ThreadHistoryBuilder`.
#[derive(Debug, Default)]
pub(crate) struct ThreadHistoryBuilder {
    turns: Vec<PendingTurn>,
    current_turn: Option<PendingTurn>,
    projector: ItemProjector,
    /// The position of the record being replayed, which names a turn no start opened, as Codex's
    /// replay names one `rollout-<index>`.
    rollout_index: usize,
}

impl ThreadHistoryBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Tracks an incomplete display item without completing it or adding a rollout record.
    pub(crate) fn item_started(&mut self, turn_id: &str, item: &RunItem) -> Vec<ItemChange> {
        let projected = match item.kind() {
            RunItemKind::Message(message) if message.role() == MessageRole::Assistant => {
                Some(ThreadItem::agent_message(
                    item.id().as_str(),
                    message.text_content(),
                    message.phase().map(|phase| match phase {
                        OutputPhase::Commentary => MessagePhase::Commentary,
                        _ => MessagePhase::FinalAnswer,
                    }),
                ))
            }
            RunItemKind::Reasoning(reasoning) => Some(ThreadItem::reasoning(
                item.id().as_str(),
                reasoning.summary().to_vec(),
                reasoning.content().to_vec(),
            )),
            _ => None,
        };
        let Some(projected) = projected else {
            return Vec::new();
        };
        if self.turn_for(Some(turn_id)).is_none() {
            return Vec::new();
        }
        if self.projector.started.insert(item.id().as_str().to_owned()) {
            vec![ItemChange::Started(projected)]
        } else {
            Vec::new()
        }
    }

    /// Finds the displayed command of an execution output event in its original turn.
    pub(crate) fn command_output(&mut self, event: &HostEvent) -> Option<(String, String)> {
        use ra_core::event::{HostEventBody, exec::ExecEvent};
        let HostEventBody::Exec(ExecEvent::Output(output)) = event.body() else {
            return None;
        };
        let turn = self.turn_for(Some(event.run_id().as_str()))?;
        turn.items.iter().find_map(|item| match item {
            ThreadItem::CommandExecution {
                id,
                process_id: Some(process_id),
                ..
            } if process_id == output.session_id().as_str() => {
                Some((id.clone(), output.text().to_owned()))
            }
            _ => None,
        })
    }

    /// Forgets everything: Codex's `reset`.
    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// The turns rebuilt, the one still open last: Codex's `finish`.
    pub(crate) fn finish(mut self) -> Vec<Turn> {
        self.finish_current_turn();
        self.turns
            .iter()
            .map(|turn| turn.snapshot(TurnItemsView::Full))
            .collect()
    }

    /// The turn open, or else the last one, with all its items: Codex's `active_turn_snapshot`.
    pub(crate) fn active_turn_snapshot(&self) -> Option<Turn> {
        self.current_turn
            .as_ref()
            .or_else(|| self.turns.last())
            .map(|turn| turn.snapshot(TurnItemsView::Full))
    }

    /// Whether a turn is open: Codex's `has_active_turn`.
    pub(crate) const fn has_active_turn(&self) -> bool {
        self.current_turn.is_some()
    }

    /// The turn `turn_id` started at `started_at`, in seconds since the Unix epoch: Codex's
    /// `handle_turn_started`. A start of the turn already open continues it, as a run's segment
    /// after approval continues its run.
    pub(crate) fn turn_started(&mut self, turn_id: &str, started_at: Option<i64>) {
        if self
            .current_turn
            .as_ref()
            .is_some_and(|turn| turn.id == turn_id)
        {
            return;
        }
        self.finish_current_turn();
        let mut turn = PendingTurn::new(turn_id);
        turn.status = TurnStatus::InProgress;
        turn.started_at = started_at;
        turn.opened_explicitly = true;
        self.current_turn = Some(turn);
    }

    /// The turn `turn_id` recorded `item` — or, for a record replayed from a rollout, the turn
    /// open — and returns what it did to the turn's items. A record of a turn the builder does not
    /// know is not kept, as Codex drops a turn-scoped item for an unknown turn; its changes are
    /// still returned.
    pub(crate) fn item(
        &mut self,
        turn_id: Option<&str>,
        item: &RunItem,
        tool_items: Option<&dyn ThreadToolItems>,
    ) -> Vec<ItemChange> {
        let changes = self.projector.project(item, tool_items);
        if changes.is_empty() {
            return changes;
        }
        if let Some(turn) = self.turn_for(turn_id) {
            for change in &changes {
                // Codex's legacy history ignores empty agent messages, even when their live
                // lifecycle completes an item that was opened while streaming.
                if matches!(change.item(), ThreadItem::AgentMessage { text, .. } if text.is_empty())
                {
                    continue;
                }
                turn.upsert(change.item().clone());
            }
        } else {
            tracing::warn!(
                item_id = item.id().as_str(),
                "dropping turn-scoped item for unknown turn id `{}`",
                turn_id.unwrap_or_default()
            );
        }
        changes
    }

    /// Projects runtime events into their original turn, including late command exits. The calls
    /// of the client's own tools are the server's to show (see [`super::dynamic_tool_calls`]); the
    /// host's tools show the rest.
    pub(crate) fn host_event(
        &mut self,
        event: &HostEvent,
        tools: Option<&dyn ThreadToolItems>,
    ) -> Vec<ItemChange> {
        let dynamic = dynamic_tool_item(event);
        if dynamic.is_none() && tools.is_none() {
            return Vec::new();
        }
        let Some(turn) = self.turn_for(Some(event.run_id().as_str())) else {
            return Vec::new();
        };
        let Some(change) =
            dynamic.or_else(|| tools.and_then(|tools| tools.event(event, &turn.items)))
        else {
            return Vec::new();
        };
        let existed = turn
            .items
            .iter()
            .any(|item| item.id() == change.item().id());
        turn.upsert(change.item().clone());
        self.projector
            .runtime_items
            .insert(change.item().id().to_owned());
        if existed && matches!(change, ItemChange::Started(_)) {
            Vec::new()
        } else {
            vec![change]
        }
    }

    /// Forgets the finished turns except those with an item still in progress — a command left
    /// running — whose runtime can still report its end into them.
    pub(crate) fn retain_background_turns(&mut self) {
        self.turns
            .retain(|turn| turn.items.iter().any(is_in_progress));
    }

    /// The turn `turn_id` failed with `error`, and will end next: Codex's `handle_error`.
    pub(crate) fn error(&mut self, turn_id: &str, error: TurnError) {
        if let Some(turn) = self.current_turn.as_mut().filter(|turn| turn.id == turn_id) {
            turn.status = TurnStatus::Failed;
            turn.error = Some(error);
        }
    }

    /// The turn `turn_id` ended as `ending` says: Codex's `handle_turn_complete` and
    /// `handle_turn_aborted`. A turn that failed stays failed.
    pub(crate) fn turn_ended(&mut self, turn_id: &str, ending: &TurnEnding) {
        let apply = |turn: &mut PendingTurn| {
            match ending.status {
                TurnStatus::Completed | TurnStatus::InProgress => {
                    if matches!(turn.status, TurnStatus::Completed | TurnStatus::InProgress) {
                        turn.status = TurnStatus::Completed;
                    }
                }
                status => turn.status = status,
            }
            if let Some(error) = ending.error.clone() {
                turn.error = Some(error);
            }
            turn.completed_at = ending.completed_at;
            turn.duration_ms = ending.duration_ms;
        };
        if let Some(turn) = self.current_turn.as_mut().filter(|turn| turn.id == turn_id) {
            apply(turn);
            self.finish_current_turn();
        } else if let Some(turn) = self.turns.iter_mut().find(|turn| turn.id == turn_id) {
            apply(turn);
        } else if let Some(turn) = self.current_turn.as_mut() {
            apply(turn);
            self.finish_current_turn();
        }
    }

    /// Replays one record of the thread's rollout: Codex's `handle_rollout_item`. A record that
    /// cannot be read is skipped, as Codex skips a rollout line it cannot parse.
    pub(crate) fn handle_record(
        &mut self,
        record: &RolloutRecord,
        tool_items: Option<&dyn ThreadToolItems>,
    ) {
        let index = self.rollout_index;
        self.rollout_index += 1;
        let Ok(payload) = record.payload() else {
            tracing::warn!(
                record = record.type_name(),
                "skipping a rollout record that cannot be read"
            );
            return;
        };
        let at_ms = record.at().as_millis();
        match payload {
            RolloutPayload::RunStarted(started) => self.run_started(&started, at_ms, tool_items),
            RolloutPayload::Item(item) => {
                if self.current_turn.is_none() {
                    // A record outside any run, such as the history a fork starts from.
                    self.current_turn = Some(PendingTurn::new(format!("rollout-{index}")));
                }
                self.item(None, &item, tool_items);
            }
            RolloutPayload::RunEnded(ended) => self.run_ended(&ended, at_ms),
            RolloutPayload::Event(event) => {
                self.host_event(&event, tool_items);
            }
            _ => {}
        }
    }

    fn run_started(
        &mut self,
        started: &RolloutRunStarted,
        at_ms: u64,
        tool_items: Option<&dyn ThreadToolItems>,
    ) {
        // A spawned thread records its own runs, even though they name a parent run.
        // Nested agent tools do not inherit the parent rollout recorder.
        let turn_id = started.run_id().as_str();
        let continues = self
            .current_turn
            .as_ref()
            .is_some_and(|turn| turn.id == turn_id);
        self.turn_started(turn_id, Some(seconds(at_ms)));
        if continues {
            // A segment continuing the run after approval; the input of one that supplied its own
            // history is that history, not new input.
            if started.input_is_continuation_base() {
                return;
            }
        } else if let Some(turn) = self.current_turn.as_mut() {
            turn.started_at_ms = Some(at_ms);
        }
        let offset = if continues {
            self.current_turn
                .as_ref()
                .map_or(0, |turn| turn.items.len())
        } else {
            0
        };
        for (index, input) in started.input().iter().enumerate() {
            let ModelInputItem::Message(message) = input else {
                continue;
            };
            let item = RunItem::new(
                turn_input_item_id(turn_id, offset + index),
                RunItemKind::Message(message.clone()),
            );
            self.item(Some(turn_id), &item, tool_items);
        }
    }

    fn run_ended(&mut self, ended: &RolloutRunEnded, at_ms: u64) {
        let turn_id = ended.run_id().as_str();
        let status = match ended.end() {
            // Paused for approval: the turn goes on once it is answered.
            RolloutRunEnd::Interrupted => return,
            RolloutRunEnd::Completed => TurnStatus::Completed,
            RolloutRunEnd::Failed => TurnStatus::Failed,
            _ => TurnStatus::Interrupted,
        };
        let started_at_ms = self
            .current_turn
            .as_ref()
            .filter(|turn| turn.id == turn_id)
            .and_then(|turn| turn.started_at_ms);
        let duration_ms = started_at_ms.map(|started_at_ms| {
            i64::try_from(at_ms.saturating_sub(started_at_ms)).unwrap_or(i64::MAX)
        });
        let error = (status == TurnStatus::Failed)
            .then(|| TurnError::new(ended.error().unwrap_or("the turn failed")));
        self.turn_ended(
            turn_id,
            &TurnEnding {
                status,
                error,
                completed_at: Some(seconds(at_ms)),
                duration_ms,
            },
        );
    }

    /// The turn a record of `turn_id` goes to: Codex's `upsert_item_in_turn_id`, or, for a
    /// record that names no turn, its `upsert_item_in_current_turn`, which opens a turn if none is
    /// open.
    fn turn_for(&mut self, turn_id: Option<&str>) -> Option<&mut PendingTurn> {
        let Some(turn_id) = turn_id else {
            let id = format!("rollout-{}", self.rollout_index);
            return Some(
                self.current_turn
                    .get_or_insert_with(|| PendingTurn::new(id)),
            );
        };
        if self
            .current_turn
            .as_ref()
            .is_some_and(|turn| turn.id == turn_id)
        {
            return self.current_turn.as_mut();
        }
        self.turns.iter_mut().find(|turn| turn.id == turn_id)
    }

    /// Closes the open turn: Codex's `finish_current_turn`, which drops a turn no start opened and
    /// that holds nothing.
    fn finish_current_turn(&mut self) {
        self.projector.clear();
        if let Some(turn) = self.current_turn.take() {
            if turn.items.is_empty() && !turn.opened_explicitly {
                return;
            }
            self.turns.push(turn);
        }
    }
}

/// Whether `item` is still running.
const fn is_in_progress(item: &ThreadItem) -> bool {
    matches!(
        item,
        ThreadItem::CommandExecution {
            status: CommandExecutionStatus::InProgress,
            ..
        } | ThreadItem::FileChange {
            status: PatchApplyStatus::InProgress,
            ..
        }
    )
}

fn seconds(millis: u64) -> i64 {
    i64::try_from(millis / 1000).unwrap_or(i64::MAX)
}

/// Where the server reads its threads' history from, and how it shows their tools' calls: what the
/// host gave [`AppServer::with_thread_store`](super::AppServer::with_thread_store) and
/// [`AppServer::with_thread_tool_items`](super::AppServer::with_thread_tool_items).
#[derive(Default)]
pub(crate) struct HistorySources {
    store: RwLock<Option<Arc<dyn ThreadStore>>>,
    tool_items: RwLock<Option<Arc<dyn ThreadToolItems>>>,
}

impl std::fmt::Debug for HistorySources {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HistorySources")
            .field("has_store", &self.store().is_some())
            .field("tool_items", &self.tool_items())
            .finish()
    }
}

impl HistorySources {
    pub(crate) fn set_store(&self, store: Arc<dyn ThreadStore>) {
        *self.store.write().unwrap_or_else(PoisonError::into_inner) = Some(store);
    }

    pub(crate) fn set_tool_items(&self, tool_items: Arc<dyn ThreadToolItems>) {
        *self
            .tool_items
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(tool_items);
    }

    pub(crate) fn store(&self) -> Option<Arc<dyn ThreadStore>> {
        self.store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn tool_items(&self) -> Option<Arc<dyn ThreadToolItems>> {
        self.tool_items
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn hook_prompt_fragments(message: &Message) -> Option<Vec<super::protocol::HookPromptFragment>> {
    #[derive(serde::Deserialize)]
    struct Fragments {
        #[serde(rename = "hook_prompt")]
        fragments: Vec<Fragment>,
    }
    #[derive(serde::Deserialize)]
    struct Fragment {
        #[serde(rename = "@hook_run_id")]
        hook_run_id: String,
        #[serde(rename = "$text", default)]
        text: String,
    }
    let text = message.text_content();
    if !text.trim_start().starts_with("<hook_prompt ") {
        return None;
    }
    let parsed: Fragments = quick_xml::de::from_str(&format!("<root>{text}</root>")).ok()?;
    if parsed.fragments.is_empty()
        || parsed
            .fragments
            .iter()
            .any(|fragment| fragment.hook_run_id.trim().is_empty())
    {
        return None;
    }
    Some(
        parsed
            .fragments
            .into_iter()
            .map(|fragment| super::protocol::HookPromptFragment {
                text: fragment.text,
                hook_run_id: fragment.hook_run_id,
            })
            .collect(),
    )
}
