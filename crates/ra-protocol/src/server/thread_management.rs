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

use std::{collections::HashSet, sync::Arc, time::Duration};

use ra_core::{error::Error, session::SessionId};
use ra_session::store::{
    ArchiveThreadParams, ArchiveThreadsParams, DeleteThreadsParams, GitInfoPatch,
    ListThreadsParams, ReadThreadParams, SanitizedGitUrl, SortDirection as StoreSortDirection,
    StoredThread, ThreadMetadataPatch, ThreadSortKey as StoreSortKey, ThreadStore,
    UpdateThreadMetadataParams,
};
use serde::Serialize;

use super::{
    outgoing::{ConnectionRequestId, ServerNotification},
    protocol::{
        SortDirection, THREAD_ARCHIVED_METHOD, THREAD_DELETED_METHOD, THREAD_NAME_UPDATED_METHOD,
        THREAD_UNARCHIVED_METHOD, Thread, ThreadArchiveParams, ThreadArchiveResponse,
        ThreadArchivedNotification, ThreadDeleteParams, ThreadDeleteResponse,
        ThreadDeletedNotification, ThreadListCwdFilter, ThreadListParams, ThreadListResponse,
        ThreadMetadataGitInfoUpdateParams, ThreadMetadataUpdateParams,
        ThreadMetadataUpdateResponse, ThreadNameUpdatedNotification, ThreadSetNameParams,
        ThreadSetNameResponse, ThreadSortKey, ThreadSourceKind, ThreadUnarchiveParams,
        ThreadUnarchiveResponse, ThreadUnarchivedNotification,
    },
    thread_history::HistorySources,
    thread_loading::{apply_live_thread_settings, is_not_found, thread_from_stored},
    thread_processor::ThreadRequestProcessor,
    thread_status::resolve_thread_status,
    turn_processor::{TurnMethodResult, unsupported},
};
use crate::frame::JsonRpcErrorObject;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

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
        let next_cursor = loop {
            query = query.with_page_size(limit - items.len());
            let page = store.list_threads(&query).await.map_err(|error| {
                JsonRpcErrorObject::internal_error(format!("failed to list threads: {error}"))
            })?;
            for thread in page.items() {
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
                    .is_some_and(|kinds| {
                        !kinds.iter().any(|kind| match kind {
                            ThreadSourceKind::Unknown => thread.thread_spawn().is_none(),
                            ThreadSourceKind::SubAgent | ThreadSourceKind::SubAgentThreadSpawn => {
                                thread.thread_spawn().is_some()
                            }
                            _ => false,
                        })
                    })
                {
                    continue;
                }
                items.push(thread.clone());
                if items.len() == limit {
                    break;
                }
            }
            let next = page.next_cursor().map(str::to_owned);
            if items.len() == limit {
                break next;
            }
            let Some(next) = next else {
                break None;
            };
            if Some(next.as_str()) == query.cursor() {
                break None;
            }
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
        let data = items
            .iter()
            .map(|thread| self.describe_stored_thread(thread))
            .collect();
        super::thread_processor::response(ThreadListResponse {
            data,
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
