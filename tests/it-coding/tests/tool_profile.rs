//! The coding agent's three tool surfaces: what each one declares, and what it costs.

use std::sync::Arc;

use async_trait::async_trait;
use ra_coding::{CodingHost, CodingProfile, prompt::assemble_stable_prefix_for_tools};
use ra_core::{
    error::Result,
    prompt::PromptRole,
    tool::{Tool, ToolContext, ToolLookupKey, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::tool::{profile::ToolSelection, registry::ToolRegistry};
use ra_tools::read_file::ReadFileTool;
use serde_json::json;
use tempfile::TempDir;

/// The line the schema budget prints its measurement on, and that `cargo xtask token-budget`
/// reads back.
///
/// The gate quotes this number instead of only reporting that a test passed — the latter is what
/// the `test` gate already does. It also means a gate that stops finding this line goes red: a
/// binary from which the measurement was renamed away passes perfectly well, and a budget nobody
/// measures is not a budget.
const MEASUREMENT_MARKER: &str = "tool-schema-budget:";

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

/// Every core tool that has a real implementation today, under the identity it registers with.
///
/// This list is the one place a newly written core tool has to be added, and the tests below read
/// today's state off it rather than restating it — a test that hard-codes which tools exist stops
/// checking the newest one on the day it lands.
///
/// The workspace only has to exist so the patch tool can bind its root capability; nothing here
/// writes to it.
fn implemented_core_tools(workspace: &TempDir) -> Vec<Arc<dyn Tool>> {
    let host = CodingHost::open(workspace.path()).expect("the host opens a workspace");
    vec![
        Arc::new(ReadFileTool::new().expect("read_file builds")),
        host.exec_command_tool().expect("exec_command builds"),
        host.write_stdin_tool().expect("write_stdin builds"),
        host.apply_patch_tool().expect("apply_patch builds"),
    ]
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
        assert_eq!(
            profile.budget().max_advertised_name_chars(),
            Some(24 * 64),
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
fn test_the_implemented_working_tools_stay_within_their_share_of_the_surface() {
    let workspace = TempDir::new().expect("a workspace");
    let tools = implemented_core_tools(&workspace);
    let bytes: usize = tools
        .iter()
        .map(|tool| {
            tool.model_definition()
                .advertised_bytes()
                .expect("a renderable tool definition")
        })
        .sum();

    // The budget is derived rather than restated. The ceiling is what one turn may advertise and
    // the default tier says how many entries share it, so an entry's fair share is that quotient,
    // and what is implemented today may hold that many shares. A fixed total would instead have to
    // be raised by whoever writes the next tool — which teaches raising it without looking, the
    // same reason the tiers are bands rather than exact counts. As a share it grows when a tool
    // lands, and only a schema that is fat for its slot turns this red.
    let default = CodingProfile::default();
    let ceiling = default
        .to_tool_profile()
        .expect("a valid profile")
        .budget()
        .max_advertised_bytes()
        .expect("the surface declares a byte ceiling");
    let share = ceiling / selected_keys(default).len();
    let budget = share * tools.len();

    println!("{MEASUREMENT_MARKER} {bytes}/{budget}");
    assert!(
        bytes <= budget,
        "{} implemented working tools advertise {bytes} bytes, above the {budget} they may hold \
         ({share} each of the surface's {ceiling}). Shrink a description or a schema; raising the \
         ceiling moves the cost onto every turn.",
        tools.len()
    );
}

#[test]
fn test_a_tier_refuses_to_assemble_while_a_declared_tool_is_missing() {
    // Some of the six core entries are still unwritten. The tier fails loudly rather than shipping
    // a smaller surface than its prompt describes, and it names one of the entries that is gone.
    let workspace = TempDir::new().expect("a workspace");
    let registry = ToolRegistry::builder()
        .register_all(implemented_core_tools(&workspace))
        .build()
        .expect("a valid registry");

    let missing: Vec<String> = selected_keys(CodingProfile::Core)
        .into_iter()
        .filter(|key| !registry.contains(key))
        .map(|key| key.name().to_owned())
        .collect();
    assert!(
        !missing.is_empty(),
        "every core tool now exists; this test has outlived its subject and should be replaced \
         by one that assembles the tier"
    );

    let error = registry
        .assemble(
            &CodingProfile::Core
                .to_tool_profile()
                .expect("a valid profile"),
        )
        .expect_err("an incomplete tool set must fail assembly");

    let message = error.to_string();
    assert!(message.contains("core"), "{message}");
    assert!(
        missing.iter().any(|name| message.contains(name)),
        "the failure names none of {missing:?}: {message}"
    );
}

/// The widest surface a tier permits, at the name lengths this product's own entries use.
///
/// Filled to the tier's own ceiling rather than to what it declares today: the budget permits that
/// many entries, so the prompt has to hold that many, and the tenth ordinary tool must not be the
/// one that discovers otherwise. `Full` names nothing of its own, so it starts from the standard
/// surface — what it adds at runtime is the host's, and the length of *those* names is the variable
/// the case below this one pins.
fn widest_permitted_surface(tier: CodingProfile) -> Vec<Arc<dyn Tool>> {
    let ceiling = tier
        .to_tool_profile()
        .expect("a valid profile")
        .budget()
        .max_advertised();
    let mut names = match tier {
        CodingProfile::Full => selected_names(CodingProfile::CodexLike),
        declared => selected_names(declared),
    };
    while names.len() < ceiling {
        names.push(format!("filler_tool_{:02}", names.len()));
    }
    names.iter().map(|name| StubTool::shared(name)).collect()
}

/// The prompt's advertised inventory holds every tier, filled to that tier's own ceiling.
///
/// This section is the one part of the cached prefix whose size nobody types: it is generated from
/// the tool list, and it is refused when it overruns its allowance. Refused means the agent does
/// not build — so the tiers this product ships have to fit with the ceiling where it stands, and
/// this is where adding the next tool finds that out.
#[test]
fn test_the_prompt_inventory_holds_every_tier_at_its_own_ceiling() {
    for tier in [
        CodingProfile::Core,
        CodingProfile::CodexLike,
        CodingProfile::Full,
    ] {
        let tools = widest_permitted_surface(tier);
        let prefix = assemble_stable_prefix_for_tools(&PromptRole::Main, &tools)
            .unwrap_or_else(|error| panic!("{tier:?} at {} entries: {error}", tools.len()));
        let inventory = prefix
            .sections()
            .iter()
            .find(|section| section.name().as_str() == "tool_surface")
            .expect("an advertised surface renders an inventory section");

        // No `MEASUREMENT_MARKER` line here, deliberately: the token-budget gate reads the first
        // marked line it finds, and that one belongs to the byte budget above.
        assert_eq!(
            inventory
                .content()
                .lines()
                .filter(|line| line.starts_with("- `"))
                .count(),
            tools.len(),
            "{tier:?} lost an entry between its ceiling and the prompt that names them"
        );
    }
}

/// MCP-style names fit the explicit aggregate-name budget at the full entry ceiling.
#[test]
fn test_an_mcp_length_surface_fits_the_prompt_allowance() {
    let ceiling = CodingProfile::Full
        .to_tool_profile()
        .expect("a valid profile")
        .budget()
        .max_advertised();
    let tools: Vec<Arc<dyn Tool>> = (0..ceiling)
        .map(|index| StubTool::shared(&format!("mcp__issue_tracker__create_issue_{index:02}")))
        .collect();

    assemble_stable_prefix_for_tools(&PromptRole::Main, &tools)
        .expect("an MCP-style full surface must assemble");
}

/// The declared aggregate-name ceiling, not a shorter incidental spelling, sizes the inventory.
#[test]
fn test_the_full_name_budget_fits_the_prompt_inventory() {
    let ceiling = CodingProfile::Full
        .to_tool_profile()
        .expect("a valid profile")
        .budget()
        .max_advertised();
    let tools: Vec<Arc<dyn Tool>> = (0..ceiling)
        .map(|index| {
            let name = format!("tool_{index:02}_{}", "n".repeat(56));
            assert_eq!(name.chars().count(), 64, "{name}");
            StubTool::shared(&name)
        })
        .collect();

    assemble_stable_prefix_for_tools(&PromptRole::Main, &tools)
        .expect("the declared full name budget must fit the inventory");
}

/// One character past the ceiling is refused by the prompt inventory itself.
///
/// The registry checks the same ceiling, but the prompt path does not go through it: an inventory
/// is built from whatever tool list the agent construction path hands over. Without its own check
/// the section would grow past the allowance it declares, and the failure would arrive as a budget
/// overrun naming no cause.
#[test]
fn test_a_name_list_past_the_ceiling_is_refused_by_the_prompt_inventory() {
    let ceiling = CodingProfile::Full
        .to_tool_profile()
        .expect("a valid profile")
        .budget()
        .max_advertised();
    let tools: Vec<Arc<dyn Tool>> = (0..ceiling)
        .map(|index| {
            // One entry carries the extra character, so the count stays inside the tier and the
            // aggregate name length is the only thing over.
            let padding = if index == 0 { 57 } else { 56 };
            StubTool::shared(&format!("tool_{index:02}_{}", "n".repeat(padding)))
        })
        .collect();

    let error = assemble_stable_prefix_for_tools(&PromptRole::Main, &tools)
        .expect_err("a name list over the declared ceiling must not assemble");

    let message = error.to_string();
    assert!(
        message.contains("characters in model-facing tool names"),
        "{message}"
    );
    assert!(message.contains("1536"), "{message}");
}

#[test]
fn test_the_declared_names_match_the_tools_that_exist() {
    // The declared list is only a specification if it is the same identity the implementations
    // register under. Checking it in this direction — every implementation lands on a declared
    // key — is what keeps a newly written tool inside the test instead of beside it.
    let workspace = TempDir::new().expect("a workspace");
    let registry = ToolRegistry::builder()
        .register_all(implemented_core_tools(&workspace))
        .build()
        .expect("a valid registry");
    let declared = selected_keys(CodingProfile::Core);

    for key in registry.keys() {
        assert!(
            declared.contains(key),
            "`{}` is implemented, but core declares no such lookup key",
            key.name()
        );
    }
    assert_eq!(
        registry.len(),
        4,
        "a core tool was written without being added to `implemented_core_tools`, or one was \
         removed"
    );
}
