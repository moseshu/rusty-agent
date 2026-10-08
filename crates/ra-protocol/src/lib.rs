//! # `ra-protocol`
//!
//! The bidirectional control protocol, transports, and the app server.
//!
//! **Boundary**: it owns the wire protocol and lifecycle between the host and the agent process.
//! It implements no runner, no session storage, and no UI: a thread's turns run on the live
//! thread of `ra-runtime` the host builds, which the server starts, steers and interrupts, as
//! Codex's app-server drives its core's threads; persistence is delegated to `ra-session`.
//!
//! **Stability**: `Evolving`. Frames are the wire protocol shared with the host, so **fields may
//! only be added, never removed**, and what a newer peer adds to parameters and results must reach
//! their reader untouched — the two sides do not upgrade in lockstep. Envelope members the format
//! does not define, such as `jsonrpc`, are ignored, as Codex's app-server ignores them.

mod absolute_path;

pub mod control;
pub mod frame;
pub mod lifecycle;
pub mod server;
pub mod subscribe;
pub mod transport;
