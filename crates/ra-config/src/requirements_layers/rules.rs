// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Requirements rules are additive across layers. Higher-priority rules are
//! appended first so the final rule order keeps priority visible.

#![allow(
    missing_docs,
    reason = "Preserve source product configuration and validation records."
)]
use crate::RequirementSource;
use crate::RequirementsExecPolicyToml;
use crate::Sourced;

use super::stack::merge_output_source;

pub(super) fn merge(
    target: &mut Option<Sourced<RequirementsExecPolicyToml>>,
    incoming: Option<RequirementsExecPolicyToml>,
    source: &RequirementSource,
) {
    let Some(incoming) = incoming else {
        return;
    };
    let Some(existing) = target.as_mut() else {
        *target = Some(Sourced::new(incoming, source.clone()));
        return;
    };

    let RequirementsExecPolicyToml { prefix_rules } = incoming;
    existing.value.prefix_rules.extend(prefix_rules);
    merge_output_source(&mut existing.source, source);
}
