//! The coding agent's three tool surfaces: what each one declares, and what it costs.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ra_coding::{
    CodingHost, CodingProfile, build_agent_with_profile, host_backed_tool_surface,
    prompt::{assemble_stable_prefix_for_surface, assemble_stable_prefix_for_tools},
};
use ra_core::{
    agent::{AgentId, AgentInstructions},
    error::Result,
    model::ModelToolDefinition,
    prompt::PromptRole,
    tool::{Tool, ToolContext, ToolLookupKey, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::tool::{
    profile::{ToolSelection, ToolSurface},
    registry::ToolRegistry,
};
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

/// A tool whose model-facing projection stops matching the one assembly recorded.
///
/// Nothing in the product does this on purpose. It stands in for every way the two readings of a
/// projection can come apart — a definition built from mutable configuration, a rename applied
/// after selection, an MCP export re-fetched between the two — because the guarantee under test is
/// that the prompt cannot name a set the request does not carry, and that guarantee has to hold
/// without trusting the projection to answer the same way twice.
struct RenamingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    renamed: Arc<AtomicBool>,
}

impl RenamingTool {
    fn shared(name: &str, renamed: Arc<AtomicBool>) -> Arc<dyn Tool> {
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
            renamed,
        })
    }
}

#[async_trait]
impl Tool for RenamingTool {
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

    fn model_definition(&self) -> ModelToolDefinition {
        if self.renamed.load(Ordering::SeqCst) {
            return ModelToolDefinition::new(
                "renamed_after_assembly",
                self.schema.input_schema().clone(),
            );
        }
        self.schema.to_model_definition()
    }
}

/// A tool that changes its model-facing name after one observation.
///
/// This models a mutable integration precisely at the seam that matters here: the registry has
/// already recorded the original name, the reconciliation sees it once more, and an unnecessary
/// third read would produce a prompt different from the reconciled projection.
struct ChangesOnSecondReadTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    reads: Arc<AtomicUsize>,
}

impl ChangesOnSecondReadTool {
    fn shared(name: &str, reads: Arc<AtomicUsize>) -> Arc<dyn Tool> {
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
            reads,
        })
    }
}

#[async_trait]
impl Tool for ChangesOnSecondReadTool {
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

    fn model_definition(&self) -> ModelToolDefinition {
        if self.reads.fetch_add(1, Ordering::SeqCst) > 0 {
            return ModelToolDefinition::new(
                "renamed_during_prefix_assembly",
                self.schema.input_schema().clone(),
            );
        }
        self.schema.to_model_definition()
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
        host.read_file_tool().expect("read_file builds"),
        host.grep_tool().expect("grep builds"),
        host.glob_tool().expect("glob builds"),
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
fn test_the_complete_core_tier_assembles_from_its_real_implementations() {
    // This is deliberately not a registry of stubs: the profile now makes the native search
    // entries available, so its complete core surface must assemble from the product host.
    let workspace = TempDir::new().expect("a workspace");
    let registry = ToolRegistry::builder()
        .register_all(implemented_core_tools(&workspace))
        .build()
        .expect("a valid registry");
    let surface = registry
        .assemble(
            &CodingProfile::Core
                .to_tool_profile()
                .expect("a valid profile"),
        )
        .expect("the complete core surface assembles");
    assert_eq!(surface.advertised_count(), 6);
}

/// Assembly refuses a tier whose declared entries are not all registered.
///
/// The happy path above cannot show this: every core entry exists now, so nothing is missing
/// unless a registry is built without one on purpose. The guarantee is what keeps a tier from
/// quietly shipping a smaller surface than the prompt it ships with describes, and it survives
/// here rather than in the test that used to prove it by waiting for an unwritten tool.
#[test]
fn test_a_tier_refuses_to_assemble_while_a_declared_tool_is_missing() {
    let workspace = TempDir::new().expect("a workspace");
    let mut tools = implemented_core_tools(&workspace);
    let withheld = tools.pop().expect("the core tier registers tools");
    let registry = ToolRegistry::builder()
        .register_all(tools)
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
    assert!(
        message.contains(withheld.origin().name()),
        "the failure does not name the withheld `{}`: {message}",
        withheld.origin().name()
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

/// The names one prefix inventories, in the order the section renders them.
fn inventoried_names(instructions: &str) -> Vec<String> {
    instructions
        .lines()
        .filter_map(|line| line.strip_prefix("- `"))
        .filter_map(|line| line.strip_suffix('`'))
        .map(str::to_owned)
        .collect()
}

/// Changing the tier changes the tools and the prompt that names them, in one step.
///
/// This is the guarantee the whole path exists for. The two tiers are assembled from one registry,
/// so nothing but the profile differs between them — and the inventory follows it entry for entry,
/// with no second place where a tier could be switched on the tools and left alone on the text.
#[test]
fn test_switching_the_tier_switches_the_tools_and_the_inventory_together() {
    let registry = registry_for(CodingProfile::CodexLike);

    let assembled = [CodingProfile::Core, CodingProfile::CodexLike].map(|tier| {
        let surface = registry
            .assemble(&tier.to_tool_profile().expect("a valid profile"))
            .expect("a declared tier assembles");
        let advertised: Vec<String> = surface.advertised_names().map(str::to_owned).collect();
        let prefix = assemble_stable_prefix_for_surface(&PromptRole::Main, &surface)
            .expect("the prefix assembles for an assembled surface");
        let instructions = prefix.system_instructions().to_owned();
        assert_eq!(
            inventoried_names(&instructions),
            advertised,
            "{tier:?} inventories a different set than it advertises"
        );
        (advertised, instructions)
    });

    let [(core, core_prefix), (codex_like, _)] = assembled;
    assert_eq!(core.len(), 6);
    assert_eq!(codex_like.len(), 15);
    for name in &core {
        assert!(
            codex_like.contains(name),
            "widening the tier dropped `{name}` from the inventory"
        );
    }
    // The narrower tier does not merely inventory less. The nine entries it drops are absent from
    // the whole prefix, which is the failure this path exists to prevent: a section elsewhere in
    // the text still telling the model to reach for a tool the request no longer carries.
    for name in codex_like.iter().filter(|name| !core.contains(name)) {
        assert!(
            !core_prefix.contains(&format!("`{name}`")),
            "`{name}` left the core tier's surface but stayed in its prompt"
        );
    }
}

/// A prompt that would name a different set than the request carries is refused.
///
/// The surface records the names the budget was charged for; the inventory is rendered from the
/// tools that surface carries. Nothing forces those two readings to agree — so they are compared,
/// and this is the case that proves the comparison is load-bearing rather than a tautology.
#[test]
fn test_a_prompt_that_disagrees_with_the_advertised_surface_is_refused() {
    let renamed = Arc::new(AtomicBool::new(false));
    let mut tools: Vec<Arc<dyn Tool>> = selected_names(CodingProfile::Core)
        .iter()
        .skip(1)
        .map(|name| StubTool::shared(name))
        .collect();
    let first = selected_names(CodingProfile::Core)
        .first()
        .expect("core names tools")
        .clone();
    tools.push(RenamingTool::shared(&first, Arc::clone(&renamed)));

    let registry = ToolRegistry::builder()
        .register_all(tools)
        .build()
        .expect("a valid registry");
    let surface = registry
        .assemble(
            &CodingProfile::Core
                .to_tool_profile()
                .expect("a valid profile"),
        )
        .expect("the core tier assembles");
    assemble_stable_prefix_for_surface(&PromptRole::Main, &surface)
        .expect("an agreeing surface assembles a prefix");

    renamed.store(true, Ordering::SeqCst);
    let error = assemble_stable_prefix_for_surface(&PromptRole::Main, &surface)
        .expect_err("a prompt naming a different set must not assemble");

    let message = error.to_string();
    assert!(message.contains("core"), "{message}");
    assert!(
        message.contains("renamed_after_assembly"),
        "the failure does not name what only the prompt would say: {message}"
    );
    assert!(
        message.contains(&first),
        "the failure does not name what only the request carries: {message}"
    );
}

/// A reconciled surface is rendered from that same read, rather than querying its tools again.
///
/// A mutable integration may change between calls to `model_definition`. Reading it again after
/// reconciliation would make the comparison pass for one name and render another into the prefix.
#[test]
fn test_a_reconciled_surface_renders_the_verified_name_snapshot() {
    let reads = Arc::new(AtomicUsize::new(0));
    let mut tools: Vec<Arc<dyn Tool>> = selected_names(CodingProfile::Core)
        .iter()
        .skip(1)
        .map(|name| StubTool::shared(name))
        .collect();
    let first = selected_names(CodingProfile::Core)
        .first()
        .expect("core names tools")
        .clone();
    tools.push(ChangesOnSecondReadTool::shared(&first, Arc::clone(&reads)));

    let registry = ToolRegistry::builder()
        .register_all(tools)
        .build()
        .expect("a valid registry");
    let surface = registry
        .assemble(
            &CodingProfile::Core
                .to_tool_profile()
                .expect("a valid profile"),
        )
        .expect("the core tier assembles");

    // Assembly consumed the initial name. Begin the prompt path at the one reconciled read that
    // must also supply its rendered inventory.
    reads.store(0, Ordering::SeqCst);
    let prefix = assemble_stable_prefix_for_surface(&PromptRole::Main, &surface)
        .expect("the reconciled snapshot remains renderable");

    assert_eq!(
        inventoried_names(prefix.system_instructions()),
        surface
            .advertised_names()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

/// A role that installs fewer capabilities narrows its tier instead of failing the tier's floor.
///
/// The floor is there to catch a surface that lost an entry, and a read-only agent is short three
/// on purpose. Both facts have to survive: the profile discounts exactly what the role withheld,
/// and it says so in its own identity rather than reporting itself as the tier it is not.
#[test]
fn test_a_read_only_role_narrows_its_tier_rather_than_failing_the_floor() {
    let workspace = TempDir::new().expect("a workspace");
    let host = CodingHost::open(workspace.path()).expect("the host opens a workspace");

    for role in [PromptRole::ReadOnlySpecialist, PromptRole::Planner] {
        let surface = host_backed_tool_surface(&role, &host, CodingProfile::Core)
            .expect("a read-only role assembles the tier it can hold");

        assert_eq!(
            surface.advertised_names().collect::<Vec<_>>(),
            ["glob", "grep", "read_file"],
            "role `{role}`"
        );
        assert_eq!(
            surface.profile().as_str(),
            format!("core-{}", role.role_name()),
            "a narrowed tier must not report itself as the tier"
        );
    }

    // The unnarrowed tier is still the tier: a role that installs everything is judged by the
    // measured band, under the name the product declared it with.
    let main = host_backed_tool_surface(&PromptRole::Main, &host, CodingProfile::Core)
        .expect("the main role assembles the complete tier");
    assert_eq!(main.profile().as_str(), "core");
    assert_eq!(main.advertised_count(), 6);
}

/// A role that answers without tools assembles an empty surface rather than failing.
#[test]
fn test_a_one_off_role_assembles_an_empty_surface() {
    let workspace = TempDir::new().expect("a workspace");
    let host = CodingHost::open(workspace.path()).expect("the host opens a workspace");

    let surface = host_backed_tool_surface(&PromptRole::OneOffAnswer, &host, CodingProfile::Core)
        .expect("a one-off role assembles nothing at all");

    assert!(surface.is_empty());
    assert_eq!(surface.advertised_count(), 0);
}

/// The agent declares exactly the entries its own prompt inventories.
///
/// Both sides are read off the built agent rather than recomputed, because the thing that can go
/// wrong is precisely a construction path that assembles one list for the provider and another for
/// the text.
#[test]
fn test_the_agent_declares_exactly_what_its_prompt_inventories() {
    let workspace = TempDir::new().expect("a workspace");
    let host = CodingHost::open(workspace.path()).expect("the host opens a workspace");

    for role in [
        PromptRole::Main,
        PromptRole::Coordinator,
        PromptRole::ReadOnlySpecialist,
    ] {
        let agent = build_agent_with_profile(
            AgentId::new("coding-agent"),
            "Coding Agent",
            &role,
            &host,
            CodingProfile::Core,
        )
        .expect("the core tier builds an agent for every role that holds it");

        let declared: Vec<String> = agent
            .tools()
            .iter()
            .map(|tool| tool.model_definition().name().to_owned())
            .collect();
        let instructions = agent
            .instructions()
            .and_then(AgentInstructions::as_static)
            .expect("the agent carries a stable prefix");

        assert_eq!(
            inventoried_names(instructions),
            declared,
            "role `{role}` inventories a different surface than it declares"
        );
    }
}

/// A tier the installed capabilities cannot satisfy fails before an agent exists.
///
/// The default tier names nine entries nobody has written, and this is what that costs: no agent,
/// rather than one whose prompt describes a surface the provider was never sent.
#[test]
fn test_a_tier_the_host_cannot_satisfy_refuses_to_build_an_agent() {
    let workspace = TempDir::new().expect("a workspace");
    let host = CodingHost::open(workspace.path()).expect("the host opens a workspace");

    let error = build_agent_with_profile(
        AgentId::new("coding-agent"),
        "Coding Agent",
        &PromptRole::Main,
        &host,
        CodingProfile::default(),
    )
    .expect_err("the default tier must not build while its entries are unwritten");

    let message = error.to_string();
    assert!(message.contains("codex_like"), "{message}");
}

/// The surface an assembled tier produces is the one the prompt path accepts unchanged.
///
/// Guards the seam from the other direction: `ToolSurface` is the only shape this prefix builder
/// takes, so a tool that reaches an agent without passing a budget has nowhere to enter.
#[test]
fn test_the_assembled_surface_feeds_the_prefix_and_the_agent_from_one_object() {
    let workspace = TempDir::new().expect("a workspace");
    let host = CodingHost::open(workspace.path()).expect("the host opens a workspace");
    let surface: ToolSurface =
        host_backed_tool_surface(&PromptRole::Main, &host, CodingProfile::Core)
            .expect("the main role assembles the complete tier");

    let prefix = assemble_stable_prefix_for_surface(&PromptRole::Main, &surface)
        .expect("the prefix assembles for an assembled surface");
    let inventoried = inventoried_names(prefix.system_instructions());
    let tools: Vec<String> = surface
        .into_tools()
        .iter()
        .map(|tool| tool.model_definition().name().to_owned())
        .collect();

    assert_eq!(inventoried, tools);
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
        6,
        "a core tool was written without being added to `implemented_core_tools`, or one was \
         removed"
    );
}
