//! Keeping the thread table in step with the rollouts: Codex's `apply_metadata_update`
//! (`thread-store/src/local/update_thread_metadata.rs`), `extract_metadata_from_rollout`
//! (`rollout/src/metadata.rs`) and `reconcile_rollout` and `read_repair_rollout_path`
//! (`rollout/src/state_db.rs`).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use ra_core::{error::Result, event::EventTimestamp, session::SessionId};
use rusqlite::TransactionBehavior;

use super::{
    StateRuntime,
    runtime::allocate_thread_updated_at,
    threads::{ThreadMetadata, get_thread, normalize_cwd, upsert_thread},
};
use crate::{
    rollout::RolloutReader,
    store::{
        ThreadMetadataPatch, first_session_meta,
        metadata_sync::{Observed, ThreadMetadataSync, readable_payloads},
    },
};

/// Applies what `patch` sets to `metadata`, as Codex's `apply_metadata_update` does field by
/// field. The originator only fills a missing one; a name is not a field of the row and is set
/// apart, through the title.
pub(crate) fn apply_patch(metadata: &mut ThreadMetadata, patch: &ThreadMetadataPatch) {
    if let Some(preview) = patch.preview() {
        preview.clone_into(&mut metadata.preview);
    }
    if let Some(title) = patch.title() {
        title.clone_into(&mut metadata.title);
    }
    if let Some(provider) = patch.model_provider() {
        metadata.model_provider = Some(provider.to_owned());
    }
    if let Some(model) = patch.model() {
        metadata.model = Some(model.to_owned());
    }
    if let Some(effort) = patch.effort() {
        metadata.effort = effort.map(str::to_owned);
    }
    if let Some(created_at) = patch.created_at() {
        metadata.created_at = created_at;
    }
    if let Some(updated_at) = patch.updated_at() {
        metadata.updated_at = updated_at;
    }
    if metadata.originator.is_none() {
        metadata.originator = patch.originator().map(str::to_owned);
    }
    if let Some(spawn) = patch.thread_spawn() {
        metadata.thread_spawn = Some(spawn.clone());
    }
    if let Some(source) = patch.forked_from_id() {
        metadata.forked_from_id = Some(source.clone());
    }
    if let Some(cwd) = patch.cwd() {
        metadata.cwd = Some(normalize_cwd(cwd));
    }
    if let Some(version) = patch.cli_version() {
        metadata.cli_version = Some(version.to_owned());
    }
    if let Some(message) = patch.first_user_message() {
        metadata.first_user_message = Some(message.to_owned());
    }
}

/// Writes `patch` into the row of `session_id`, kept at `rollout_path`, archived when `archived`:
/// Codex's `apply_metadata_update`.
///
/// A thread without a row gets one built from the patch, as Codex's
/// `metadata_for_missing_sqlite_row` builds it: created when the patch says, or when it was last
/// updated, or now. A name replaces the title, as Codex sets it in its legacy history mode.
/// The read, merge and write share an immediate transaction: Codex serializes these steps with
/// its per-thread pending metadata lock; a transaction also covers independently opened runtimes.
pub(crate) async fn apply_metadata_update(
    db: &StateRuntime,
    session_id: &SessionId,
    patch: &ThreadMetadataPatch,
    rollout_path: PathBuf,
    archived: bool,
) -> Result<()> {
    let session_id = session_id.clone();
    let patch = patch.clone();
    let latest_updated_at = Arc::clone(&db.thread_updated_at_millis);
    db.run(move |connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = get_thread(&transaction, session_id.as_str())?;
        let mut metadata = existing.unwrap_or_else(|| {
            let created_at = patch
                .created_at()
                .or_else(|| patch.updated_at())
                .unwrap_or_else(EventTimestamp::now);
            let mut metadata = ThreadMetadata::new(session_id, rollout_path.clone(), created_at);
            if archived {
                metadata.archived_at = Some(metadata.updated_at);
            }
            metadata
        });
        metadata.rollout_path = rollout_path;
        apply_patch(&mut metadata, &patch);
        if let Some(name) = patch.name() {
            metadata.title = name.unwrap_or_default().to_owned();
        }
        metadata.updated_at = allocate_thread_updated_at(&latest_updated_at, metadata.updated_at);
        upsert_thread(&transaction, &metadata)?;
        transaction.commit()
    })
    .await
}

/// What the rollout at `path` says of its thread: Codex's `extract_metadata_from_rollout`.
///
/// The whole rollout is read and derived as a live thread's metadata sync derives it from a
/// resumed history; the update time is the file's modification time. A rollout that does not
/// open with session metadata describes no thread, as a listing does not list it, and gives
/// `None`.
pub(crate) async fn extract_metadata_from_rollout(path: &Path) -> Result<Option<ThreadMetadata>> {
    let records = RolloutReader::open(path).read_all().await?;
    let Some(meta) = first_session_meta(&records)? else {
        return Ok(None);
    };
    let created_at = meta
        .created_at()
        .or_else(|| records.first().map(crate::rollout::RolloutRecord::at))
        .unwrap_or_else(EventTimestamp::now);
    let mut metadata =
        ThreadMetadata::new(meta.session_id().clone(), path.to_path_buf(), created_at);
    let payloads = readable_payloads(&records);
    let observed = payloads
        .iter()
        .map(|(payload, at)| Observed::of_payload(payload, *at))
        .collect::<Vec<_>>();
    if let Some(update) =
        ThreadMetadataSync::for_resume(meta.session_id().clone(), &observed).take_pending_update()
    {
        apply_patch(&mut metadata, &update.patch);
    }
    if let Some(modified) = crate::lite::modified_time(path).await {
        metadata.updated_at = modified;
    }
    Ok(Some(metadata))
}

/// Whether the row of a thread names a rollout other than `path` that is still there, which a
/// repair from `path` must then leave alone.
///
/// Codex never moves a row to another rollout during a repair, since after a revert several
/// rollouts of one thread can be found and only the row knows which is current. A thread here
/// has one rollout, active or archived, so a row whose rollout is gone — moved by a handle of the
/// directory without the database — is repaired to the one found.
async fn names_another_rollout(existing: &ThreadMetadata, path: &Path) -> bool {
    existing.rollout_path != path
        && tokio::fs::metadata(&existing.rollout_path)
            .await
            .is_ok_and(|metadata| metadata.is_file())
}

/// Sets `metadata` archived or active as `archived` says, if it says: Codex's handling of
/// `archived_only` in a repair.
fn settle_archived(metadata: &mut ThreadMetadata, archived: Option<bool>) {
    match archived {
        Some(true) if metadata.archived_at.is_none() => {
            metadata.archived_at = Some(metadata.updated_at);
        }
        Some(false) => metadata.archived_at = None,
        _ => {}
    }
}

/// Rebuilds the row of the thread whose rollout is `path` from the rollout: Codex's
/// `reconcile_rollout` without items. A name the row was given is kept. Failures are logged, as
/// Codex's are: the database is an index and a listing goes on without it.
pub(crate) async fn reconcile_rollout(db: &StateRuntime, path: &Path, archived: Option<bool>) {
    let mut metadata = match extract_metadata_from_rollout(path).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the state database could not read a rollout to reconcile");
            return;
        }
    };
    let existing = db.get_thread(&metadata.session_id).await.ok().flatten();
    if let Some(existing) = existing.as_ref() {
        if names_another_rollout(existing, path).await {
            return;
        }
        metadata.prefer_existing_explicit_title(existing);
    }
    settle_archived(&mut metadata, archived);
    if let Err(error) = db.upsert_thread(&metadata).await {
        tracing::warn!(path = %path.display(), %error, "the state database could not reconcile a rollout");
    }
}

/// Points the row of `session_id` at `path`, where a listing found its rollout, and sets it
/// archived or active as it was found: Codex's `read_repair_rollout_path`. A row already right is
/// not written; a missing or unreadable one is rebuilt from the rollout.
pub(crate) async fn read_repair_rollout_path(
    db: &StateRuntime,
    session_id: &SessionId,
    archived: Option<bool>,
    path: &Path,
) {
    if let Ok(Some(existing)) = db.get_thread(session_id).await {
        if names_another_rollout(&existing, path).await {
            return;
        }
        let mut repaired = existing.clone();
        path.clone_into(&mut repaired.rollout_path);
        settle_archived(&mut repaired, archived);
        if repaired == existing {
            return;
        }
        match db.upsert_thread(&repaired).await {
            Ok(()) => return,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "the state database could not repair a thread's rollout path");
            }
        }
    }
    reconcile_rollout(db, path, archived).await;
}
