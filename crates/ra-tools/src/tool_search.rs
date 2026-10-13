//! Deferred discovery is assembled by `ra-runtime` from each turn's enabled registry snapshot.
//!
//! It needs that exact snapshot and a binding-owned cache, so it is installed during preparation
//! rather than exposed as a standalone tool that could index disabled or host-only capabilities.
