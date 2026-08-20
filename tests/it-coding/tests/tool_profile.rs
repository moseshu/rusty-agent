//! The coding agent's three tool surfaces: what each one declares, and what it costs.

use std::sync::Arc;

use async_trait::async_trait;
use ra_coding::CodingProfile;
use ra_core::{
    error::Result,
    tool::{Tool, ToolContext, ToolLookupKey, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::tool::{profile::ToolSelection, registry::ToolRegistry};
use ra_tools::{exec_command::ExecCommandTool, read_file::ReadFileTool};
use serde_json::json;

/// A stand-in for a tool this product has declared but not yet written.
///
/// The profile's job is to hold the surface to a shape; proving it does so must not wait for
/// fifteen implementations.
struct StubTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl StubTool {
    fn shared(name: &str) -> Arc<dyn Tool> {
        Arc::new(Self {
            origin: ToolOrigin::new(name).expect("a valid tool name"),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .expect("a strict schema"),
        })
    }
}

#[async_trait]
impl Tool for StubTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("unused"))
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new()
    }
}

fn selected_keys(profile: CodingProfile) -> Vec<ToolLookupKey> {
    match profile
        .to_tool_profile()
        .expect("a valid profile")
        .selection()
    {
        ToolSelection::Explicit(keys) => keys.iter().cloned().collect(),
        other => panic!("{profile:?} should name its tools, got {other:?}"),
    }
}

fn selected_names(profile: CodingProfile) -> Vec<String> {
    selected_keys(profile)
        .iter()
        .map(|key| key.name().to_owned())
        .collect()
}

/// A registry holding exactly the tools a tier declares, standing in for the real ones.
fn registry_for(profile: CodingProfile) -> ToolRegistry {
    let mut builder = ToolRegistry::builder();
    for key in selected_keys(profile) {
        builder = builder.register(StubTool::shared(key.name()));
    }
    builder.build().expect("a valid registry")
}

#[test]
fn test_the_default_surface_is_the_codex_like_one() {
    // Fifteen entries against Codex's measured sixteen. The default is the tier the product is
    // actually sized for, not the smallest one it can run.
    assert_eq!(CodingProfile::default(), CodingProfile::CodexLike);
    assert_eq!(selected_keys(CodingProfile::CodexLike).len(), 15);
}

#[test]
fn test_every_tier_declares_a_surface_that_fits_its_own_budget() {
    for tier in [CodingProfile::Core, CodingProfile::CodexLike] {
        let profile = tier.to_tool_profile().expect("a valid profile");
        let declared = selected_keys(tier).len();
        let budget = profile.budget();

        assert!(
            declared >= budget.min_advertised() && declared <= budget.max_advertised(),
            "{tier:?} declares {declared} entries, outside its own budget of {}..={}",
            budget.min_advertised(),
            budget.max_advertised()
        );
    }
}

#[test]
fn test_the_tiers_hold_the_measured_bounds() {
    let core = CodingProfile::Core
        .to_tool_profile()
        .expect("a valid profile");
    assert_eq!(core.id().as_str(), "core");
    assert_eq!(core.budget().min_advertised(), 6);
    assert_eq!(core.budget().max_advertised(), 8);

    let codex_like = CodingProfile::CodexLike
        .to_tool_profile()
        .expect("a valid profile");
    assert_eq!(codex_like.id().as_str(), "codex_like");
    assert_eq!(codex_like.budget().min_advertised(), 14);
    assert_eq!(codex_like.budget().max_advertised(), 16);

    // The ceiling is Claude Code's 24; the floor stays the standard surface's, because `full` is
    // that surface plus whatever else the host installed.
    let full = CodingProfile::Full
        .to_tool_profile()
        .expect("a valid profile");
    assert_eq!(full.id().as_str(), "full");
    assert_eq!(full.budget().min_advertised(), 14);
    assert_eq!(full.budget().max_advertised(), 24);
    assert_eq!(full.selection(), &ToolSelection::AllRegistered);

    // 20 KB is the whole-surface schema budget, and it applies to every tier: a smaller surface
    // made of larger entries costs the same per turn.
    for tier in [
        CodingProfile::Core,
        CodingProfile::CodexLike,
        CodingProfile::Full,
    ] {
        let profile = tier.to_tool_profile().expect("a valid profile");
        assert_eq!(
            profile.budget().max_advertised_bytes(),
            Some(20 * 1024),
            "{tier:?}"
        );
    }
}

#[test]
fn test_the_standard_surface_contains_the_core_one() {
    let core = selected_names(CodingProfile::Core);
    let codex_like = selected_names(CodingProfile::CodexLike);

    for name in &core {
        assert!(
            codex_like.contains(name),
            "`{name}` is in core but not in codex_like"
        );
    }
    // Widening a tier must not swap tools out from under the prompt that describes them.
    assert_eq!(codex_like.len(), core.len() + 9);
}

#[test]
fn test_the_execution_and_observation_entries_are_in_every_tier() {
    let core = selected_names(CodingProfile::Core);

    // The four that do the work plus the two that produce structured observations. `grep` and
    // `glob` are here rather than left to `exec_command` because a run that shells out for search
    // gets raw text instead of match counts and skip reasons.
    for name in [
        "exec_command",
        "write_stdin",
        "apply_patch",
        "read_file",
        "grep",
        "glob",
    ] {
        assert!(core.contains(&name.to_owned()), "core is missing `{name}`");
    }
    assert_eq!(core.len(), 6);
}

#[test]
fn test_a_tier_assembles_into_a_surface_within_its_budget() {
    for tier in [CodingProfile::Core, CodingProfile::CodexLike] {
        let profile = tier.to_tool_profile().expect("a valid profile");
        let surface = registry_for(tier)
            .assemble(&profile)
            .expect("a declared tier assembles");

        assert_eq!(surface.advertised_count(), selected_keys(tier).len());
        assert!(
            surface.advertised_bytes() <= 20 * 1024,
            "{tier:?} advertises {} bytes",
            surface.advertised_bytes()
        );
    }
}

#[test]
fn test_a_tier_refuses_to_assemble_while_a_declared_tool_is_missing() {
    // Today's real state: two of the six core entries exist. The tier fails loudly rather than
    // shipping a four-entry surface whose prompt describes six.
    let registry = ToolRegistry::builder()
        .register(Arc::new(ReadFileTool::new().expect("read_file builds")))
        .register(Arc::new(
            ExecCommandTool::new().expect("exec_command builds"),
        ))
        .build()
        .expect("a valid registry");

    let error = registry
        .assemble(
            &CodingProfile::Core
                .to_tool_profile()
                .expect("a valid profile"),
        )
        .expect_err("an incomplete tool set must fail assembly");

    let message = error.to_string();
    assert!(message.contains("core"), "{message}");
    assert!(message.contains("apply_patch"), "{message}");
}

#[test]
fn test_the_declared_names_match_the_tools_that_exist() {
    // The declared list is only a specification if it is the same identity the implementations
    // register under. These two are written, so they can be checked against it now.
    let registry = ToolRegistry::builder()
        .register(Arc::new(ReadFileTool::new().expect("read_file builds")))
        .register(Arc::new(
            ExecCommandTool::new().expect("exec_command builds"),
        ))
        .build()
        .expect("a valid registry");

    for key in selected_keys(CodingProfile::Core) {
        if matches!(key.name(), "read_file" | "exec_command") {
            assert!(
                registry.contains(&key),
                "core declares `{}`, which no implementation registers under that key",
                key.name()
            );
        }
    }
}
