//! The thread lifecycle methods and notifications: Codex's `thread/unsubscribe`, `thread/closed`
//! and `thread/backgroundTerminals/*`, and the history reads `thread/turns/list` and
//! `thread/items/list` (`protocol/v2/thread.rs`).

use serde::{Deserialize, Serialize};

use super::{ThreadItem, Turn, TurnItemsView};

/// The method of the notification that a thread was unloaded.
pub const THREAD_CLOSED_METHOD: &str = "thread/closed";

/// The parameters of `thread/unsubscribe`: Codex's `ThreadUnsubscribeParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUnsubscribeParams {
    thread_id: String,
}

impl ThreadUnsubscribeParams {
    /// Unsubscribes the calling connection from `thread_id`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

/// What `thread/unsubscribe` found: Codex's `ThreadUnsubscribeStatus`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadUnsubscribeStatus {
    /// No such thread is loaded.
    NotLoaded,
    /// The connection was not subscribed to it.
    NotSubscribed,
    /// The connection's subscription was removed.
    Unsubscribed,
}

/// The answer to `thread/unsubscribe`: Codex's `ThreadUnsubscribeResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUnsubscribeResponse {
    status: ThreadUnsubscribeStatus,
}

impl ThreadUnsubscribeResponse {
    pub(crate) const fn new(status: ThreadUnsubscribeStatus) -> Self {
        Self { status }
    }

    /// What the call found.
    #[must_use]
    pub const fn status(&self) -> ThreadUnsubscribeStatus {
        self.status
    }
}

/// That a thread was unloaded, sent to every connection: Codex's `ThreadClosedNotification`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadClosedNotification {
    thread_id: String,
}

impl ThreadClosedNotification {
    pub(crate) fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

/// The parameters of `thread/backgroundTerminals/clean`: Codex's
/// `ThreadBackgroundTerminalsCleanParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBackgroundTerminalsCleanParams {
    thread_id: String,
}

impl ThreadBackgroundTerminalsCleanParams {
    /// Stops every background terminal of `thread_id`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

/// The answer to `thread/backgroundTerminals/clean`: Codex's
/// `ThreadBackgroundTerminalsCleanResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadBackgroundTerminalsCleanResponse {}

impl ThreadBackgroundTerminalsCleanResponse {
    pub(crate) const fn new() -> Self {
        Self {}
    }
}

/// The parameters of `thread/backgroundTerminals/list`: Codex's
/// `ThreadBackgroundTerminalsListParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBackgroundTerminalsListParams {
    thread_id: String,
    /// Where a previous page ended: the `nextCursor` it returned.
    #[serde(default)]
    cursor: Option<String>,
    /// How many terminals a page holds at most.
    #[serde(default)]
    limit: Option<u32>,
}

impl ThreadBackgroundTerminalsListParams {
    /// Lists the background terminals of `thread_id`, all on one page.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            cursor: None,
            limit: None,
        }
    }

    /// Continues after the page that returned `cursor`.
    #[must_use]
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// Puts at most `limit` terminals on a page.
    #[must_use]
    pub const fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn into_parts(self) -> (String, Option<String>, Option<u32>) {
        (self.thread_id, self.cursor, self.limit)
    }
}

/// A command a thread's tools left running: Codex's `ThreadBackgroundTerminal`.
///
/// Codex fills neither the process's operating-system id nor its resource use; nor does this
/// server, and they are sent as `null`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBackgroundTerminal {
    item_id: String,
    process_id: String,
    command: String,
    cwd: String,
    os_pid: Option<u32>,
    cpu_percent: Option<f64>,
    rss_kb: Option<u64>,
}

impl ThreadBackgroundTerminal {
    pub(crate) fn new(info: super::super::BackgroundTerminalInfo) -> Self {
        Self {
            item_id: info.item_id,
            process_id: info.process_id,
            command: info.command,
            cwd: info.cwd,
            os_pid: None,
            cpu_percent: None,
            rss_kb: None,
        }
    }

    /// The tool call that started it.
    #[must_use]
    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// What `thread/backgroundTerminals/terminate` names it by.
    #[must_use]
    pub fn process_id(&self) -> &str {
        &self.process_id
    }

    /// The command line.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The directory it runs in.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }
}

/// The answer to `thread/backgroundTerminals/list`: Codex's
/// `ThreadBackgroundTerminalsListResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBackgroundTerminalsListResponse {
    data: Vec<ThreadBackgroundTerminal>,
    /// Where the next page starts, or `None` on the last page.
    next_cursor: Option<String>,
}

impl ThreadBackgroundTerminalsListResponse {
    pub(crate) const fn new(
        data: Vec<ThreadBackgroundTerminal>,
        next_cursor: Option<String>,
    ) -> Self {
        Self { data, next_cursor }
    }

    /// The terminals on this page.
    #[must_use]
    pub fn data(&self) -> &[ThreadBackgroundTerminal] {
        &self.data
    }

    /// Where the next page starts, or `None` on the last page.
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }
}

/// The parameters of `thread/backgroundTerminals/terminate`: Codex's
/// `ThreadBackgroundTerminalsTerminateParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBackgroundTerminalsTerminateParams {
    thread_id: String,
    process_id: String,
}

impl ThreadBackgroundTerminalsTerminateParams {
    /// Stops the background terminal `process_id` of `thread_id`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>, process_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            process_id: process_id.into(),
        }
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The terminal.
    #[must_use]
    pub fn process_id(&self) -> &str {
        &self.process_id
    }
}

/// The answer to `thread/backgroundTerminals/terminate`: Codex's
/// `ThreadBackgroundTerminalsTerminateResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBackgroundTerminalsTerminateResponse {
    terminated: bool,
}

impl ThreadBackgroundTerminalsTerminateResponse {
    pub(crate) const fn new(terminated: bool) -> Self {
        Self { terminated }
    }

    /// Whether a running terminal of that id was stopped.
    #[must_use]
    pub const fn terminated(&self) -> bool {
        self.terminated
    }
}

/// Which way a page runs: Codex's `SortDirection`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    /// Oldest first.
    Asc,
    /// Newest first.
    Desc,
}

/// The parameters of `thread/turns/list`: Codex's `ThreadTurnsListParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTurnsListParams {
    thread_id: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    sort_direction: Option<SortDirection>,
    #[serde(default)]
    items_view: Option<TurnItemsView>,
}

impl ThreadTurnsListParams {
    /// Lists the turns of `thread_id`: the newest page, with summary items.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            cursor: None,
            limit: None,
            sort_direction: None,
            items_view: None,
        }
    }

    /// Starts from `cursor`: a page's `nextCursor`, or its `backwardsCursor` with the opposite
    /// direction.
    #[must_use]
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// Puts at most `limit` turns on the page.
    #[must_use]
    pub const fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Runs the page in `sort_direction`; newest first by default.
    #[must_use]
    pub const fn with_sort_direction(mut self, sort_direction: SortDirection) -> Self {
        self.sort_direction = Some(sort_direction);
        self
    }

    /// Includes as much of each turn's items as `items_view` says; a summary by default.
    #[must_use]
    pub const fn with_items_view(mut self, items_view: TurnItemsView) -> Self {
        self.items_view = Some(items_view);
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn into_parts(
        self,
    ) -> (
        String,
        Option<String>,
        Option<u32>,
        Option<SortDirection>,
        Option<TurnItemsView>,
    ) {
        (
            self.thread_id,
            self.cursor,
            self.limit,
            self.sort_direction,
            self.items_view,
        )
    }
}

/// The answer to `thread/turns/list`: Codex's `ThreadTurnsListResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTurnsListResponse {
    data: Vec<Turn>,
    next_cursor: Option<String>,
    backwards_cursor: Option<String>,
}

impl ThreadTurnsListResponse {
    pub(crate) const fn new(
        data: Vec<Turn>,
        next_cursor: Option<String>,
        backwards_cursor: Option<String>,
    ) -> Self {
        Self {
            data,
            next_cursor,
            backwards_cursor,
        }
    }

    /// The turns on this page, in its direction.
    #[must_use]
    pub fn data(&self) -> &[Turn] {
        &self.data
    }

    /// Where the next page starts, or `None` on the last page.
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }

    /// Where a page in the opposite direction starts, including this page's first turn again, or
    /// `None` if this page is empty.
    #[must_use]
    pub fn backwards_cursor(&self) -> Option<&str> {
        self.backwards_cursor.as_deref()
    }
}

/// An item that an item page starts after: Codex's `ThreadItemsListAnchor`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadItemsListAnchor {
    /// The item `item_id` of the requested turn.
    #[non_exhaustive]
    Item {
        /// The item.
        #[serde(rename = "itemId")]
        item_id: String,
    },
}

impl ThreadItemsListAnchor {
    /// The item `item_id`.
    #[must_use]
    pub fn item(item_id: impl Into<String>) -> Self {
        Self::Item {
            item_id: item_id.into(),
        }
    }
}

/// Where an item page starts: Codex's `ThreadItemsListCursor`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThreadItemsListCursor {
    /// A cursor a page returned.
    Opaque(String),
    /// An item to start after.
    Anchor(ThreadItemsListAnchor),
}

/// The parameters of `thread/items/list`: Codex's `ThreadItemsListParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItemsListParams {
    thread_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    cursor: Option<ThreadItemsListCursor>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    sort_direction: Option<SortDirection>,
}

impl ThreadItemsListParams {
    /// Lists the items of `thread_id`.
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: None,
            cursor: None,
            limit: None,
            sort_direction: None,
        }
    }

    /// Lists only the items of `turn_id`.
    #[must_use]
    pub fn with_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    /// Starts from `cursor`.
    #[must_use]
    pub fn with_cursor(mut self, cursor: ThreadItemsListCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// Puts at most `limit` items on the page.
    #[must_use]
    pub const fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Runs the page in `sort_direction`; oldest first by default.
    #[must_use]
    pub const fn with_sort_direction(mut self, sort_direction: SortDirection) -> Self {
        self.sort_direction = Some(sort_direction);
        self
    }

    /// The thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The turn, if the page is of one.
    #[must_use]
    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    /// Where the page starts, if not at the beginning.
    #[must_use]
    pub const fn cursor(&self) -> Option<&ThreadItemsListCursor> {
        self.cursor.as_ref()
    }
}

/// An item of a thread and where it is: Codex's `ThreadItemEntry`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItemEntry {
    turn_id: String,
    item: ThreadItem,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
}

impl ThreadItemEntry {
    /// The turn the item belongs to.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The item.
    #[must_use]
    pub const fn item(&self) -> &ThreadItem {
        &self.item
    }

    /// When it started, in milliseconds since the Unix epoch, if recorded.
    #[must_use]
    pub const fn started_at_ms(&self) -> Option<i64> {
        self.started_at_ms
    }

    /// When it completed, in milliseconds since the Unix epoch, if recorded.
    #[must_use]
    pub const fn completed_at_ms(&self) -> Option<i64> {
        self.completed_at_ms
    }
}

/// The answer to `thread/items/list`: Codex's `ThreadItemsListResponse`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItemsListResponse {
    data: Vec<ThreadItemEntry>,
    next_cursor: Option<String>,
    backwards_cursor: Option<String>,
}

impl ThreadItemsListResponse {
    /// The items on this page, in its direction.
    #[must_use]
    pub fn data(&self) -> &[ThreadItemEntry] {
        &self.data
    }

    /// Where the next page starts, or `None` on the last page.
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }

    /// Where a page in the opposite direction starts, or `None` if this page is empty.
    #[must_use]
    pub fn backwards_cursor(&self) -> Option<&str> {
        self.backwards_cursor.as_deref()
    }
}
