//! Which runs belong together.
//!
//! A port of the reference's `run_internal/run_grouping.py`. Several runs are one conversation
//! when they share a server-side conversation, an SDK session, or a group the host names; a run
//! that has none of these is a group of its own. Sandbox memory appends every run of one group to
//! the same rollout file.

use ra_core::session::Session;

/// What a group was resolved from, in the reference's order of preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunGroupingKind {
    /// A server-side conversation.
    Conversation,
    /// An SDK session.
    Session,
    /// A group the host named on the run configuration.
    Group,
    /// Nothing: a value generated for this run alone.
    Run,
}

/// The group a run belongs to: a server-side conversation, then an SDK session, then the host's
/// group id, then a generated per-run value. Blank values are skipped and the others are trimmed.
pub(crate) fn resolve_run_grouping(
    conversation_id: Option<&str>,
    session: Option<&dyn Session>,
    group_id: Option<&str>,
) -> (RunGroupingKind, String) {
    if let Some(conversation_id) = conversation_id.map(str::trim).filter(|id| !id.is_empty()) {
        return (RunGroupingKind::Conversation, conversation_id.to_owned());
    }
    if let Some(session_id) = session
        .map(|session| session.session_id().as_str().trim())
        .filter(|id| !id.is_empty())
    {
        return (RunGroupingKind::Session, session_id.to_owned());
    }
    if let Some(group_id) = group_id.map(str::trim).filter(|id| !id.is_empty()) {
        return (RunGroupingKind::Group, group_id.to_owned());
    }
    (
        RunGroupingKind::Run,
        uuid::Uuid::new_v4().simple().to_string(),
    )
}

/// The group's id: the resolved value, with a generated one prefixed `run-`.
pub(crate) fn resolve_run_grouping_id(
    conversation_id: Option<&str>,
    session: Option<&dyn Session>,
    group_id: Option<&str>,
) -> String {
    match resolve_run_grouping(conversation_id, session, group_id) {
        (RunGroupingKind::Run, value) => format!("run-{value}"),
        (_, value) => value,
    }
}
