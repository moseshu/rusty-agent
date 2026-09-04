//! This crate's view of the shared character basis every context estimate prices with.
//!
//! The basis itself lives in [`ra_core::item::estimate`], because a third consumer joined the two
//! here: [`compaction`](crate::compaction) reads the number as a trigger against a configured
//! limit, [`usage`](crate::usage) reads it as a diagnostic a host shows a user, and a
//! [`ContextFilterReport`](ra_core::filter::ContextFilterReport) reports a saving that only means
//! something when it is measured against the same limit. Two of those callers are in other crates,
//! so the primitive belongs with the item type rather than with any one reader of it.
//!
//! These aliases exist so this crate's call sites keep reading as their own, and so the shared
//! basis has exactly one importer here rather than one per module.

pub(crate) use ra_core::item::estimate::{chars_to_tokens, item_tokens};
