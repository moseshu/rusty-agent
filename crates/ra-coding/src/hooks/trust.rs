// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Normalized trust identity operations; authority and normalization come from real discovery.

use crate::config::{HookHandlerConfig, HookStateToml, MatcherGroup, version_for_toml};
use ra_core::hook::HookEventName;
use ra_protocol::server::protocol::HookTrustStatus;
use serde::Serialize;
use toml::Value as TomlValue;

/// Hash a normalized, config-derived identity instead of source text so equivalent
/// hooks from config TOML and hooks.json converge on the same trust identity.
#[derive(Serialize)]
struct NormalizedHookIdentity {
    event_name: &'static str,
    #[serde(flatten)]
    group: MatcherGroup,
}

/// Hashes one already normalized handler, excluding unrelated siblings in the group.
///
/// Normalization of command selection and output limits belongs to source discovery.
/// Supply the normalized config identity before runtime environment substitution.
///
/// # Panics
/// Panics if the identity cannot be represented as TOML. This includes a timeout above
/// `i64::MAX`, an `additional_context_limit` above `i64::MAX` on 64-bit targets, or
/// non-TOML-representable MCP input values. Valid JSON declarations can contain these
/// large numbers, and ordinary-event timeout normalization does not reduce them.
/// This reachable panic preserves the upstream behavior.
#[must_use]
pub fn hook_hash(
    event_name: HookEventName,
    matcher: Option<&str>,
    group: &MatcherGroup,
    normalized_handler: &HookHandlerConfig,
) -> String {
    let mut group = group.clone();
    group.matcher = matcher.map(ToOwned::to_owned);
    group.hooks = vec![normalized_handler.clone()];
    let identity = NormalizedHookIdentity {
        event_name: event_name.as_str(),
        group,
    };
    let Ok(value) = TomlValue::try_from(identity) else {
        unreachable!("normalized hook identity should serialize to TOML");
    };
    version_for_toml(&value)
}

/// Computes metadata from loader-supplied managed/built-in authority and the saved hash.
#[must_use]
pub fn hook_trust_status(
    is_managed: bool,
    is_builtin: bool,
    current_hash: &str,
    trusted_hash: Option<&str>,
) -> HookTrustStatus {
    if is_builtin {
        HookTrustStatus::Trusted
    } else if is_managed {
        HookTrustStatus::Managed
    } else {
        match trusted_hash {
            Some(trusted_hash) if trusted_hash == current_hash => HookTrustStatus::Trusted,
            Some(_) => HookTrustStatus::Modified,
            None => HookTrustStatus::Untrusted,
        }
    }
}

/// Managed and built-in handlers ignore ordinary per-handler disablement.
/// This predicate does not establish trust or evaluate source policy.
#[must_use]
pub fn hook_enabled(is_managed: bool, is_builtin: bool, state: Option<&HookStateToml>) -> bool {
    is_builtin || is_managed || state.and_then(|state| state.enabled) != Some(false)
}

/// Managed handlers do not consume ordinary saved trust hashes.
#[must_use]
pub fn hook_trusted_hash(is_managed: bool, state: Option<&HookStateToml>) -> Option<&str> {
    (!is_managed)
        .then(|| state.and_then(|state| state.trusted_hash.as_deref()))
        .flatten()
}
