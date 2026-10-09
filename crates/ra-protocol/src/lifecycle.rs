//! A turn's end is not a thread's: a completed turn closes neither the connection nor the thread,
//! and what its tools left running — background terminals — keeps running, as in Codex's
//! app-server. A thread is released only once it has had no subscribers and no active turn for the
//! server's unload delay, and a connection's close removes only its subscriptions; see
//! [`crate::server`].
