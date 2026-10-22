//! Stable cache grouping after model-call input filters.
//!
//! Ported from the Agents SDK prompt-cache resolver at f53c62451c7f34c15f0c87fa745e05404025420f;
//! see NOTICE for the original source paths and retained MIT license.

use ra_core::{prompt::ContentHash, session::Session, state::RunState};

use super::{
    RunConfig,
    grouping::{RunGroupingKind, resolve_run_grouping},
};
use crate::turn::prepare::PreparedTurn;

/// Adds one key per run, unless the product supplies an override or the adapter opts out.
pub(super) fn attach(
    prepared: PreparedTurn,
    config: &RunConfig,
    state: &mut RunState,
    session: Option<&dyn Session>,
) -> PreparedTurn {
    if let Some(key) = config.prompt_cache_key_override() {
        return prepared.map_request(|request| request.with_prompt_cache_key(key));
    }
    if !prepared
        .model()
        .supports_default_prompt_cache_key(prepared.request().model_settings())
    {
        return prepared;
    }
    let key = if let Some(key) = state.generated_prompt_cache_key() {
        key.to_owned()
    } else {
        // The runner has no server-conversation input yet, as with sandbox rollout grouping.
        let (kind, value) = resolve_run_grouping(None, session, config.group_id());
        let key = key_for_grouping(kind, &value);
        state.set_generated_prompt_cache_key(key.clone());
        key
    };
    prepared.map_request(|request| request.with_prompt_cache_key(key))
}

pub(super) fn key_for_grouping(kind: RunGroupingKind, value: &str) -> String {
    let name = match kind {
        RunGroupingKind::Conversation => "conversation",
        RunGroupingKind::Session => "session",
        RunGroupingKind::Group => "group",
        RunGroupingKind::Run => return format!("agents-sdk:run:{value}"),
    };
    let hash = ContentHash::compute(value);
    format!("agents-sdk:{name}:{}", &hash.as_str()[..32])
}
