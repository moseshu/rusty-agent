//! # `ra-prompt`
//!
//! Prompt assembly machinery. This crate contains no prompt content.
//!
//! **Boundary**: it handles sectioning, ordering, the stable prefix, and the cache plan. Identity,
//! tool preferences, business discipline and similar text belong to a product crate; this crate
//! reads no configuration file and calls no provider.
//!
//! **Stability**: `Evolving`. Section names and assembly order may grow but not shrink, and a
//! semantic change goes in the CHANGELOG — they determine the cache prefix, so touching one moves
//! hit rate and cost.

pub mod assembler;
pub mod cache_plan;
pub mod dump;
pub mod reminder;
pub mod role;
pub mod section;
pub mod stability;
