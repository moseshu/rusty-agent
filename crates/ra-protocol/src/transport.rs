//! How messages travel: one JSON message per line over stdio, or WebSocket frames, as Codex's
//! app-server transports carry them.

pub mod stdio;
pub mod websocket;
