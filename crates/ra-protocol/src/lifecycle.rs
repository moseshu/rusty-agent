//! A turn's end is not a thread's: a completed turn neither closes the connection nor releases the
//! thread while work it started — background terminals, spawned agents — is still running, as in
//! Codex's app-server.
