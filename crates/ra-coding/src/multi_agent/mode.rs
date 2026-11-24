// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Selects and renders the coding product's multi-agent mode developer message.

use ra_core::sandbox::token_truncation::{TruncationPolicy, truncate_text};

use crate::config::catalog::MultiAgentModeMessages;

const MAX_CUSTOM_TOKENS: i64 = 400;

const EXPLICIT: &str = "Any earlier instruction enabling proactive multi-agent delegation no longer applies. Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
const PROACTIVE: &str = "Proactive multi-agent delegation is active. Any earlier developer instruction requiring an explicit user request before spawning sub-agents no longer applies. This mode remains active until a later multi-agent mode developer message changes it. User requests override this hint.\n\nIf at any point you can parallelize work by delegating tasks to another agent (no matter if you are root or subagent), you should do so using collaboration tools if it could save time or improve quality.";

pub(super) fn instructions(
    configured: Option<&str>,
    catalog: Option<&MultiAgentModeMessages>,
    ultra: bool,
) -> Option<String> {
    let custom = configured
        .or_else(|| catalog.and_then(|mode| mode.hint_text.as_deref()))
        .or_else(|| {
            catalog.and_then(|mode| {
                if ultra {
                    mode.proactive.as_deref()
                } else {
                    mode.explicit.as_deref()
                }
            })
        });
    let body = match custom {
        Some("") => return None,
        Some(text) => truncate_text(text, TruncationPolicy::tokens(MAX_CUSTOM_TOKENS)),
        None if ultra => PROACTIVE.to_owned(),
        None => EXPLICIT.to_owned(),
    };
    Some(format!("<multi_agent_mode>{body}</multi_agent_mode>"))
}
