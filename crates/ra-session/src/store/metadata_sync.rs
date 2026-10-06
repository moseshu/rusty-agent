//! Deriving a live thread's metadata from the records appended to it: Codex's
//! `ThreadMetadataSync` (`thread-store/src/thread_metadata_sync.rs`).
//!
//! Stores receive records and explicit metadata patches; deciding what a record says about the
//! thread — its first user message, preview and title, the latest model and effort, its working
//! directory and when it was last updated — belongs here, above the store. The sync keeps one
//! pending patch, merging every observation into it, and hands out snapshots of it tagged with a
//! generation: a snapshot that was applied clears the pending patch only if nothing was merged in
//! since it was taken, so a failed or overtaken write is retried with the next one.
//! Background commit signals are wakeups, not permission to submit any later pending patch. An
//! eligible observation requests a commit; applying its latest snapshot clears that request, and
//! leftover signals cannot submit a subsequent throttled touch. Barriers may submit pending
//! metadata regardless of that request, as Codex's live thread does.
//!
//! An append whose records say nothing about the thread's metadata only touches its update time,
//! and such touches are written at most once per [`THREAD_UPDATED_AT_TOUCH_INTERVAL`]; the ones in
//! between stay pending for the next write or barrier.
//!
//! # Which records count
//!
//! Codex reads its `SessionMeta`, `TurnContext`, user message, token count, goal and thread
//! settings events. This framework's counterparts are the session metadata, which is read only
//! from a resumed history since it is not a record a run appends; the turn context, for the model,
//! effort and working directory; and the start of a run, whose new input holds the user's
//! messages. A run start counts only when that input holds a user message and is not a
//! continuation base, as Codex counts only its user message events. Token usage, goals, thread
//! settings, recency, Git facts and the memory mode have no field in this framework's patch and are
//! not derived.
//!
//! A user message gives the preview and the first user message from its text, or a placeholder
//! for one without text, as a listing reads them; the title comes only from text.

use std::time::{Duration, Instant};

use ra_core::{
    event::EventTimestamp,
    item::ModelInputItem,
    session::{
        SessionId,
        rollout::{RolloutItem, RolloutRunStarted, RolloutTurnContext},
    },
};

use super::ThreadMetadataPatch;
use crate::{
    lite::user_message_preview,
    rollout::{RolloutPayload, RolloutRecord, RolloutSessionMeta, truncation::is_user_message},
};

/// How often an append that changes nothing but the update time writes it: Codex's
/// `THREAD_UPDATED_AT_TOUCH_INTERVAL`.
const THREAD_UPDATED_AT_TOUCH_INTERVAL: Duration = Duration::from_secs(5);

/// What a live thread has derived and not yet written: Codex's `ThreadMetadataSync`.
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Derived-field flags, lifecycle deferrals and background submission eligibility"
)]
pub(crate) struct ThreadMetadataSync {
    session_id: SessionId,
    cwd_seen: bool,
    preview_seen: bool,
    first_user_message_seen: bool,
    title_seen: bool,
    pending_update: Option<ThreadMetadataPatch>,
    pending_update_generation: u64,
    commit_requested: bool,
    last_touch_persisted_at: Option<Instant>,
    defer_create_update_until_history_exists: bool,
    defer_resume_update_until_append: bool,
}

/// A snapshot of the pending patch, to be marked applied once written: Codex's
/// `PendingThreadMetadataPatch`.
#[derive(Debug, Clone)]
pub(crate) struct PendingThreadMetadataPatch {
    pub(crate) patch: ThreadMetadataPatch,
    generation: u64,
}

/// A record as the sync reads it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Observed<'a> {
    /// Session metadata, with the time its record was written.
    SessionMeta(&'a RolloutSessionMeta, EventTimestamp),
    TurnContext(&'a RolloutTurnContext),
    RunStarted(&'a RolloutRunStarted),
    /// A record that says nothing about the thread's metadata.
    Other,
}

impl<'a> Observed<'a> {
    /// What the sync reads of a record a run appends.
    pub(crate) const fn of_item(item: &'a RolloutItem) -> Self {
        match item {
            RolloutItem::TurnContext(context) => Self::TurnContext(context),
            RolloutItem::RunStarted(started) => Self::RunStarted(started),
            _ => Self::Other,
        }
    }

    /// What the sync reads of a stored payload written at `at`.
    pub(crate) const fn of_payload(payload: &'a RolloutPayload, at: EventTimestamp) -> Self {
        match payload {
            RolloutPayload::SessionMeta(meta) => Self::SessionMeta(meta, at),
            RolloutPayload::TurnContext(context) => Self::TurnContext(context),
            RolloutPayload::RunStarted(started) => Self::RunStarted(started),
            _ => Self::Other,
        }
    }

    /// Whether the record can change derived metadata: Codex's
    /// `rollout_item_affects_thread_metadata`.
    fn affects_metadata(self) -> bool {
        match self {
            Self::SessionMeta(..) | Self::TurnContext(_) => true,
            Self::RunStarted(started) => {
                !started.input_is_continuation_base() && started.input().iter().any(is_user_message)
            }
            Self::Other => false,
        }
    }
}

/// The payloads of `records` with the time each was written; a record whose payload cannot be
/// read is left out, as it says nothing the sync can use.
pub(crate) fn readable_payloads(
    records: &[RolloutRecord],
) -> Vec<(RolloutPayload, EventTimestamp)> {
    records
        .iter()
        .filter_map(|record| record.payload().ok().map(|payload| (payload, record.at())))
        .collect()
}

/// Sets what `set` sets on `update`, through the patch's builder.
fn set(
    update: &mut ThreadMetadataPatch,
    set: impl FnOnce(ThreadMetadataPatch) -> ThreadMetadataPatch,
) {
    *update = set(std::mem::take(update));
}

impl ThreadMetadataSync {
    /// The sync of a thread being created with `meta`: Codex's `for_create`. The thread's initial
    /// metadata stays pending until something is appended or the thread is persisted.
    pub(crate) fn for_create(meta: &RolloutSessionMeta) -> Self {
        let created_at = meta.created_at().unwrap_or_else(EventTimestamp::now);
        let mut update = ThreadMetadataPatch::new()
            .with_created_at(created_at)
            .with_updated_at(created_at);
        if let Some(provider) = meta.model_provider() {
            set(&mut update, |patch| patch.with_model_provider(provider));
        }
        if let Some(originator) = meta
            .originator()
            .filter(|originator| !originator.is_empty())
        {
            set(&mut update, |patch| patch.with_originator(originator));
        }
        if let Some(cwd) = meta.cwd() {
            set(&mut update, |patch| patch.with_cwd(cwd));
        }
        if let Some(version) = meta.cli_version() {
            set(&mut update, |patch| patch.with_cli_version(version));
        }
        update = with_lineage(update, meta);
        Self {
            session_id: meta.session_id().clone(),
            cwd_seen: meta.cwd().is_some_and(|cwd| !cwd.is_empty()),
            preview_seen: false,
            first_user_message_seen: false,
            title_seen: false,
            pending_update: Some(update),
            pending_update_generation: 1,
            commit_requested: false,
            last_touch_persisted_at: None,
            defer_create_update_until_history_exists: true,
            defer_resume_update_until_append: false,
        }
    }

    /// The sync of a thread being resumed with `history`: Codex's `for_resume` without state
    /// database metadata, as Codex resumes a legacy thread. What the history says stays pending
    /// until something is appended, so a resume alone writes nothing.
    pub(crate) fn for_resume(session_id: SessionId, history: &[Observed<'_>]) -> Self {
        let mut sync = Self {
            session_id,
            cwd_seen: false,
            preview_seen: false,
            first_user_message_seen: false,
            title_seen: false,
            pending_update: None,
            pending_update_generation: 0,
            commit_requested: false,
            last_touch_persisted_at: None,
            defer_create_update_until_history_exists: false,
            defer_resume_update_until_append: false,
        };
        let update = sync.observe_items_with_update(history, ThreadMetadataPatch::new());
        sync.merge_pending_update(update);
        sync.defer_resume_update_until_append = sync.pending_update.is_some();
        sync
    }

    /// A snapshot of the pending patch, which stays pending until marked applied: Codex's
    /// `take_pending_update`.
    pub(crate) fn take_pending_update(&self) -> Option<PendingThreadMetadataPatch> {
        self.pending_update
            .clone()
            .map(|patch| PendingThreadMetadataPatch {
                patch,
                generation: self.pending_update_generation,
            })
    }

    /// Takes a snapshot while an eligible observation's submission request remains outstanding.
    /// Extra queued signals cannot submit touches that a later observation throttled. Failed
    /// writes retain the request until a later write or barrier applies the latest snapshot.
    pub(crate) fn take_pending_update_for_commit(&self) -> Option<PendingThreadMetadataPatch> {
        if !self.commit_requested {
            return None;
        }
        self.take_pending_update()
    }

    /// As [`Self::take_pending_update`], except while a created thread has no history yet or a
    /// resumed one has had nothing appended: Codex's `take_pending_update_for_existing_history`.
    pub(crate) fn take_pending_update_for_existing_history(
        &self,
    ) -> Option<PendingThreadMetadataPatch> {
        if self.defer_create_update_until_history_exists || self.defer_resume_update_until_append {
            return None;
        }
        self.take_pending_update()
    }

    /// Clears the pending patch if `update` is still its latest snapshot, and notes when the update
    /// time was last written: Codex's `mark_pending_update_applied`.
    pub(crate) fn mark_pending_update_applied(&mut self, update: &PendingThreadMetadataPatch) {
        if self.pending_update_generation == update.generation {
            self.pending_update = None;
            self.commit_requested = false;
        }
        if update.patch.updated_at().is_some() {
            self.last_touch_persisted_at = Some(Instant::now());
        }
    }

    /// Forgets the pending patch, for a thread whose writer is discarded.
    pub(crate) fn discard_pending_update(&mut self) {
        self.commit_requested = false;
        if self.pending_update.take().is_some() {
            self.pending_update_generation = self.pending_update_generation.wrapping_add(1);
        }
    }

    /// Merges what `items`, just appended, say into the pending patch and returns a snapshot to
    /// write now, or `None` when the append only touches the update time within
    /// [`THREAD_UPDATED_AT_TOUCH_INTERVAL`] of the last one written: Codex's
    /// `observe_appended_items`. An empty append is not an append and changes nothing.
    pub(crate) fn observe_appended_items(
        &mut self,
        items: &[Observed<'_>],
    ) -> Option<PendingThreadMetadataPatch> {
        if items.is_empty() {
            return None;
        }
        self.defer_create_update_until_history_exists = false;
        self.defer_resume_update_until_append = false;
        let affects_metadata = items.iter().any(|item| item.affects_metadata());
        let update = if affects_metadata {
            self.observe_items_with_update(
                items,
                ThreadMetadataPatch::new().with_updated_at(EventTimestamp::now()),
            )?
        } else {
            ThreadMetadataPatch::new().with_updated_at(EventTimestamp::now())
        };
        self.merge_pending_update(Some(update));
        if !affects_metadata
            && !self
                .pending_update
                .as_ref()
                .is_some_and(update_has_metadata_facts)
            && self
                .last_touch_persisted_at
                .is_some_and(|last_touch| last_touch.elapsed() < THREAD_UPDATED_AT_TOUCH_INTERVAL)
        {
            return None;
        }
        self.commit_requested = true;
        self.take_pending_update()
    }

    fn observe_items_with_update(
        &mut self,
        items: &[Observed<'_>],
        mut update: ThreadMetadataPatch,
    ) -> Option<ThreadMetadataPatch> {
        if items.is_empty() {
            return None;
        }
        for item in items {
            match *item {
                Observed::SessionMeta(meta, at) if *meta.session_id() == self.session_id => {
                    self.observe_session_meta(meta, at, &mut update);
                }
                Observed::TurnContext(context) => {
                    if !self.cwd_seen
                        && let Some(cwd) = context.cwd()
                    {
                        self.cwd_seen = true;
                        set(&mut update, |patch| patch.with_cwd(cwd));
                    }
                    if let Some(model) = context.model() {
                        set(&mut update, |patch| patch.with_model(model));
                    }
                    set(&mut update, |patch| {
                        patch.with_effort(context.effort().map(str::to_owned))
                    });
                }
                Observed::RunStarted(started) if !started.input_is_continuation_base() => {
                    for input in started
                        .input()
                        .iter()
                        .filter(|input| is_user_message(input))
                    {
                        self.observe_user_message(input, &mut update);
                    }
                }
                Observed::SessionMeta(..) | Observed::RunStarted(_) | Observed::Other => {}
            }
        }
        Some(update)
    }

    fn observe_session_meta(
        &mut self,
        meta: &RolloutSessionMeta,
        at: EventTimestamp,
        update: &mut ThreadMetadataPatch,
    ) {
        let created_at = meta.created_at().unwrap_or(at);
        set(update, |patch| patch.with_created_at(created_at));
        if let Some(originator) = meta
            .originator()
            .filter(|originator| !originator.is_empty())
        {
            set(update, |patch| patch.with_originator(originator));
        }
        if let Some(provider) = meta
            .model_provider()
            .filter(|provider| !provider.is_empty())
        {
            set(update, |patch| patch.with_model_provider(provider));
        }
        if let Some(version) = meta.cli_version().filter(|version| !version.is_empty()) {
            set(update, |patch| patch.with_cli_version(version));
        }
        if let Some(cwd) = meta.cwd().filter(|cwd| !cwd.is_empty()) {
            self.cwd_seen = true;
            set(update, |patch| patch.with_cwd(cwd));
        }
        *update = with_lineage(std::mem::take(update), meta);
    }

    /// Codex's `observe_user_message`: the first user message and preview come from the first
    /// message that has either text or an attachment, the title from the first with text.
    fn observe_user_message(&mut self, item: &ModelInputItem, update: &mut ThreadMetadataPatch) {
        if (!self.first_user_message_seen || !self.preview_seen)
            && let Some(preview) = user_message_preview(item)
        {
            if !self.first_user_message_seen {
                self.first_user_message_seen = true;
                set(update, |patch| {
                    patch.with_first_user_message(preview.clone())
                });
            }
            if !self.preview_seen {
                self.preview_seen = true;
                set(update, |patch| patch.with_preview(preview));
            }
        }
        if !self.title_seen
            && let ModelInputItem::Message(message) = item
        {
            let text = message.text_content();
            let title = text.trim();
            if !title.is_empty() {
                self.title_seen = true;
                set(update, |patch| patch.with_title(title));
            }
        }
    }

    fn merge_pending_update(&mut self, update: Option<ThreadMetadataPatch>) {
        let Some(update) = update else {
            return;
        };
        match self.pending_update.as_mut() {
            Some(pending_update) => pending_update.merge(update),
            None => self.pending_update = Some(update),
        }
        self.pending_update_generation = self.pending_update_generation.wrapping_add(1);
    }
}

/// `update` with where the thread of `meta` was spawned from and forked from, as Codex's patch
/// carries the session source.
fn with_lineage(mut update: ThreadMetadataPatch, meta: &RolloutSessionMeta) -> ThreadMetadataPatch {
    if let Some(spawn) = meta.thread_spawn() {
        update = update.with_thread_spawn(spawn.clone());
    }
    if let Some(source) = meta.forked_from_id() {
        update = update.with_forked_from_id(source.clone());
    }
    update
}

/// Whether `update` sets anything but the update time: Codex's `update_has_metadata_facts`.
fn update_has_metadata_facts(update: &ThreadMetadataPatch) -> bool {
    update.preview().is_some()
        || update.title().is_some()
        || update.model_provider().is_some()
        || update.model().is_some()
        || update.effort().is_some()
        || update.created_at().is_some()
        || update.originator().is_some()
        || update.cwd().is_some()
        || update.cli_version().is_some()
        || update.first_user_message().is_some()
        || update.thread_spawn().is_some()
        || update.forked_from_id().is_some()
}
