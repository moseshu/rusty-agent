//! Sandbox memory generation: recording runs while a session is open and turning them into memory.
//!
//! A port of the reference's `sandbox/memory/` package. During a sandbox session, each run appends
//! a segment to its rollout's JSONL file under the layout's sessions directory. When the session
//! closes, phase one extracts every rollout into a raw memory and a rollout summary, and phase two
//! consolidates the most recent raw memories into the memories directory the memory capability
//! reads from.
//!
//! [`manager`] runs that pipeline: one manager per memory layout per session, found or created
//! when a run of a generating agent ends, with its flush registered as the session's pre-stop
//! callback. [`phase_one`] and [`phase_two`] run the two model phases as sandbox agents borrowing
//! the session, configured by [`MemoryRunConfig`]; [`storage`] reads and writes the memory files,
//! [`rollouts`] the rollout files and the segment a run becomes, and [`prompts`] holds the
//! reference's prompts. The configuration is [`ra_core::sandbox::MemoryGenerateConfig`], handed
//! over by the memory capability in `ra-tools` through
//! [`Capability::sandbox_memory`](ra_core::capability::Capability::sandbox_memory).
//!
//! # Deviations from the reference
//!
//! - **Written JSON is typed.** The reference takes a rollout segment as JSON text and
//!   re-serializes it; [`rollouts::write_rollout`] takes the segment as a value and serializes it
//!   once, in the reference's separators and ASCII escaping, with its fields in their own order.
//! - **Failures are the framework's.** The reference raises `ValueError` for a bad slug, id, path
//!   or record, and `UserError` for conflicting managers; these are configuration errors here,
//!   with the reference's wording. A session's failure travels as the sandbox error it is.
//! - **A run is classified by how it ended, not by an exception.** The reference's runs end either
//!   with a result or by raising; a run here can also stop softly with a result on its turn cap, a
//!   cancellation or a tripped guardrail, and [`rollouts::terminal_metadata_for_result`] reads
//!   those from its finish reason. A failed run's exception type is its error's stable code.
//! - **A segment's input is the run's own input.** The reference records the items its SDK session
//!   saved for the run, or the caller's input to a server-managed conversation; a run here has
//!   neither, so the segment records the input the run started from.
//! - **Rollouts are grouped by the host's group id or not at all.** The reference groups runs by
//!   server-side conversation, then SDK session, then `group_id`; a run here has only the last.

mod agents;
mod json;
pub mod manager;
pub mod phase_one;
pub mod phase_two;
pub mod prompts;
pub mod rollouts;
pub mod storage;

pub use agents::{MODEL_INSTANCE_PROVIDER, MemoryRunConfig};
