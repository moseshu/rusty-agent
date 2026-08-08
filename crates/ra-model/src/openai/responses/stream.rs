//! Responses SSE event aggregation lands in R1-7.
//!
//! R1-4 keeps `Model::stream_response` usable by performing one non-streaming request and emitting
//! a completed raw event followed by normalized items. It deliberately does not claim token-level
//! streaming or terminal backfill semantics before the shared stream state machine exists.
