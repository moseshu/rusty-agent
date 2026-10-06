//! Listing threads with the state database: Codex's `list_threads_db` (`rollout/src/state_db.rs`),
//! `list_threads_with_db_fallback` and `fill_missing_thread_item_metadata_from_state_db`
//! (`rollout/src/recorder.rs`) and the name resolution of its local store
//! (`thread-store/src/local/helpers.rs`).

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use ra_core::{error::Result, session::SessionId};

use super::{
    StateRuntime,
    repair::{read_repair_rollout_path, reconcile_rollout},
    threads::{ThreadFilterOptions, ThreadMetadata},
};
use crate::{
    lite,
    rollout::session_index,
    store::{ListThreadsParams, SortDirection, StoredThread, ThreadPage, ThreadSortKey},
};

/// Lists the threads of the rollouts in `dir`, archived when `params` asks, with names from the
/// session index in `index_dir`.
///
/// With [`ListThreadsParams::with_state_db_only`] only the database is read. Otherwise, as Codex's
/// `list_threads_with_db_fallback` does, the rollouts are listed first — newest-first listings ask
/// for twice the page, as Codex's do — and the row of every thread found is repaired from what
/// was found. A filtered listing then returns that page, with what only the database knows filled
/// in. A search returns the database's page after rebuilding the rows it matched, except when
/// its first page is empty: then the rollout page keeps matches from the session index.
/// Unfiltered listings return the database's page. If the database cannot be read, the page of
/// rollouts is returned.
pub(crate) async fn list_threads(
    db: &StateRuntime,
    dir: &Path,
    index_dir: &Path,
    params: &ListThreadsParams,
) -> Result<ThreadPage> {
    let archived = params.is_archived();
    if params.use_state_db_only() {
        return list_threads_db(db, index_dir, params).await;
    }
    let scan = match params.sort_direction() {
        SortDirection::Asc => params.clone(),
        _ => params
            .clone()
            .with_page_size(params.page_size().saturating_mul(2)),
    };
    let scanned = lite::list_threads(dir, index_dir, archived, &scan).await?;
    let search = params.search_term().is_some();
    for thread in scanned.items() {
        let Some(path) = thread.rollout_path() else {
            continue;
        };
        if search {
            reconcile_rollout(db, path, Some(archived)).await;
        } else {
            read_repair_rollout_path(db, thread.session_id(), Some(archived), path).await;
        }
    }

    let filtered = params.model_providers().is_some() || params.cwd_filters().is_some();
    if filtered && !search {
        let page = truncate(
            scanned,
            params.page_size(),
            params.sort_direction(),
            params.sort_key(),
        );
        return Ok(fill_from_db(db, page).await);
    }
    match list_threads_db(db, index_dir, params).await {
        Ok(page) if search && (!page.items().is_empty() || params.cursor().is_some()) => {
            for thread in page.items() {
                if let Some(path) = thread.rollout_path() {
                    reconcile_rollout(db, path, Some(archived)).await;
                }
            }
            Ok(list_threads_db(db, index_dir, params).await.unwrap_or(page))
        }
        Ok(_) if search => {
            let page = truncate(
                scanned,
                params.page_size(),
                params.sort_direction(),
                params.sort_key(),
            );
            Ok(fill_from_db(db, page).await)
        }
        Ok(page) => Ok(page),
        Err(error) => {
            tracing::warn!(%error, "the state database could not be listed; listing the rollouts instead");
            Ok(truncate(
                scanned,
                params.page_size(),
                params.sort_direction(),
                params.sort_key(),
            ))
        }
    }
}

/// A page of the database's rows: Codex's `list_threads_db`. Rows whose rollout is no longer
/// there are left out, as Codex leaves out a row with a stale path, and the page is filled from
/// the rows after them.
pub(crate) async fn list_threads_db(
    db: &StateRuntime,
    index_dir: &Path,
    params: &ListThreadsParams,
) -> Result<ThreadPage> {
    let cursor = params.cursor().map(lite::parse_cursor).transpose()?;
    let mut anchor = cursor;
    let mut rows = Vec::new();
    let mut next_anchor = None;
    while rows.len() < params.page_size() {
        let filters = ThreadFilterOptions {
            archived_only: params.is_archived(),
            model_providers: params.model_providers(),
            cwd_filters: params.cwd_filters(),
            anchor: anchor.as_ref().map(|(at, id)| (*at, id.as_str())),
            sort_key: params.sort_key(),
            sort_direction: params.sort_direction(),
            search_term: params.search_term(),
        };
        let page = db
            .list_threads(params.page_size() - rows.len(), filters)
            .await?;
        next_anchor = page.next_anchor;
        for row in page.items {
            if is_file(&row.rollout_path).await {
                rows.push(row);
            } else {
                tracing::warn!(
                    session_id = %row.session_id,
                    path = %row.rollout_path.display(),
                    "the state database lists a thread whose rollout is gone; it is left out"
                );
            }
        }
        match &next_anchor {
            Some((at, id)) => anchor = Some((*at, id.as_str().to_owned())),
            None => break,
        }
    }
    let names = resolve_names(index_dir, &rows).await;
    let items = rows
        .iter()
        .map(|row| {
            let mut thread = stored_thread(row);
            if let Some(name) = names.get(&row.session_id) {
                thread.set_name(name.clone());
            }
            thread
        })
        .collect();
    let next_cursor = next_anchor.map(|(at, id)| format!("{at}|{id}"));
    Ok(ThreadPage::new(items, next_cursor))
}

/// The names of `rows`: those in the session index, replaced by a name a row's title holds, as
/// Codex's `resolve_thread_names` reads them for its legacy history mode.
async fn resolve_names(index_dir: &Path, rows: &[ThreadMetadata]) -> HashMap<SessionId, String> {
    let ids = rows
        .iter()
        .map(|row| row.session_id.clone())
        .collect::<HashSet<_>>();
    let mut names = session_index::find_thread_names(index_dir, &ids)
        .await
        .unwrap_or_default();
    for row in rows {
        if let Some(title) = row.distinct_title() {
            names.insert(row.session_id.clone(), title.to_owned());
        }
    }
    names
}

/// The thread a row describes, as a listing returns it.
pub(crate) fn stored_thread(row: &ThreadMetadata) -> StoredThread {
    let mut thread = StoredThread::new(row.session_id.clone(), None);
    thread.rollout_path = Some(row.rollout_path.clone());
    thread.thread_spawn.clone_from(&row.thread_spawn);
    thread.forked_from_id.clone_from(&row.forked_from_id);
    thread.created_at = Some(row.created_at);
    thread.cwd.clone_from(&row.cwd);
    thread.model_provider.clone_from(&row.model_provider);
    thread.originator.clone_from(&row.originator);
    thread.cli_version.clone_from(&row.cli_version);
    thread.preview.clone_from(&row.preview);
    thread
        .first_user_message
        .clone_from(&row.first_user_message);
    thread.updated_at = Some(row.updated_at);
    thread.archived_at = row.archived_at;
    thread.model.clone_from(&row.model);
    thread.effort.clone_from(&row.effort);
    thread
}

/// Fills in what only the database knows of the threads of a page of rollouts: Codex's
/// `fill_missing_thread_item_metadata_from_state_db`. The model and effort come from the row; the
/// rest only where the rollout left it unknown; a name a row's title holds replaces the index's.
async fn fill_from_db(db: &StateRuntime, page: ThreadPage) -> ThreadPage {
    let next_cursor = page.next_cursor().map(str::to_owned);
    let mut items = page.into_items();
    for thread in &mut items {
        let Ok(Some(row)) = db.get_thread(thread.session_id()).await else {
            continue;
        };
        apply_overlay(thread, &row);
        if thread.originator.is_none() {
            thread.originator.clone_from(&row.originator);
        }
        if thread.cwd.is_none() {
            thread.cwd.clone_from(&row.cwd);
        }
        if thread.model_provider.is_none() {
            thread.model_provider.clone_from(&row.model_provider);
        }
        if thread.cli_version.is_none() {
            thread.cli_version.clone_from(&row.cli_version);
        }
    }
    ThreadPage::new(items, next_cursor)
}

/// Overlays what the database knows of `thread` on what its rollout says, as Codex's
/// `read_thread` does for a thread of its legacy history mode: the model and effort, and a name
/// the row's title holds.
pub(crate) async fn overlay(db: &StateRuntime, thread: &mut StoredThread) {
    let row = match db.get_thread(thread.session_id()).await {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(session_id = %thread.session_id(), %error, "the state database could not be read for a thread");
            return;
        }
    };
    apply_overlay(thread, &row);
}

fn apply_overlay(thread: &mut StoredThread, row: &ThreadMetadata) {
    thread.model.clone_from(&row.model);
    thread.effort.clone_from(&row.effort);
    if let Some(name) = row.distinct_title() {
        thread.set_name(name.to_owned());
    }
}

/// A page of rollouts cut to `page_size`, continuing after its last thread: Codex's
/// `page_from_filesystem_scan`, which cuts only newest-first pages, the only ones it overfetches.
fn truncate(
    page: ThreadPage,
    page_size: usize,
    direction: SortDirection,
    sort_key: ThreadSortKey,
) -> ThreadPage {
    if direction == SortDirection::Asc || page.items().len() <= page_size {
        return page;
    }
    let mut items = page.into_items();
    items.truncate(page_size);
    let next_cursor = items.last().and_then(|thread| {
        let at = match sort_key {
            ThreadSortKey::UpdatedAt => thread.updated_at(),
            _ => thread.created_at(),
        }?;
        Some(format!("{}|{}", at.as_millis(), thread.session_id()))
    });
    ThreadPage::new(items, next_cursor)
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}
