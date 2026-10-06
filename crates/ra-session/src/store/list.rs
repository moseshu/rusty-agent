//! What [`ThreadStore::list_threads`](super::ThreadStore::list_threads) takes and returns:
//! Codex's `ListThreadsParams`, `ThreadSortKey`, `SortDirection` and `ThreadPage`
//! (`thread-store/src/types.rs`).

use super::StoredThread;

/// What threads are sorted by: Codex's `ThreadSortKey`.
///
/// Codex's `RecencyAt` and `SectionPosition` come from its `SQLite` state database, which is not
/// ported; without it Codex's own recency falls back to the update time.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ThreadSortKey {
    /// When the thread was created.
    #[default]
    CreatedAt,
    /// When the thread was last written to.
    UpdatedAt,
}

/// Which way threads are sorted: Codex's `SortDirection`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SortDirection {
    /// Older threads first.
    Asc,
    /// Newer threads first.
    #[default]
    Desc,
}

/// Which threads to list, in what order, from where: Codex's `ListThreadsParams`.
///
/// Codex's source, section, project and spawn-relation filters are not ported: this framework has
/// no session-source vocabulary, and the others are answered by Codex's state database. Its
/// `use_state_db_only` has nothing to select here.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListThreadsParams {
    page_size: usize,
    cursor: Option<String>,
    sort_key: ThreadSortKey,
    sort_direction: SortDirection,
    model_providers: Option<Vec<String>>,
    cwd_filters: Option<Vec<String>>,
    archived: bool,
    search_term: Option<String>,
}

impl ListThreadsParams {
    /// Lists up to `page_size` active threads, newest created first.
    #[must_use]
    pub const fn new(page_size: usize) -> Self {
        Self {
            page_size,
            cursor: None,
            sort_key: ThreadSortKey::CreatedAt,
            sort_direction: SortDirection::Desc,
            model_providers: None,
            cwd_filters: None,
            archived: false,
            search_term: None,
        }
    }

    /// Continues after the page that returned `cursor` as its [`ThreadPage::next_cursor`].
    #[must_use]
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// Sorts by `sort_key`.
    #[must_use]
    pub const fn with_sort_key(mut self, sort_key: ThreadSortKey) -> Self {
        self.sort_key = sort_key;
        self
    }

    /// Sorts in `sort_direction`.
    #[must_use]
    pub const fn with_sort_direction(mut self, sort_direction: SortDirection) -> Self {
        self.sort_direction = sort_direction;
        self
    }

    /// Lists only threads recorded with one of `providers`; an empty list matches every provider,
    /// as Codex's does.
    #[must_use]
    pub fn with_model_providers(mut self, providers: Vec<String>) -> Self {
        self.model_providers = Some(providers);
        self
    }

    /// Lists only threads whose recorded working directory is one of `cwds`; an empty list matches
    /// no thread, as Codex's does.
    #[must_use]
    pub fn with_cwd_filters(mut self, cwds: Vec<String>) -> Self {
        self.cwd_filters = Some(cwds);
        self
    }

    /// Lists archived threads instead of active ones.
    #[must_use]
    pub const fn archived(mut self) -> Self {
        self.archived = true;
        self
    }

    /// Lists only threads whose name contains `term`, as Codex's file-backed listing matches the
    /// names in its session index.
    #[must_use]
    pub fn with_search_term(mut self, term: impl Into<String>) -> Self {
        self.search_term = Some(term.into());
        self
    }

    /// The most threads a page holds.
    #[must_use]
    pub const fn page_size(&self) -> usize {
        self.page_size
    }

    /// Where the page starts, if not at the beginning.
    #[must_use]
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// What threads are sorted by.
    #[must_use]
    pub const fn sort_key(&self) -> ThreadSortKey {
        self.sort_key
    }

    /// Which way threads are sorted.
    #[must_use]
    pub const fn sort_direction(&self) -> SortDirection {
        self.sort_direction
    }

    /// The model providers threads must have been recorded with, if filtered.
    #[must_use]
    pub fn model_providers(&self) -> Option<&[String]> {
        self.model_providers.as_deref()
    }

    /// The working directories threads must have been recorded in, if filtered.
    #[must_use]
    pub fn cwd_filters(&self) -> Option<&[String]> {
        self.cwd_filters.as_deref()
    }

    /// Whether archived threads are listed instead of active ones.
    #[must_use]
    pub const fn is_archived(&self) -> bool {
        self.archived
    }

    /// What thread names must contain, if searched.
    #[must_use]
    pub fn search_term(&self) -> Option<&str> {
        self.search_term.as_deref()
    }
}

/// A page of threads: Codex's `ThreadPage`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadPage {
    items: Vec<StoredThread>,
    next_cursor: Option<String>,
}

impl ThreadPage {
    /// The page holding `items`, continued from `next_cursor` if more may follow.
    #[must_use]
    pub const fn new(items: Vec<StoredThread>, next_cursor: Option<String>) -> Self {
        Self { items, next_cursor }
    }

    /// The threads on the page.
    #[must_use]
    pub fn items(&self) -> &[StoredThread] {
        &self.items
    }

    /// The threads on the page, taken.
    #[must_use]
    pub fn into_items(self) -> Vec<StoredThread> {
        self.items
    }

    /// The cursor the next page starts from, or `None` when nothing follows.
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }
}
