// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex cache routing without changing thread storage or lifecycle identities.

use ra_core::session::{SessionId, rollout::RolloutThreadSpawn};
use ra_protocol::server::{
    InternalSessionSource, SessionSource, ThreadFactoryRequest, ThreadOrigin,
};

/// Resolves Codex's override, internal-parent and session fallback order.
///
/// Internal-parent routing is available to product callers. The server's thread/start API does
/// not yet accept a parent, so an internal root started there uses the session fallback.
#[must_use]
pub fn prompt_cache_key(
    key_override: Option<&str>,
    source: &SessionSource,
    session_id: &SessionId,
    parent_thread_id: Option<&SessionId>,
) -> String {
    if let Some(key) = key_override {
        return key.to_owned();
    }
    if let (SessionSource::Internal(source), Some(parent)) = (source, parent_thread_id) {
        let source = match source {
            InternalSessionSource::MemoryConsolidation => "memory_consolidation",
            InternalSessionSource::Guardian => "guardian",
            _ => return session_id.as_str().to_owned(),
        };
        return format!("{source}:{parent}");
    }
    session_id.as_str().to_owned()
}

pub(crate) fn session_id(request: &ThreadFactoryRequest) -> &SessionId {
    request
        .thread_spawn()
        .map_or(request.thread_id(), RolloutThreadSpawn::root_session_id)
}

pub(crate) fn for_thread(request: &ThreadFactoryRequest) -> String {
    let key_override = if request.ephemeral()
        && !request.session_source().is_non_root_agent()
        && matches!(request.origin(), ThreadOrigin::Fork { .. })
    {
        request.fork_session_id().map(SessionId::as_str)
    } else {
        None
    };
    prompt_cache_key(
        key_override,
        request.session_source(),
        session_id(request),
        request
            .thread_spawn()
            .map(RolloutThreadSpawn::parent_session_id),
    )
}
