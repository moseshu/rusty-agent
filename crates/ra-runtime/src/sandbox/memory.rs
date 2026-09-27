//! Sandbox memory generation: the files, prompts and rules it runs on.
//!
//! A port of the reference's `sandbox/memory/` package. During a sandbox session, each run appends
//! a segment to its rollout's JSONL file under the layout's sessions directory. When the session
//! closes, phase one extracts every rollout into a raw memory and a rollout summary, and phase two
//! consolidates the most recent raw memories into the memories directory the memory capability
//! reads from.
//!
//! This module holds what that pipeline is made of: [`storage`] for the memory files, [`rollouts`]
//! for the rollout files, [`phase_one`] for the extraction's input and output rules, and
//! [`prompts`] for the reference's prompts. The configuration is
//! [`ra_core::sandbox::MemoryGenerateConfig`]; the read side is the memory capability in
//! `ra-tools`.
//!
//! # Deviations from the reference
//!
//! - **Written JSON is typed.** The reference takes a rollout segment as JSON text and
//!   re-serializes it; [`rollouts::write_rollout`] takes the segment as a value and serializes it
//!   once, in the reference's separators and ASCII escaping, with its fields in their own order.
//! - **Failures are the framework's.** The reference raises `ValueError` for a bad slug, id, path
//!   or record; these are configuration errors here, with the reference's wording. A session's
//!   failure travels as the sandbox error it is.

mod json;
pub mod phase_one;
pub mod prompts;
pub mod rollouts;
pub mod storage;
