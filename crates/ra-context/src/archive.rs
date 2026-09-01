//! Addressable archive of the authoritative history represented by a compaction.
//!
//! This module deliberately stores no second copy of history. A [`Session`] remains authoritative;
//! the archive reference identifies the compaction record inside it, whose covered item IDs select
//! the complete `RunItem` values on demand. A host may expose this read through an explicit tool or
//! another bounded projection, but this module never adds archived items to model input itself.
//!
//! Nothing here tells a model that an archive exists. That is
//! [`Compaction::with_archive_ref`](ra_core::item::Compaction::with_archive_ref)'s `notice`, which
//! the host writes because only the host knows whether it wired up a retrieval path at all.

use std::collections::BTreeSet;

use ra_core::{
    error::{Error, Result, SessionErrorKind},
    item::{ArchiveRef, RunItem, RunItemKind},
    session::Session,
};

/// Complete authoritative records represented by one archived compaction.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchivedHistory {
    archive_ref: ArchiveRef,
    items: Vec<RunItem>,
}

impl ArchivedHistory {
    /// Reference used to retrieve this history.
    #[must_use]
    pub const fn archive_ref(&self) -> &ArchiveRef {
        &self.archive_ref
    }

    /// Complete records in their original chronological order.
    #[must_use]
    pub fn items(&self) -> &[RunItem] {
        &self.items
    }
}

/// Reads the complete authoritative history represented by an archive reference.
///
/// The record is selected by the reference it carries, **not** by the session named inside the
/// reference. That session is provenance: a sub-agent transcript grafted into its root, a forked
/// conversation, an imported log all keep the address they were written with, and gating on it
/// would report history that is sitting right there as unavailable. Reading is confined to the
/// session passed in, which is the only history this function can reach at all, so nothing is
/// widened by taking the address at face value. When this build can parse the address, its
/// compaction item ID must also match the stored record; an unrecognized future address still
/// selects by its exact text so it can survive a downgrade read.
///
/// A missing compaction record, or one that no longer carries this reference, returns `None`, so a
/// caller can treat a stale address as unavailable. A record that claims to cover missing or
/// duplicate items is session corruption rather than an incomplete archive, and is reported as
/// such.
pub async fn read_archived_history(
    session: &(impl Session + ?Sized),
    archive_ref: &ArchiveRef,
) -> Result<Option<ArchivedHistory>> {
    let history = session.get_items(None).await?;
    let Some((compaction_index, compacted_item_ids)) =
        history
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item.kind() {
                RunItemKind::Compaction(compaction)
                    if compaction.archive_ref() == Some(archive_ref)
                        && archive_ref
                            .compaction_item_id()
                            .is_none_or(|owner| item.id() == owner) =>
                {
                    Some((index, compaction.compacted_items()))
                }
                _ => None,
            })
    else {
        return Ok(None);
    };

    let requested: BTreeSet<_> = compacted_item_ids.iter().collect();
    if requested.len() != compacted_item_ids.len() {
        return Err(Error::session(
            SessionErrorKind::Corrupted,
            format!("archive `{archive_ref}` names the same archived item more than once"),
        ));
    }

    // One pass: the records the archive covers are collected as they are recognized, and the set of
    // IDs seen so far is what catches a session that stores the same ID twice.
    let mut items: Vec<RunItem> = Vec::with_capacity(requested.len());
    let mut found = BTreeSet::new();
    for item in &history[..compaction_index] {
        if !requested.contains(item.id()) {
            continue;
        }
        if !found.insert(item.id()) {
            return Err(Error::session(
                SessionErrorKind::Corrupted,
                format!(
                    "archive `{archive_ref}` has duplicate authoritative item ID `{}`",
                    item.id()
                ),
            ));
        }
        items.push(item.clone());
    }
    if let Some(missing) = requested.difference(&found).next() {
        return Err(Error::session(
            SessionErrorKind::Corrupted,
            format!("archive `{archive_ref}` names missing or later item `{missing}`"),
        ));
    }

    Ok(Some(ArchivedHistory {
        archive_ref: archive_ref.clone(),
        items,
    }))
}
