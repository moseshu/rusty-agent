// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex cache-key calculation, including the internal-source branch awaiting parent wiring.
use ra_coding::prompt_cache_key::prompt_cache_key;
use ra_core::session::SessionId;
use ra_protocol::server::{InternalSessionSource, SessionSource};

#[test]
fn override_internal_parent_and_session_fallback_follow_codex_order() {
    let session = SessionId::new("root");
    let parent = SessionId::new("parent");
    for (source, expected) in [
        (
            SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
            "memory_consolidation:parent",
        ),
        (
            SessionSource::Internal(InternalSessionSource::Guardian),
            "guardian:parent",
        ),
        (SessionSource::Cli, "root"),
    ] {
        assert_eq!(
            prompt_cache_key(None, &source, &session, Some(&parent)),
            expected
        );
        assert_eq!(prompt_cache_key(None, &source, &session, None), "root");
        assert_eq!(
            prompt_cache_key(Some("override"), &source, &session, Some(&parent)),
            "override"
        );
    }
}
