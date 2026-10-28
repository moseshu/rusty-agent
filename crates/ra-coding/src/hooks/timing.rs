// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Command-hook timeout normalization before dispatch.

use ra_core::hook::HookEventName;
use std::path::Path;

pub(crate) const SESSION_END_DEFAULT_TIMEOUT_SEC: u64 = 1;
pub(crate) const SESSION_END_MAX_TIMEOUT_SEC: u64 = 3;

/// Normalizes hook timeouts. `SessionEnd` and Interrupt default to one second and are capped at three
/// seconds; all other hooks keep the standard ten-minute default.
pub fn normalize_command_hook(
    event_name: HookEventName,
    timeout_sec: Option<u64>,
    source_path: &Path,
    warnings: &mut Vec<String>,
) -> u64 {
    match event_name {
        HookEventName::SessionEnd | HookEventName::Interrupt => {
            let max_timeout_sec = SESSION_END_MAX_TIMEOUT_SEC;
            if timeout_sec.is_some_and(|timeout_sec| timeout_sec > max_timeout_sec) {
                warnings.push(format!(
                    "clamping {} hook timeout to {max_timeout_sec}s in {}",
                    if event_name == HookEventName::SessionEnd {
                        "SessionEnd"
                    } else {
                        "Interrupt"
                    },
                    source_path.display()
                ));
            }
            timeout_sec
                .unwrap_or(SESSION_END_DEFAULT_TIMEOUT_SEC)
                .clamp(1, max_timeout_sec)
        }
        _ => timeout_sec.unwrap_or(600).max(1),
    }
}
