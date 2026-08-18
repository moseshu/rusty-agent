//! # `ra-prompt`
//!
//! Prompt assembly machinery. This crate contains no prompt content.
//!
//! **Boundary**: it handles sectioning, ordering, the stable prefix, invalidation tracking, the
//! cache-plan intent, the dump report, and the tail reminder channel. Content arrives as
//! [`PromptSection`](ra_core::prompt::PromptSection) values registered by a product crate, which is
//! also where the assembled prefix is turned into an agent's instructions — the loop kernel depends
//! on `ra-core` alone and cannot reach this crate.
//!
//! The separation is the most direct expression of "the framework supplies mechanism". Text that
//! only one product could use — an identity, a tone, a list of shell escapes a read-only agent must
//! not reach for — describes that product, and putting it here would make every other product
//! inherit it.
//!
//! **Stability**: `Evolving`. Section names and assembly order may grow but not shrink, and a
//! semantic change goes in the CHANGELOG — they determine the cache prefix, so touching one moves
//! hit rate and cost.

pub mod assembler;
pub mod dump;
pub mod dynamic;
pub mod metrics;
pub mod reminder;
pub mod section;
pub mod stability;

pub use assembler::{PromptAssembler, StablePrefix};
pub use dump::{PromptDump, PromptDumpSection};
pub use dynamic::{
    resolve_dynamic_prompt, resolved_to_tail_input_items, resolved_to_volatile_sections,
};
pub use metrics::{
    calculate_cache_hit_rate, calculate_cache_hit_rate_from_usage, meets_cache_hit_target,
};
pub use reminder::{ReminderAttachment, RuntimeReminder};
pub use section::{PromptSectionBuilder, compute_content_hash, estimate_tokens};
pub use stability::{InvalidationReason, PrefixStabilityTracker, assert_prefix_stable};
