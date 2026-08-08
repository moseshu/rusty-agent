//! # `ra-protocol`
//!
//! The bidirectional control protocol, transports, and the app server.
//!
//! **Boundary**: it owns the wire protocol and lifecycle between the host and the agent process.
//! It implements no runner, no session storage, and no UI; persistence is delegated to
//! `ra-session` and execution to the assembly above.
//!
//! **Stability**: `Evolving`. Frames are the wire protocol shared with the host, so **fields may
//! only be added, never removed**, and unknown fields must be preserved verbatim — the two sides
//! do not upgrade in lockstep.

pub mod control;
pub mod frame;
pub mod lifecycle;
pub mod server;
pub mod subscribe;
pub mod transport;
