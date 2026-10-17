// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Persistent thread management, ported from Codex's thread processor and thread delete handler.
//!
//! Archive and delete stop the spawned subtree before moving or removing storage. Explicit
//! metadata updates materialize a loaded persistent thread and use its store's writer queue.
//! Replies precede the archive, unarchive, delete and name broadcasts.
//!
//! Product sections, projects and Daybreak settings are not represented by this store and are
//! refused. Recency sorts use update time because there is no product recency index. Unlike Codex,
//! this provider-neutral server has no global default provider or interactive session source;
//! an omitted provider/source filter includes all providers/unknown root sources. Explicit source
//! filters match the source vocabulary this port records: unknown and spawned threads. Cursors
//! use the store's timestamp/id ordering, including its tie breaking, rather than Codex's
//! timestamp-only legacy cursors.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use ra_core::{error::Error, session::SessionId};
use ra_session::store::{
    ArchiveThreadParams, ArchiveThreadsParams, DeleteThreadsParams, GitInfoPatch,
    ListThreadsParams, ReadThreadParams, SanitizedGitUrl, SortDirection as StoreSortDirection,
    StoredThread, ThreadMetadataPatch, ThreadSortKey as StoreSortKey, ThreadStore,
    UpdateThreadMetadataParams,
};
use serde::Serialize;

use super::{
    SessionSource, SubAgentSource,
    outgoing::{
        ConnectionRequestId, OutgoingMessageSender, ServerNotification,
        ThreadScopedOutgoingMessageSender,
    },
    protocol::{
        SortDirection, THREAD_ARCHIVED_METHOD, THREAD_DELETED_METHOD, THREAD_NAME_UPDATED_METHOD,
        THREAD_READ_STATE_CHANGED_METHOD, THREAD_UNARCHIVED_METHOD, Thread, ThreadArchiveParams,
        ThreadArchiveResponse, ThreadArchivedNotification, ThreadDeleteParams,
        ThreadDeleteResponse, ThreadDeletedNotification, ThreadItem, ThreadListCwdFilter,
        ThreadListParams, ThreadListResponse, ThreadMetadataGitInfoUpdateParams,
        ThreadMetadataUpdateParams, ThreadMetadataUpdateResponse, ThreadNameUpdatedNotification,
        ThreadReadParams, ThreadReadState, ThreadReadStateChangedNotification,
        ThreadReadStateOperation, ThreadReadStateUpdateParams, ThreadReadStateUpdateResponse,
        ThreadSetNameParams, ThreadSetNameResponse, ThreadSortKey, ThreadSourceKind,
        ThreadUnarchiveParams, ThreadUnarchiveResponse, ThreadUnarchivedNotification,
        ThreadUnreadPosition,
    },
    thread_history::HistorySources,
    thread_loading::{apply_live_thread_settings, is_not_found, thread_from_stored},
    thread_processor::ThreadRequestProcessor,
    thread_status::resolve_thread_status,
    turn_processor::{TurnMethodResult, unsupported},
};
use crate::frame::JsonRpcErrorObject;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const THREAD_LIST_MAX_EXCLUDED_IDS: usize = 100;

impl ThreadRequestProcessor {
    pub(crate) async fn thread_archive(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadArchiveParams,
        history: &HistorySources,
    ) -> TurnMethodResult {
        let _state = self.thread_list_state.lock().await;
        let store = require_store(history, "thread/archive")?;
        let root = SessionId::new(params.thread_id);
        let subtree = spawn_subtree(store.as_ref(), &root).await?;
        if let Some(loaded) = self.thread_manager.get_thread(&root)
            && !loaded.info.ephemeral
        {
            loaded
                .ensure_rollout_materialized()
                .await
                .map_err(|error| mutation_error("archive", &root, &error))?;
        }
        store
            .read_thread(&ReadThreadParams::new(root.clone()))
            .await
            .map_err(|error| mutation_error("archive", &root, &error))?;
        let mut archive_order = vec![root.clone()];
        for child in subtree.iter().skip(1).rev() {
            match store
                .read_thread(&ReadThreadParams::new(child.clone()).including_archived())
                .await
            {
                Ok(thread) if thread.archived_at().is_none() => archive_order.push(child.clone()),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%child, %error, "failed to read a spawned descendant before archiving");
                }
            }
        }
        for id in &subtree {
            self.prepare_thread_for_removal(id).await;
        }
        let archived = store
            .archive_threads(&ArchiveThreadsParams::new(archive_order))
            .await
            .map_err(|error| mutation_error("archive", &root, &error))?;
        self.outgoing
            .send_serialized_response(request_id, ThreadArchiveResponse::default())
            .await;
        for id in archived {
            self.send_management_notification(
                THREAD_ARCHIVED_METHOD,
                ThreadArchivedNotification {
                    thread_id: id.as_str().to_owned(),
                },
            )
            .await;
        }
        Ok(None)
    }

    pub(crate) async fn thread_delete(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadDeleteParams,
        history: &HistorySources,
    ) -> TurnMethodResult {
        let _state = self.thread_list_state.lock().await;
        let store = require_store(history, "thread/delete")?;
        let root = SessionId::new(params.thread_id);
        let subtree = spawn_subtree(store.as_ref(), &root).await?;
        if let Some(loaded) = self.thread_manager.get_thread(&root) {
            if loaded.info.ephemeral {
                return Err(JsonRpcErrorObject::invalid_request(format!(
                    "thread is not persisted and cannot be deleted: {root}"
                )));
            }
        } else if let Err(error) = store
            .read_thread(&ReadThreadParams::new(root.clone()).including_archived())
            .await
            && !(is_not_found(&error) && subtree.len() > 1)
        {
            return Err(mutation_error("delete", &root, &error));
        }
        for id in &subtree {
            self.prepare_thread_for_removal(id).await;
        }
        let mut delete_order: Vec<_> = subtree.into_iter().skip(1).rev().collect();
        delete_order.push(root.clone());
        store
            .delete_threads(&DeleteThreadsParams::new(delete_order.clone()))
            .await
            .map_err(|error| mutation_error("delete", &root, &error))?;
        self.outgoing
            .send_serialized_response(request_id, ThreadDeleteResponse::default())
            .await;
        for id in delete_order {
            self.send_management_notification(
                THREAD_DELETED_METHOD,
                ThreadDeletedNotification {
                    thread_id: id.as_str().to_owned(),
                },
            )
            .await;
        }
        Ok(None)
    }

    pub(crate) async fn thread_unarchive(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadUnarchiveParams,
        history: &HistorySources,
    ) -> TurnMethodResult {
        let _state = self.thread_list_state.lock().await;
        let store = require_store(history, "thread/unarchive")?;
        let id = SessionId::new(params.thread_id);
        let stored = store
            .unarchive_thread(&ArchiveThreadParams::new(id.clone()))
            .await
            .map_err(|error| mutation_error("unarchive", &id, &error))?;
        let thread = self.describe_stored_thread(&stored);
        self.outgoing
            .send_serialized_response(request_id, ThreadUnarchiveResponse { thread })
            .await;
        self.send_management_notification(
            THREAD_UNARCHIVED_METHOD,
            ThreadUnarchivedNotification {
                thread_id: id.as_str().to_owned(),
            },
        )
        .await;
        Ok(None)
    }

    pub(crate) async fn thread_set_name(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadSetNameParams,
        history: &HistorySources,
    ) -> TurnMethodResult {
        let name = params.name.trim().to_owned();
        if name.is_empty() {
            return Err(JsonRpcErrorObject::invalid_request(
                "thread name must not be empty",
            ));
        }
        let _state = self.thread_list_state.lock().await;
        let id = SessionId::new(params.thread_id);
        self.update_stored_metadata(
            history,
            &id,
            ThreadMetadataPatch::new().with_name(Some(name.clone())),
            false,
        )
        .await?;
        self.outgoing
            .send_serialized_response(request_id, ThreadSetNameResponse::default())
            .await;
        self.send_management_notification(
            THREAD_NAME_UPDATED_METHOD,
            ThreadNameUpdatedNotification {
                thread_id: id.as_str().to_owned(),
                thread_name: Some(name),
            },
        )
        .await;
        Ok(None)
    }

    pub(crate) async fn thread_metadata_update(
        &self,
        params: ThreadMetadataUpdateParams,
        history: &HistorySources,
    ) -> TurnMethodResult {
        if params.project_id.is_some() {
            return Err(unsupported("thread/metadata/update", "projectId"));
        }
        if params.daybreak_enabled.is_some() {
            return Err(unsupported("thread/metadata/update", "daybreakEnabled"));
        }
        let git = params.git_info.ok_or_else(|| {
            JsonRpcErrorObject::invalid_request(
                "thread metadata update must include at least one field",
            )
        })?;
        let patch = normalize_git_patch(git)?;
        let _state = self.thread_list_state.lock().await;
        let id = SessionId::new(params.thread_id);
        let stored = self
            .update_stored_metadata(
                history,
                &id,
                ThreadMetadataPatch::new().with_git_info(patch),
                true,
            )
            .await?;
        super::thread_processor::response(ThreadMetadataUpdateResponse {
            thread: self.describe_stored_thread(&stored),
        })
    }

    async fn update_stored_metadata(
        &self,
        history: &HistorySources,
        id: &SessionId,
        patch: ThreadMetadataPatch,
        include_archived: bool,
    ) -> Result<StoredThread, JsonRpcErrorObject> {
        let store = require_store(history, "update thread metadata")?;
        if let Some(loaded) = self.thread_manager.get_thread(id) {
            if loaded.info.ephemeral {
                return Err(JsonRpcErrorObject::invalid_request(format!(
                    "ephemeral thread does not support metadata updates: {id}"
                )));
            }
            loaded
                .ensure_rollout_materialized()
                .await
                .map_err(|error| mutation_error("update thread metadata", id, &error))?;
        }
        let mut update = UpdateThreadMetadataParams::new(id.clone(), patch);
        let mut read = ReadThreadParams::new(id.clone());
        if include_archived {
            update = update.including_archived();
            read = read.including_archived();
        }
        match store
            .update_thread_metadata(&update)
            .await
            .map_err(|error| mutation_error("update thread metadata", id, &error))?
        {
            Some(thread) => Ok(thread),
            None => store
                .read_thread(&read)
                .await
                .map_err(|error| mutation_error("read thread metadata", id, &error)),
        }
    }

    /// Stops the runtime before closing its writer. A bounded shutdown failure is logged, as in
    /// Codex; storage still enforces writer ownership and reports a conflict rather than moving
    /// records while a writer is in flight.
    async fn prepare_thread_for_removal(&self, id: &SessionId) {
        self.pending_thread_unloads.lock().await.insert(id.clone());
        if let Some(loaded) = self.thread_manager.get_thread(id) {
            self.thread_manager.remove_thread_if_matches(&loaded);
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
                loaded.shutdown().await;
                if let Err(error) = loaded.close_rollout().await {
                    tracing::warn!(%id, %error, "failed to close a thread's rollout before removal");
                }
            }).await.is_err() { tracing::warn!(%id, "thread shutdown timed out; proceeding with removal"); }
        }
        self.finalize_thread_teardown(id).await;
    }

    fn describe_stored_thread(&self, stored: &StoredThread) -> Thread {
        let mut thread = thread_from_stored(stored);
        let running = if let Some(loaded) = self.thread_manager.get_thread(stored.session_id()) {
            apply_live_thread_settings(&mut thread, &loaded);
            loaded.is_running()
        } else {
            false
        };
        thread.status = resolve_thread_status(
            self.thread_watch
                .loaded_status_for_thread(stored.session_id()),
            running,
        );
        thread
    }

    async fn send_management_notification(
        &self,
        method: &'static str,
        notification: impl Serialize,
    ) {
        match serde_json::to_value(notification) {
            Ok(params) => {
                self.outgoing
                    .send_server_notification(ServerNotification::new(method, params))
                    .await;
            }
            Err(error) => {
                tracing::error!(%method, %error, "failed to serialize a thread notification");
            }
        }
    }

    #[allow(clippy::too_many_lines)] // Follows Codex's list validation and filtered pagination.
    pub(crate) async fn thread_list(
        &self,
        params: ThreadListParams,
        history: &HistorySources,
    ) -> TurnMethodResult {
        let excluded_thread_ids = params.excluded_thread_ids.unwrap_or_default();
        if excluded_thread_ids.len() > THREAD_LIST_MAX_EXCLUDED_IDS {
            return Err(JsonRpcErrorObject::invalid_params(format!(
                "excludedThreadIds accepts at most {THREAD_LIST_MAX_EXCLUDED_IDS} entries"
            )));
        }
        // Framework session IDs are opaque strings, including host-supplied IDs and `sess-`
        // IDs. Preserve that contract instead of imposing Codex's UUID-only parser here.
        let excluded_thread_ids: HashSet<_> = excluded_thread_ids
            .into_iter()
            .map(SessionId::new)
            .collect();
        if params
            .originators
            .as_ref()
            .is_some_and(|origins| !origins.is_empty())
        {
            return Err(JsonRpcErrorObject::invalid_params(
                "originator filtering is not supported by the local app-server",
            ));
        }
        if params.section_id.is_some() {
            return Err(unsupported("thread/list", "sectionId"));
        }
        if params.project_id.is_some() {
            return Err(unsupported("thread/list", "projectId"));
        }
        if params.parent_thread_id.is_some() && params.ancestor_thread_id.is_some() {
            return Err(JsonRpcErrorObject::invalid_request(
                "parentThreadId and ancestorThreadId are mutually exclusive",
            ));
        }
        let store = require_store(history, "thread/list")?;
        let ancestors = if let Some(id) = &params.ancestor_thread_id {
            Some(
                spawn_subtree(store.as_ref(), &SessionId::new(id))
                    .await?
                    .into_iter()
                    .skip(1)
                    .collect::<HashSet<_>>(),
            )
        } else {
            None
        };
        let sort_key = match params.sort_key.unwrap_or(ThreadSortKey::CreatedAt) {
            ThreadSortKey::CreatedAt => StoreSortKey::CreatedAt,
            ThreadSortKey::UpdatedAt | ThreadSortKey::RecencyAt => StoreSortKey::UpdatedAt,
            ThreadSortKey::SectionPosition => {
                return Err(unsupported("thread/list", "sortKey.section_position"));
            }
        };
        let direction = match params.sort_direction.unwrap_or(SortDirection::Desc) {
            SortDirection::Asc => StoreSortDirection::Asc,
            SortDirection::Desc => StoreSortDirection::Desc,
        };
        let limit = params
            .limit
            .map_or(25, |limit| limit as usize)
            .clamp(1, 100);
        let mut query = ListThreadsParams::new(limit)
            .with_sort_key(sort_key)
            .with_sort_direction(direction);
        if let Some(cursor) = params.cursor {
            // Stores use millisecond/id cursors. Validate client input before storage errors are
            // projected; unrelated I/O failures must retain their internal-error classification.
            if cursor
                .split_once('|')
                .is_none_or(|(at, id)| at.parse::<u64>().is_err() || id.is_empty())
            {
                return Err(JsonRpcErrorObject::invalid_request(
                    "invalid thread/list cursor",
                ));
            }
            query = query.with_cursor(cursor);
        }
        if params.archived.unwrap_or(false) {
            query = query.archived();
        }
        if params.use_state_db_only {
            query = query.with_state_db_only();
        }
        if let Some(providers) = params.model_providers {
            query = query.with_model_providers(providers);
        }
        if let Some(term) = params.search_term {
            query = query.with_search_term(term);
        }
        if let Some(cwd) = params.cwd {
            let dirs = match cwd {
                ThreadListCwdFilter::One(dir) => vec![dir],
                ThreadListCwdFilter::Many(dirs) => dirs,
            };
            let current = std::env::current_dir().map_err(|error| {
                JsonRpcErrorObject::invalid_params(format!(
                    "invalid thread/list cwd filter: {error}"
                ))
            })?;
            query = query.with_cwd_filters(
                dirs.into_iter()
                    .map(|dir| current.join(dir).to_string_lossy().into_owned())
                    .collect(),
            );
        }
        let mut items = Vec::new();
        let mut seen_cursors: HashSet<_> = query.cursor().map(str::to_owned).into_iter().collect();
        let next_cursor = loop {
            query = query.with_page_size(limit - items.len());
            let page = store.list_threads(&query).await.map_err(|error| {
                JsonRpcErrorObject::internal_error(format!("failed to list threads: {error}"))
            })?;
            for thread in page.items() {
                if excluded_thread_ids.contains(thread.session_id()) {
                    continue;
                }
                if params.parent_thread_id.as_ref().is_some_and(|id| {
                    thread.parent_session_id().map(SessionId::as_str) != Some(id.as_str())
                }) {
                    continue;
                }
                if ancestors
                    .as_ref()
                    .is_some_and(|ids| !ids.contains(thread.session_id()))
                {
                    continue;
                }
                if params
                    .source_kinds
                    .as_ref()
                    .filter(|kinds| !kinds.is_empty())
                    .is_some_and(|kinds| !source_kind_matches(thread, kinds))
                {
                    continue;
                }
                items.push(thread.clone());
                if items.len() == limit {
                    break;
                }
            }
            let next = page.next_cursor().map(str::to_owned);
            if let Some(cursor) = &next
                && !seen_cursors.insert(cursor.clone())
            {
                return Err(JsonRpcErrorObject::internal_error(
                    "thread listing returned a repeated cursor",
                ));
            }
            if items.len() == limit {
                break next;
            }
            let Some(next) = next else {
                break None;
            };
            query = query.with_cursor(next);
        };
        let backwards_cursor = items.first().and_then(|thread| {
            let at = match sort_key {
                StoreSortKey::UpdatedAt => thread.updated_at(),
                _ => thread.created_at(),
            }?;
            // The store includes id in every timestamp tie; reverse traversal starts at the
            // first row's actual key, preserving other rows sharing its timestamp.
            Some(format!("{}|{}", at.as_millis(), thread.session_id()))
        });
        let data: Vec<_> = items
            .iter()
            .map(|thread| self.describe_stored_thread(thread))
            .collect();
        let read_states = read_state_snapshots(history, &data).await;
        super::thread_processor::response(ThreadListResponse {
            data,
            read_states,
            next_cursor,
            backwards_cursor,
        })
    }
}

fn require_store(
    history: &HistorySources,
    method: &str,
) -> Result<Arc<dyn ThreadStore>, JsonRpcErrorObject> {
    history.store().ok_or_else(|| {
        JsonRpcErrorObject::method_not_found(format!("{method} is not supported yet"))
    })
}

fn mutation_error(operation: &str, id: &SessionId, error: &Error) -> JsonRpcErrorObject {
    if is_not_found(error) {
        JsonRpcErrorObject::invalid_request(format!("thread not found: {id}"))
    } else if matches!(error, Error::Caller { .. }) || is_writer_conflict(error) {
        JsonRpcErrorObject::invalid_request(error.to_string())
    } else {
        JsonRpcErrorObject::internal_error(format!("failed to {operation} thread: {error}"))
    }
}

fn is_writer_conflict(error: &Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
        {
            return true;
        }
        source = error.source();
    }
    false
}

async fn spawn_subtree(
    store: &dyn ThreadStore,
    root: &SessionId,
) -> Result<Vec<SessionId>, JsonRpcErrorObject> {
    let mut threads = vec![root.clone()];
    let mut seen = HashSet::from([root.clone()]);
    let mut index = 0;
    while index < threads.len() {
        let children = store
            .list_spawn_children(&threads[index])
            .await
            .map_err(|error| {
                JsonRpcErrorObject::internal_error(format!(
                    "failed to list spawned descendants for thread id {root}: {error}"
                ))
            })?;
        threads.extend(children.into_iter().filter(|id| seen.insert(id.clone())));
        index += 1;
    }
    Ok(threads)
}

fn normalize_git_patch(
    git: ThreadMetadataGitInfoUpdateParams,
) -> Result<GitInfoPatch, JsonRpcErrorObject> {
    if git.sha.is_none() && git.branch.is_none() && git.origin_url.is_none() {
        return Err(JsonRpcErrorObject::invalid_request(
            "gitInfo must include at least one field",
        ));
    }
    let origin = normalize_git_field(git.origin_url, "gitInfo.originUrl")?;
    let origin_url = match origin {
        Some(Some(url)) => Some(Some(SanitizedGitUrl::try_from(url).map_err(|_| {
            JsonRpcErrorObject::invalid_request("gitInfo.originUrl must be a valid Git remote")
        })?)),
        Some(None) => Some(None),
        None => None,
    };
    let mut patch = GitInfoPatch::default();
    patch.sha = normalize_git_field(git.sha, "gitInfo.sha")?;
    patch.branch = normalize_git_field(git.branch, "gitInfo.branch")?;
    patch.origin_url = origin_url;
    Ok(patch)
}

#[allow(
    clippy::option_option,
    reason = "Codex's keep, clear and replace semantics"
)]
fn normalize_git_field(
    value: Option<Option<String>>,
    name: &str,
) -> Result<Option<Option<String>>, JsonRpcErrorObject> {
    match value {
        Some(Some(value)) => {
            let value = value.trim().to_owned();
            if value.is_empty() {
                return Err(JsonRpcErrorObject::invalid_request(format!(
                    "{name} must not be empty"
                )));
            }
            Ok(Some(Some(value)))
        }
        other => Ok(other),
    }
}

/// Codex's explicit source-kind filter, with spawned lineage overriding raw root metadata.
fn source_kind_matches(thread: &StoredThread, filter: &[ThreadSourceKind]) -> bool {
    if thread.thread_spawn().is_some() {
        return filter.iter().any(|kind| {
            matches!(
                kind,
                ThreadSourceKind::SubAgent | ThreadSourceKind::SubAgentThreadSpawn
            )
        });
    }
    let source = SessionSource::from_metadata(thread.source());
    filter.iter().any(|kind| match kind {
        ThreadSourceKind::Cli => matches!(source, SessionSource::Cli),
        ThreadSourceKind::VsCode => matches!(source, SessionSource::VSCode),
        ThreadSourceKind::Exec => matches!(source, SessionSource::Exec),
        ThreadSourceKind::AppServer => matches!(source, SessionSource::Mcp),
        ThreadSourceKind::SubAgent => matches!(source, SessionSource::SubAgent(_)),
        ThreadSourceKind::SubAgentReview => {
            matches!(source, SessionSource::SubAgent(SubAgentSource::Review))
        }
        ThreadSourceKind::SubAgentCompact => {
            matches!(source, SessionSource::SubAgent(SubAgentSource::Compact))
        }
        ThreadSourceKind::SubAgentThreadSpawn => matches!(
            source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        ),
        ThreadSourceKind::SubAgentOther => {
            matches!(source, SessionSource::SubAgent(SubAgentSource::Other(_)))
        }
        ThreadSourceKind::Unknown => matches!(source, SessionSource::Unknown),
    })
}

/// Codex excludes ephemeral, subagent, unknown and product-policy scopes from local receipts.
fn read_state_eligible(thread: &Thread) -> bool {
    !thread.ephemeral
        && source_has_local_read_state(&thread.source)
        && matches!(thread.thread_source.as_deref(), None | Some("user"))
}

fn source_has_local_read_state(source: &serde_json::Value) -> bool {
    matches!(
        source.as_str(),
        Some("cli" | "vscode" | "exec" | "appServer")
    ) || source
        .get("custom")
        .is_some_and(serde_json::Value::is_string)
}

fn read_state_to_api(state: &ra_session::store::local::ThreadReadState) -> ThreadReadState {
    ThreadReadState {
        first_unread: state.first_unread_turn().map(|turn_id| {
            if turn_id.is_empty() {
                ThreadUnreadPosition::ThreadStart
            } else {
                ThreadUnreadPosition::Turn {
                    turn_id: turn_id.to_owned(),
                }
            }
        }),
        revision: state.revision().to_owned(),
    }
}

/// Reads only eligible local threads in the requested page. No DB or a failed read means null.
pub(super) async fn read_state_snapshots(
    history: &HistorySources,
    threads: &[Thread],
) -> Option<HashMap<String, ThreadReadState>> {
    let db = history.store()?.state_db()?;
    let ids = threads
        .iter()
        .filter(|thread| read_state_eligible(thread))
        .map(|thread| SessionId::new(&thread.id))
        .collect::<Vec<_>>();
    match db.thread_read_states(&ids).await {
        Ok(states) => Some(
            states
                .into_iter()
                .map(|(id, state)| (id.as_str().to_owned(), read_state_to_api(&state)))
                .collect(),
        ),
        Err(error) => {
            tracing::warn!(%error, "thread read state unavailable");
            None
        }
    }
}

async fn notify_read_state(
    db: &ra_session::store::local::StateRuntime,
    outgoing: &Arc<OutgoingMessageSender>,
    id: &SessionId,
) {
    match db.thread_read_states(std::slice::from_ref(id)).await {
        Ok(mut states) => {
            if let Some(state) = states.remove(id) {
                let subscribers = outgoing.thread_state().subscribed_connection_ids(id);
                let params = ThreadReadStateChangedNotification {
                    thread_id: id.as_str().to_owned(),
                    read_state: read_state_to_api(&state),
                };
                ThreadScopedOutgoingMessageSender::new(
                    Arc::clone(outgoing),
                    subscribers,
                    id.clone(),
                )
                .send_server_notification(
                    ServerNotification::new(
                        THREAD_READ_STATE_CHANGED_METHOD,
                        serde_json::json!(params),
                    )
                    .experimental(THREAD_READ_STATE_CHANGED_METHOD),
                )
                .await;
            }
        }
        Err(error) => tracing::warn!(%error, "could not project committed read state"),
    }
}

/// Runs only on newly delivered terminal events, before their completion notification.
pub(super) async fn publish_read_state(
    event: &ra_runtime::thread::ThreadEvent,
    loaded: &super::thread_manager::LoadedThread,
    summary: &super::thread_state::TurnSummary,
    history: &HistorySources,
    outgoing: &Arc<OutgoingMessageSender>,
) {
    use ra_runtime::thread::ThreadEvent;
    let Some(db) = history.store().and_then(|store| store.state_db()) else {
        return;
    };
    if loaded.info.ephemeral
        || !loaded
            .info
            .source
            .as_ref()
            .is_some_and(source_has_local_read_state)
        || !matches!(loaded.info.thread_source.as_deref(), None | Some("user"))
    {
        return;
    }
    let has_error = summary.last_error.is_some();
    let final_text = match &summary.last_agent_message {
        Some(ThreadItem::AgentMessage { text, .. }) => Some(text.as_str()),
        _ => None,
    };
    let (turn_id, visible) = match event {
        ThreadEvent::TurnComplete { turn_id, .. } if has_error => (turn_id, true),
        ThreadEvent::TurnComplete { turn_id, .. } => {
            let Some(text) = final_text else {
                return;
            };
            // Goals and automatic goal continuation are not supported by this runtime.
            (
                turn_id,
                !text.contains("<decision>DONT_NOTIFY</decision>") && !text.trim().is_empty(),
            )
        }
        // This runtime's abort event only represents interruption; failures arrive as Error.
        ThreadEvent::TurnAborted { turn_id, .. } => (turn_id, has_error || final_text.is_some()),
        _ => return,
    };
    if !visible {
        return;
    }
    if let Err(error) = loaded.flush_rollout().await {
        tracing::warn!(%error, "cannot publish attention before the terminal result is durable");
        return;
    }
    match db
        .publish_thread_attention(loaded.thread_id(), turn_id)
        .await
    {
        Ok(true) => notify_read_state(&db, outgoing, loaded.thread_id()).await,
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "failed to publish terminal attention"),
    }
}

impl ThreadRequestProcessor {
    /// Applies Codex's revision-checked local read-state operation.
    pub(crate) async fn thread_read_state_update(
        &self,
        params: ThreadReadStateUpdateParams,
        history: &HistorySources,
    ) -> TurnMethodResult {
        use ra_session::store::local::{ReadStateOperation, ReadStateUpdate};
        let id = SessionId::new(&params.thread_id);
        let db = history
            .store()
            .and_then(|store| store.state_db())
            .ok_or_else(|| {
                JsonRpcErrorObject::method_not_found("durable thread read state unavailable")
            })?;
        let thread = self
            .read_thread_view(ThreadReadParams::new(params.thread_id), history)
            .await?;
        if !read_state_eligible(&thread) {
            return Err(JsonRpcErrorObject::invalid_request(
                "thread read state unavailable",
            ));
        }
        let operation = match params.operation {
            ThreadReadStateOperation::Read => ReadStateOperation::Read,
            ThreadReadStateOperation::Unread => ReadStateOperation::Unread,
        };
        let outcome = db
            .update_thread_read_state(&id, &params.expected_revision, operation)
            .await
            .map_err(|error| {
                JsonRpcErrorObject::internal_error(format!("read state was not saved: {error}"))
            })?;
        match outcome {
            ReadStateUpdate::Applied(state) => {
                notify_read_state(&db, &self.outgoing, &id).await;
                super::thread_processor::response(ThreadReadStateUpdateResponse {
                    read_state: read_state_to_api(&state),
                })
            }
            ReadStateUpdate::Conflict(state) => {
                let data = serde_json::json!({
                    "reason": "readStateConflict", "readState": read_state_to_api(&state),
                });
                Err(JsonRpcErrorObject::invalid_request("read state changed").with_data(data))
            }
            ReadStateUpdate::Unavailable => Err(JsonRpcErrorObject::invalid_request(
                "thread read state unavailable",
            )),
            _ => Err(JsonRpcErrorObject::internal_error(
                "unsupported read state update outcome",
            )),
        }
    }
}
