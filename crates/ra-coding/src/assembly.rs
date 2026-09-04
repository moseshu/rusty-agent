//! The committed record of what every tier and role actually assemble into.
//!
//! A tool surface only ever grows, and every entry on it is paid for on every turn of every run:
//! the schema table the provider is sent, plus the inventory and the capability paragraphs that
//! describe it in the cached prefix. Each addition is individually reasonable and nothing fails, so
//! the only moment the trade is visible is the one where the numbers are written down and diffed.
//! That is what this record is.
//!
//! # What it covers that the other two records do not
//!
//! [`dump`](crate::prompt::dump) covers one prefix per role, plus the host-backed prefix of the
//! main role at the tier the product builds by default; the tool-surface digest covers the names and
//! the schema fingerprint of that one surface. Neither says what any *other* tier and role produce,
//! and neither states what the advertised schemas cost — which is usually the larger half of what a
//! turn pays for.
//!
//! So this record is the matrix: every tier this product ships, in every role it ships, with the
//! entries each installed capability put on the surface, what those entries cost in advertised
//! schema bytes, what each capability's prompt fragment costs, and what the assembled prefix came
//! to against the allowance its own sections declared. A profile that cannot assemble also records
//! its declared entries and budget beside the refusal; the unavailable measurements stay explicitly
//! unavailable rather than becoming fabricated zeroes.
//!
//! **It deliberately carries no section text and no hashes.** Those belong to the prompt dump and to
//! the reviewed prefix snapshots; recording them here as well would make one edit move two artifacts
//! that answer for the same fact, and the second one would be the copy nobody reads.
//!
//! # Why there is no deferred column
//!
//! A capability's third prompt channel is delivered by the turn loop from the capabilities installed
//! on the *run configuration*, and every capability a tier assembles is installed on the agent
//! instead. So the deferred cost of every row here is not zero-for-now but zero-by-construction, and
//! a column of structural zeroes reads as a measurement. What holds it is an assertion rather than a
//! number: the coding host's capabilities are checked to declare no deferred text, which fails on
//! the day one of them wants some — and that is the day this product has to decide how such a
//! fragment reaches a run at all.
//!
//! Reporting the run configuration's own capabilities here instead would report the wrong number:
//! a deferred fragment is resolved from the capability *bound to a run*, and no run exists while a
//! record is being rendered.
//!
//! # Why the tiers that cannot be assembled are rows too
//!
//! Two of the three tiers name entries nobody has written yet, so they refuse to assemble. Recording
//! the refusal — in the product's own words, which name every entry the registry could not
//! provide — is the point rather than an omission: the list is the specification, the refusal is how
//! far the specification is from being met, and the day it shrinks to nothing those rows fill in with
//! a surface that has to be reviewed like any other.

use std::fmt::Write as _;

use ra_core::{
    capability::CapabilityFamily,
    error::Result,
    prompt::{PromptRole, PromptSection},
    tool::ToolLookupKey,
};
use ra_prompt::assembler::StablePrefix;
use ra_runtime::tool::{
    profile::{ToolProfile, ToolSelection},
    registry::ToolRegistry,
};

use crate::{
    agent::{HostBackedSurface, host_backed_surface},
    capabilities::RoleCapabilities,
    host::CodingHost,
    profile::CodingProfile,
    prompt::{assemble_stable_prefix_for_surface, dump::shipped_roles},
};

/// Width of the banner and the rules between a row's sections.
const RULE_WIDTH: usize = 96;

/// Width of the label column, matching the prompt dump's so the two read alike.
const LABEL_WIDTH: usize = 22;

/// Renders the assembly record for every shipped tier in every shipped role.
///
/// The rows come from [`host_backed_surface`] and the prefix from the same assembler the agent
/// builder uses, so what is recorded is what the product installs rather than a second reading of
/// the same capabilities. A tier the host cannot satisfy records both the surface it declared and
/// the refusal it produced. The declaration keeps an unwritten profile reviewable without
/// inventing schema, prompt, or token measurements that no assembled request has.
///
/// # Errors
///
/// Propagates prefix assembly failures. A tier that refuses to assemble is a row, not an error: the
/// record is about what each tier and role produce, and "this one produces nothing yet" is one of
/// the answers.
pub async fn render_capability_assembly(host: &CodingHost) -> Result<String> {
    let mut out = String::new();
    for tier in CodingProfile::SHIPPED {
        for role in shipped_roles() {
            let _ = writeln!(
                out,
                "### tier: {} / role: {}",
                tier.tier_name(),
                role.role_name()
            );
            out.push_str(&render_row(role, host, tier).await?);
            out.push('\n');
        }
    }
    Ok(out)
}

/// One tier and role: the assembled surface, or the reason there is none.
async fn render_row(role: &PromptRole, host: &CodingHost, tier: CodingProfile) -> Result<String> {
    let declared = declared_surface(role, host, tier)?;
    match host_backed_surface(role, host, tier).await {
        Ok(assembled) => {
            let prefix = assemble_stable_prefix_for_surface(
                role,
                assembled.tool_surface(),
                assembled.capability_sections(),
            )?;
            Ok(render_assembled(&assembled, &prefix))
        }
        Err(error) => Ok(render_refusal(&declared, &error.to_string())),
    }
}

/// The selection a row asks the host to assemble, before assembly proves it exists.
///
/// This is intentionally separate from an assembled surface. A profile that names a tool not yet
/// implemented has no truthful schema bytes or prefix total, but its declared entries and bounds
/// are still product policy and must appear in the committed record.
fn declared_surface(
    role: &PromptRole,
    host: &CodingHost,
    tier: CodingProfile,
) -> Result<DeclaredSurface> {
    let capabilities = RoleCapabilities::resolve(role, host)?;
    let profile = tier.to_tool_profile_for_role(
        role,
        &capabilities.withheld_tool_keys(),
        &capabilities.withheld_advertised_tool_keys(),
    )?;
    let registry = ToolRegistry::builder()
        .register_all(capabilities.tools())
        .build()?;
    Ok(DeclaredSurface {
        profile,
        registered_keys: registry.keys().cloned().collect(),
    })
}

/// The part of an assembly row that exists even when no surface can be assembled.
struct DeclaredSurface {
    profile: ToolProfile,
    registered_keys: Vec<ToolLookupKey>,
}

/// What one assembled surface holds, and what it costs.
fn render_assembled(assembled: &HostBackedSurface, prefix: &StablePrefix) -> String {
    let surface = assembled.tool_surface();
    let budget = assembled.budget();
    let mut out = banner();

    label(&mut out, "Profile:", surface.profile().as_str());
    label(
        &mut out,
        "Advertised Entries:",
        &format!(
            "{} of {}-{}",
            surface.advertised_count(),
            budget.min_advertised(),
            budget.max_advertised()
        ),
    );
    label(
        &mut out,
        "Advertised Schema:",
        &format!(
            "{} B of {}",
            surface.advertised_bytes(),
            budget
                .max_advertised_bytes()
                .map_or_else(|| "unbounded".to_owned(), |bytes| format!("{bytes} B"))
        ),
    );
    // The allowance is summed from the prefix's own sections rather than from a constant here: a
    // ceiling written down twice is a ceiling one of the two copies is wrong about. A section that
    // declared none would understate the sum, which is why a shipped section declaring an allowance
    // at all is its own assertion in the prompt regression suite.
    let declared: usize = prefix
        .sections()
        .iter()
        .filter_map(PromptSection::token_budget)
        .sum();
    label(
        &mut out,
        "Prefix Tokens:",
        &format!("~{} of {declared} declared", prefix.token_estimate()),
    );
    label(
        &mut out,
        "Withheld Families:",
        &render_families(assembled.withheld_families()),
    );

    rule(&mut out);
    let _ = writeln!(
        out,
        "{:<16}{:<36}{:<9}Budget",
        "Capability", "Advertised Entries", "Tokens"
    );
    rule(&mut out);
    if assembled.installed().is_empty() {
        out.push_str("(no capability installed)\n");
    }
    for installed in assembled.installed() {
        let fragment = prefix
            .sections()
            .iter()
            .find(|section| section.source() == &installed.family().prompt_source());
        let tokens = fragment.map_or_else(
            || "-".to_owned(),
            |section| section.token_estimate().to_string(),
        );
        let allowance = fragment
            .and_then(PromptSection::token_budget)
            .map_or_else(|| "-".to_owned(), |budget| budget.to_string());
        let _ = writeln!(
            out,
            "{:<16}{:<36}{tokens:<9}{allowance}",
            installed.family().as_str(),
            render_entries(installed.advertised())
        );
    }
    close(&mut out);
    out
}

/// A tier this product declares and this host cannot assemble, in the product's own words.
fn render_refusal(declared: &DeclaredSurface, reason: &str) -> String {
    let mut out = banner();
    let budget = declared.profile.budget();
    label(&mut out, "Profile:", declared.profile.id().as_str());
    label(
        &mut out,
        "Declared Entries:",
        &render_declared_entries(declared),
    );
    label(
        &mut out,
        "Advertised Entries:",
        &format!(
            "unavailable (declared {}-{})",
            budget.min_advertised(),
            budget.max_advertised()
        ),
    );
    label(
        &mut out,
        "Advertised Schema:",
        "unavailable (assembly refused)",
    );
    label(&mut out, "Prefix Tokens:", "unavailable (assembly refused)");
    label(&mut out, "Assembly:", "refused");
    label(&mut out, "Reason:", reason);
    close(&mut out);
    out
}

fn banner() -> String {
    format!("{:=^RULE_WIDTH$}\n", " CAPABILITY ASSEMBLY ")
}

fn rule(out: &mut String) {
    let _ = writeln!(out, "{}", "-".repeat(RULE_WIDTH));
}

fn close(out: &mut String) {
    let _ = writeln!(out, "{}", "=".repeat(RULE_WIDTH));
}

fn label(out: &mut String, label: &str, value: &str) {
    let _ = writeln!(out, "{label:<LABEL_WIDTH$}{value}");
}

fn render_entries(names: &[String]) -> String {
    if names.is_empty() {
        return "none advertised".to_owned();
    }
    names.join(", ")
}

fn render_families(families: &[CapabilityFamily]) -> String {
    if families.is_empty() {
        return "none".to_owned();
    }
    families
        .iter()
        .map(CapabilityFamily::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The selected lookup keys, including the current contents of an all-registered profile.
fn render_declared_entries(declared: &DeclaredSurface) -> String {
    let (keys, all_registered): (Vec<&ToolLookupKey>, bool) = match declared.profile.selection() {
        ToolSelection::Explicit(keys) => (keys.iter().collect(), false),
        ToolSelection::AllRegistered => (declared.registered_keys.iter().collect(), true),
        _ => return "unavailable (unknown tool selection)".to_owned(),
    };
    let entries = keys
        .iter()
        .map(|key| render_lookup_key(key))
        .collect::<Vec<_>>()
        .join(", ");
    let entries = if entries.is_empty() {
        "none".to_owned()
    } else {
        entries
    };
    if all_registered {
        format!("all registered: {entries}")
    } else {
        entries
    }
}

/// One routing identity in the product report's compact form.
fn render_lookup_key(key: &ToolLookupKey) -> String {
    key.namespace().map_or_else(
        || key.name().to_owned(),
        |namespace| format!("{namespace}.{}", key.name()),
    )
}
