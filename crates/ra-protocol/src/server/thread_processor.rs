//! The thread lifecycle methods: the parts of Codex's `ThreadRequestProcessor`
//! (`request_processors/thread_processor.rs`) for `thread/unsubscribe`,
//! `thread/backgroundTerminals/*` and the history reads `thread/turns/list` and
//! `thread/items/list`, and its cleanup when a connection closes or the server stops.
//!
//! # Reading history
//!
//! `thread/turns/list` takes the path Codex takes for a thread in its legacy history mode: it loads
//! the thread's whole rollout from the server's store, rebuilds its turns (see
//! [`super::thread_history`]), lays the running turn of a loaded thread over them — the rollout may
//! not hold all of it yet — and pages the result by turn. A turn still in progress in the rollout of
//! a thread that is not running a turn was cut off, and is reported interrupted. A thread the store
//! does not hold is read from nowhere: Codex reads a loaded thread's own history then, which a
//! thread here does not keep, so it answers as Codex answers for a thread that keeps none, an
//! ephemeral one.
//!
//! `thread/items/list` reads Codex's paginated history, a projection its legacy mode does not have;
//! like Codex's for a legacy thread, it answers that it is not supported.
//!
//! Unsubscribing removes only the calling connection's subscription; the thread stays loaded, its
//! turn keeps running, and its listener unloads it once it has been idle with no subscribers for
//! the server's unload delay (see [`super::thread_listener`]). A closed connection loses its
//! subscriptions the same way. The thread's server requests stay with the thread — a client that
//! subscribes again is sent those still waiting — and are withdrawn only when the thread unloads.
//!
//! # Differences from Codex
//!
//! Codex's terminal ids are integers, which it parses and pages through numerically; a terminal
//! here is named by whatever the host names it by — an execution session, whose ids are
//! time-ordered — so ids are opaque strings, sorted and compared as strings for paging, and an id
//! that names no running terminal is simply not terminated rather than refused as malformed.

use std::{collections::HashSet, sync::Arc, time::Duration};

use ra_core::{error::SessionErrorKind, session::SessionId};
use ra_session::{rollout::RolloutRecord, store::LoadThreadHistoryParams};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::{
    outgoing::{ConnectionRequestId, OutgoingMessageSender},
    protocol::{
        SortDirection, ThreadBackgroundTerminal, ThreadBackgroundTerminalsCleanParams,
        ThreadBackgroundTerminalsCleanResponse, ThreadBackgroundTerminalsListParams,
        ThreadBackgroundTerminalsListResponse, ThreadBackgroundTerminalsTerminateParams,
        ThreadBackgroundTerminalsTerminateResponse, ThreadItemsListCursor, ThreadItemsListParams,
        ThreadTurnsListParams, ThreadTurnsListResponse, ThreadUnsubscribeParams,
        ThreadUnsubscribeResponse, ThreadUnsubscribeStatus, Turn, TurnItemsView, TurnStatus,
    },
    thread_history::{HistorySources, ThreadHistoryBuilder},
    thread_manager::{LoadedThread, ThreadManager},
    turn_processor::TurnMethodResult,
};
use crate::{frame::JsonRpcErrorObject, transport::ConnectionId};

/// How long the server waits for its threads to shut down when it stops: Codex's
/// `shutdown_all_threads_bounded`.
const SHUTDOWN_THREADS_TIMEOUT: Duration = Duration::from_secs(10);

/// How many turns a page holds unless asked otherwise: Codex's `THREAD_TURNS_DEFAULT_LIMIT`.
const THREAD_TURNS_DEFAULT_LIMIT: usize = 25;

/// The most turns a page holds: Codex's `THREAD_TURNS_MAX_LIMIT`.
const THREAD_TURNS_MAX_LIMIT: usize = 100;

/// Handles the thread lifecycle methods.
#[derive(Debug)]
pub(crate) struct ThreadRequestProcessor {
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    pending_thread_unloads: Arc<Mutex<HashSet<SessionId>>>,
}

impl ThreadRequestProcessor {
    pub(crate) const fn new(
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        pending_thread_unloads: Arc<Mutex<HashSet<SessionId>>>,
    ) -> Self {
        Self {
            thread_manager,
            outgoing,
            pending_thread_unloads,
        }
    }

    /// `thread/unsubscribe`: Codex's `thread_unsubscribe`.
    pub(crate) async fn thread_unsubscribe(
        &self,
        request_id: &ConnectionRequestId,
        params: &ThreadUnsubscribeParams,
    ) -> TurnMethodResult {
        let thread_id = SessionId::new(params.thread_id());
        if self.thread_manager.get_thread(&thread_id).is_none() {
            self.finalize_thread_teardown(&thread_id).await;
            return response(ThreadUnsubscribeResponse::new(
                ThreadUnsubscribeStatus::NotLoaded,
            ));
        }
        let was_subscribed = self
            .outgoing
            .thread_state()
            .unsubscribe_connection_from_thread(&thread_id, request_id.connection_id);
        response(ThreadUnsubscribeResponse::new(if was_subscribed {
            ThreadUnsubscribeStatus::Unsubscribed
        } else {
            ThreadUnsubscribeStatus::NotSubscribed
        }))
    }

    /// `thread/turns/list`: Codex's `thread_turns_list`, on its legacy history.
    pub(crate) async fn thread_turns_list(
        &self,
        params: ThreadTurnsListParams,
        history: &HistorySources,
    ) -> TurnMethodResult {
        let (thread_id, cursor, limit, sort_direction, items_view) = params.into_parts();
        let session_id = SessionId::new(&thread_id);
        let records = self
            .load_thread_turns_list_history(&session_id, history)
            .await?;
        let loaded = self.thread_manager.get_thread(&session_id);
        let has_live_running_thread = loaded.as_ref().is_some_and(|loaded| loaded.is_running());
        let active_turn = if loaded.is_some() {
            self.outgoing
                .thread_state()
                .thread_state(&session_id)
                .lock()
                .await
                .active_turn_snapshot()
        } else {
            None
        };
        let tool_items = history.tool_items();
        let mut builder = ThreadHistoryBuilder::new();
        for record in &records {
            builder.handle_record(record, tool_items.as_deref());
        }
        let mut turns = builder.finish();
        let has_live_in_progress_turn = has_live_running_thread
            || active_turn
                .as_ref()
                .is_some_and(|turn| turn.status() == TurnStatus::InProgress);
        normalize_thread_turns_status(&mut turns, has_live_in_progress_turn);
        if let Some(active_turn) = active_turn {
            turns.retain(|turn| turn.id() != active_turn.id());
            turns.push(active_turn);
        }
        let items_view = items_view.unwrap_or(TurnItemsView::Summary);
        let turns = turns
            .into_iter()
            .map(|turn| turn.in_items_view(items_view))
            .collect();
        let page = paginate_thread_turns(
            turns,
            cursor.as_deref(),
            limit,
            sort_direction.unwrap_or(SortDirection::Desc),
        )?;
        response(ThreadTurnsListResponse::new(
            page.turns,
            page.next_cursor,
            page.backwards_cursor,
        ))
    }

    /// `thread/items/list`: Codex's `thread_items_list`, for a store that keeps no item
    /// projection.
    pub(crate) fn thread_items_list(params: &ThreadItemsListParams) -> TurnMethodResult {
        let has_anchor = matches!(params.cursor(), Some(ThreadItemsListCursor::Anchor(_)));
        if has_anchor && params.turn_id().is_none_or(str::is_empty) {
            return Err(JsonRpcErrorObject::invalid_params(
                "turnId is required when cursor is an item anchor",
            ));
        }
        Err(JsonRpcErrorObject::method_not_found(
            "thread/items/list is not supported yet",
        ))
    }

    /// The rollout `thread/turns/list` rebuilds turns from: Codex's
    /// `load_thread_turns_list_history`.
    async fn load_thread_turns_list_history(
        &self,
        session_id: &SessionId,
        history: &HistorySources,
    ) -> Result<Vec<RolloutRecord>, JsonRpcErrorObject> {
        if let Some(store) = history.store() {
            let params = LoadThreadHistoryParams::new(session_id.clone()).including_archived();
            match store.load_history(&params).await {
                Ok(history) => return Ok(history.into_records()),
                Err(ra_core::error::Error::Session {
                    kind: SessionErrorKind::NotFound,
                    ..
                }) => {}
                Err(error) => {
                    return Err(JsonRpcErrorObject::internal_error(format!(
                        "failed to read thread: {error}"
                    )));
                }
            }
        }
        if self.thread_manager.get_thread(session_id).is_none() {
            return Err(JsonRpcErrorObject::invalid_request(format!(
                "thread not loaded: {session_id}"
            )));
        }
        Err(JsonRpcErrorObject::invalid_request(
            "ephemeral threads do not support thread/turns/list",
        ))
    }

    /// `thread/backgroundTerminals/clean`: Codex's `thread_background_terminals_clean`.
    pub(crate) async fn thread_background_terminals_clean(
        &self,
        params: &ThreadBackgroundTerminalsCleanParams,
    ) -> TurnMethodResult {
        let thread = self.load_thread(params.thread_id())?;
        if let Some(terminals) = &thread.terminals {
            terminals.clean().await;
        }
        response(ThreadBackgroundTerminalsCleanResponse::new())
    }

    /// `thread/backgroundTerminals/list`: Codex's `thread_background_terminals_list`.
    pub(crate) async fn thread_background_terminals_list(
        &self,
        params: ThreadBackgroundTerminalsListParams,
    ) -> TurnMethodResult {
        let (thread_id, cursor, limit) = params.into_parts();
        let thread = self.load_thread(&thread_id)?;
        let mut terminals: Vec<_> = match &thread.terminals {
            Some(terminals) => terminals
                .list()
                .await
                .into_iter()
                .map(ThreadBackgroundTerminal::new)
                .collect(),
            None => Vec::new(),
        };
        // Codex's process manager lists them by id, which the cursor then compares against.
        terminals.sort_by(|left, right| left.process_id().cmp(right.process_id()));
        let (data, next_cursor) = paginate_background_terminals(&terminals, cursor, limit);
        response(ThreadBackgroundTerminalsListResponse::new(
            data,
            next_cursor,
        ))
    }

    /// `thread/backgroundTerminals/terminate`: Codex's `thread_background_terminals_terminate`.
    pub(crate) async fn thread_background_terminals_terminate(
        &self,
        params: &ThreadBackgroundTerminalsTerminateParams,
    ) -> TurnMethodResult {
        let thread = self.load_thread(params.thread_id())?;
        let terminated = match &thread.terminals {
            Some(terminals) => terminals.terminate(params.process_id()).await,
            None => false,
        };
        response(ThreadBackgroundTerminalsTerminateResponse::new(terminated))
    }

    /// Forgets a closed connection's subscriptions: Codex's `connection_closed`. A thread it
    /// leaves without subscribers that is no longer loaded has what is left of it torn down.
    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        let thread_ids = self
            .outgoing
            .thread_state()
            .remove_connection(connection_id);
        for thread_id in thread_ids {
            if self.thread_manager.get_thread(&thread_id).is_none() {
                self.finalize_thread_teardown(&thread_id).await;
            }
        }
    }

    /// Shuts every loaded thread down as the server stops: Codex's `shutdown_threads`.
    pub(crate) async fn shutdown_threads(&self) {
        let threads = self.thread_manager.threads();
        let shutdowns = threads.iter().map(LoadedThread::shutdown);
        if tokio::time::timeout(
            SHUTDOWN_THREADS_TIMEOUT,
            futures::future::join_all(shutdowns),
        )
        .await
        .is_err()
        {
            tracing::warn!("timed out waiting for threads to shut down; proceeding");
        }
    }

    /// Clears what the server kept of a thread that is gone: Codex's `finalize_thread_teardown`.
    async fn finalize_thread_teardown(&self, thread_id: &SessionId) {
        self.pending_thread_unloads.lock().await.remove(thread_id);
        self.outgoing.cancel_requests_for_thread(thread_id, None);
        self.outgoing
            .thread_state()
            .remove_thread_state(thread_id)
            .await;
    }

    fn load_thread(&self, thread_id: &str) -> Result<LoadedThread, JsonRpcErrorObject> {
        self.thread_manager
            .get_thread(&SessionId::new(thread_id))
            .ok_or_else(|| {
                JsonRpcErrorObject::invalid_request(format!("thread not found: {thread_id}"))
            })
    }
}

/// One page of `terminals`: Codex's `paginate_background_terminals`, on string ids.
fn paginate_background_terminals(
    terminals: &[ThreadBackgroundTerminal],
    cursor: Option<String>,
    limit: Option<u32>,
) -> (Vec<ThreadBackgroundTerminal>, Option<String>) {
    let start = cursor.map_or(0, |cursor| {
        terminals
            .iter()
            .position(|terminal| terminal.process_id() > cursor.as_str())
            .unwrap_or(terminals.len())
    });
    let effective_limit = limit
        .map_or(terminals.len(), |limit| {
            usize::try_from(limit).unwrap_or(usize::MAX)
        })
        .max(1);
    let end = start.saturating_add(effective_limit).min(terminals.len());
    let next_cursor = (end < terminals.len()).then(|| terminals[end - 1].process_id().to_owned());
    (terminals[start..end].to_vec(), next_cursor)
}

/// Reports the turns still in progress as interrupted unless the thread is running one: Codex's
/// `normalize_thread_turns_status`. A turn in progress in the rollout of a thread that is not
/// running one was cut off.
fn normalize_thread_turns_status(turns: &mut [Turn], has_live_in_progress_turn: bool) {
    if has_live_in_progress_turn {
        return;
    }
    for turn in turns {
        if turn.status() == TurnStatus::InProgress {
            turn.set_status(TurnStatus::Interrupted);
        }
    }
}

/// A page of turns and the cursors around it: Codex's `ThreadTurnsPage`.
struct ThreadTurnsPage {
    turns: Vec<Turn>,
    next_cursor: Option<String>,
    backwards_cursor: Option<String>,
}

/// Where a page of turns starts: Codex's `ThreadTurnsCursor`, written as JSON.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadTurnsCursor {
    turn_id: String,
    include_anchor: bool,
}

/// One page of `turns`, oldest first, in `sort_direction`: Codex's `paginate_thread_turns`.
fn paginate_thread_turns(
    turns: Vec<Turn>,
    cursor: Option<&str>,
    limit: Option<u32>,
    sort_direction: SortDirection,
) -> Result<ThreadTurnsPage, JsonRpcErrorObject> {
    if turns.is_empty() {
        return Ok(ThreadTurnsPage {
            turns: Vec::new(),
            next_cursor: None,
            backwards_cursor: None,
        });
    }

    let anchor = cursor.map(parse_thread_turns_cursor).transpose()?;
    let page_size = limit
        .map_or(THREAD_TURNS_DEFAULT_LIMIT, |limit| {
            usize::try_from(limit).unwrap_or(usize::MAX)
        })
        .clamp(1, THREAD_TURNS_MAX_LIMIT);

    let anchor_index = anchor
        .as_ref()
        .and_then(|anchor| turns.iter().position(|turn| turn.id() == anchor.turn_id));
    if anchor.is_some() && anchor_index.is_none() {
        return Err(JsonRpcErrorObject::invalid_request(
            "invalid cursor: anchor turn is no longer present",
        ));
    }

    let mut keyed_turns: Vec<_> = turns.into_iter().enumerate().collect();
    match sort_direction {
        SortDirection::Desc => {
            keyed_turns.reverse();
            if let (Some(anchor), Some(anchor_index)) = (anchor.as_ref(), anchor_index) {
                keyed_turns.retain(|(index, _)| {
                    if anchor.include_anchor {
                        *index <= anchor_index
                    } else {
                        *index < anchor_index
                    }
                });
            }
        }
        _ => {
            if let (Some(anchor), Some(anchor_index)) = (anchor.as_ref(), anchor_index) {
                keyed_turns.retain(|(index, _)| {
                    if anchor.include_anchor {
                        *index >= anchor_index
                    } else {
                        *index > anchor_index
                    }
                });
            }
        }
    }

    let more_turns_available = keyed_turns.len() > page_size;
    keyed_turns.truncate(page_size);
    let backwards_cursor = keyed_turns
        .first()
        .map(|(_, turn)| serialize_thread_turns_cursor(turn.id(), true))
        .transpose()?;
    let next_cursor = if more_turns_available {
        keyed_turns
            .last()
            .map(|(_, turn)| serialize_thread_turns_cursor(turn.id(), false))
            .transpose()?
    } else {
        None
    };
    let turns = keyed_turns.into_iter().map(|(_, turn)| turn).collect();

    Ok(ThreadTurnsPage {
        turns,
        next_cursor,
        backwards_cursor,
    })
}

fn serialize_thread_turns_cursor(
    turn_id: &str,
    include_anchor: bool,
) -> Result<String, JsonRpcErrorObject> {
    serde_json::to_string(&ThreadTurnsCursor {
        turn_id: turn_id.to_owned(),
        include_anchor,
    })
    .map_err(|error| {
        JsonRpcErrorObject::internal_error(format!("failed to serialize cursor: {error}"))
    })
}

fn parse_thread_turns_cursor(cursor: &str) -> Result<ThreadTurnsCursor, JsonRpcErrorObject> {
    serde_json::from_str(cursor)
        .map_err(|_| JsonRpcErrorObject::invalid_request(format!("invalid cursor: {cursor}")))
}

fn response(response: impl serde::Serialize) -> TurnMethodResult {
    serde_json::to_value(response).map(Some).map_err(|error| {
        JsonRpcErrorObject::internal_error(format!("failed to serialize response: {error}"))
    })
}
