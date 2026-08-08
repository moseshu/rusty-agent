//! # `ra-core`
//!
//! Kernel types and contracts: no implementation, only types, traits, and constants. The
//! constitution of the framework.
//!
//! **Boundary**: this crate defines the protocol-neutral types shared across layers and nothing
//! else. It performs no IO, chooses no provider, assembles no loop, and contains no product
//! vocabulary. Service implementations must depend on this crate, never the reverse.
//!
//! **Stability**: `Stable` — it is the framework constitution, and downstream code matches on
//! these enums and implements these traits directly. Two deliberate exceptions: the `RunState`
//! fields in [`state`] and the field names in [`trace::field`] are `Evolving` (they may grow but
//! not shrink), and the turn-settlement intermediates in [`step`] are `Internal` (once they leak
//! into the public API, R1 and R3 can no longer be refactored).
//!
//! [`finish::FinishReason`] is `Stable` and lives in its own module rather than beside `NextStep`
//! for exactly that reason: the run's stopping reason is something hosts, graph edges, and the
//! closeout step all read, so it must not inherit the `Internal` grade of the settlement
//! intermediate that produces it.

pub mod agent;
pub mod budget;
pub mod cancel;
pub mod capability;
pub mod compat;
pub mod config;
pub mod error;
pub mod finish;
pub mod guard;
pub mod hook;
pub mod item;
pub mod model;
pub mod permission;
pub mod prompt;
pub mod session;
pub mod state;
pub mod step;
pub mod tool;
pub mod trace;
pub mod usage;
