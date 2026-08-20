//! Contracts for the tool registry and the profiles that assemble a surface out of it.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    error::Result,
    tool::{
        Tool, ToolAvailability, ToolContext, ToolExposure, ToolLookupKey, ToolNamespace,
        ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_runtime::tool::{
    profile::{ToolProfile, ToolProfileId, ToolSelection, ToolSurfaceBudget},
    registry::ToolRegistry,
};
use serde_json::json;

struct StubTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
}

impl StubTool {
    fn bare(name: &str) -> Self {
        Self::from_origin(ToolOrigin::new(name).expect("a valid tool name"), name)
    }

    fn namespaced(namespace: &str, name: &str) -> Self {
        let namespace = ToolNamespace::new(namespace).expect("a valid namespace");
        Self::from_origin(
            ToolOrigin::namespaced(namespace, name).expect("a valid namespaced identity"),
            name,
        )
    }

    fn from_origin(origin: ToolOrigin, name: &str) -> Self {
        let schema = ToolSchema::new(
            name,
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .expect("a strict schema");
        Self {
            origin,
            schema,
            options: ToolOptions::new(),
        }
    }

    fn with_description(mut self, description: &str) -> Self {
        self.schema = self.schema.with_description(description);
        self
    }

    fn with_options(mut self, options: ToolOptions) -> Self {
        self.options = options;
        self
    }

    fn shared(self) -> Arc<dyn Tool> {
        Arc::new(self)
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
        self.options.clone()
    }
}

fn key(name: &str) -> ToolLookupKey {
    ToolLookupKey::bare(name).expect("a valid tool name")
}

fn profile_id(name: &str) -> ToolProfileId {
    ToolProfileId::new(name).expect("a valid profile name")
}

/// A profile that takes exactly these names and tolerates any count.
fn selecting(id: &str, names: &[&str]) -> ToolProfile {
    ToolProfile::builder(profile_id(id))
        .include_all(names.iter().copied().map(key))
        .budget(ToolSurfaceBudget::new(0, usize::MAX).expect("a valid budget"))
        .build()
        .expect("a valid profile")
}

#[test]
fn test_explicit_selection_takes_only_the_named_tools() {
    let registry = ToolRegistry::builder()
        .register(StubTool::bare("read_file").shared())
        .register(StubTool::bare("grep").shared())
        .register(StubTool::bare("glob").shared())
        .build()
        .expect("a valid registry");

    let surface = registry
        .assemble(&selecting("reader", &["read_file", "grep"]))
        .expect("a selectable surface");

    assert_eq!(
        surface.advertised_names().collect::<Vec<_>>(),
        ["grep", "read_file"]
    );
    // Advertising is not owning: the third tool stays registered and dispatchable, it is just not
    // on this agent's surface.
    assert_eq!(registry.len(), 3);
    assert!(registry.contains(&key("glob")));
}

#[test]
fn test_selecting_a_tool_no_one_registered_names_it() {
    let registry = ToolRegistry::builder()
        .register(StubTool::bare("read_file").shared())
        .build()
        .expect("a valid registry");

    // The whole point of naming planned tools in a profile: the surface refuses to assemble
    // rather than coming out one entry smaller than the prompt describes.
    let error = registry
        .assemble(&selecting("coding", &["read_file", "apply_patch"]))
        .expect_err("an unregistered selection must fail");

    let message = error.to_string();
    assert!(message.contains("apply_patch"), "{message}");
    assert!(message.contains("coding"), "{message}");
}

#[test]
fn test_all_registered_takes_whatever_the_host_installed() {
    let registry = ToolRegistry::builder()
        .register(StubTool::bare("read_file").shared())
        .register(StubTool::namespaced("mcp.github", "search").shared())
        .build()
        .expect("a valid registry");

    let profile = ToolProfile::builder(profile_id("full"))
        .all_registered()
        .budget(ToolSurfaceBudget::new(1, 24).expect("a valid budget"))
        .build()
        .expect("a valid profile");

    let surface = registry.assemble(&profile).expect("a full surface");

    // A profile that could only enumerate keys could never include a tool an MCP server exported
    // after the profile was written.
    assert_eq!(surface.advertised_count(), 2);
    assert_eq!(
        surface.advertised_names().collect::<Vec<_>>(),
        ["read_file", "search"]
    );
}

#[test]
fn test_two_servers_may_export_the_same_name_but_one_surface_may_not() {
    let registry = ToolRegistry::builder()
        .register(StubTool::namespaced("mcp.github", "search").shared())
        .register(StubTool::namespaced("mcp.jira", "search").shared())
        .build()
        .expect("two servers exporting `search` is a normal installation");

    // The lookup keys differ, so the registry routes them apart...
    assert_eq!(registry.len(), 2);

    // ...but both project to `search` at the model boundary, so a surface holding both is a
    // request every provider rejects. It fails here, where the error can name both keys.
    let profile = ToolProfile::builder(profile_id("full"))
        .all_registered()
        .budget(ToolSurfaceBudget::new(0, 24).expect("a valid budget"))
        .build()
        .expect("a valid profile");
    let error = registry
        .assemble(&profile)
        .expect_err("two entries named `search` must fail");

    let message = error.to_string();
    assert!(message.contains("mcp.github"), "{message}");
    assert!(message.contains("mcp.jira"), "{message}");
    assert!(message.contains("search"), "{message}");
}

#[test]
fn test_a_host_only_tool_may_share_a_name_with_an_advertised_one() {
    // A name is only ever ambiguous inside one tool list, and a hidden tool never appears in one.
    // Rejecting this would refuse an ordinary installation — a host-only `search` beside an
    // integration's `search` — over a conflict no provider would ever see.
    let hidden = ToolOptions::new().with_exposure(ToolExposure::Hidden);
    let registry = ToolRegistry::builder()
        .register(StubTool::namespaced("mcp.github", "search").shared())
        .register(
            StubTool::namespaced("host", "search")
                .with_options(hidden)
                .shared(),
        )
        .build()
        .expect("a valid registry");

    let profile = ToolProfile::builder(profile_id("full"))
        .all_registered()
        .budget(ToolSurfaceBudget::new(1, 1).expect("a valid budget"))
        .build()
        .expect("a valid profile");
    let surface = registry
        .assemble(&profile)
        .expect("only one `search` is offered");

    assert_eq!(surface.len(), 2);
    assert_eq!(surface.advertised_names().collect::<Vec<_>>(), ["search"]);
}

#[test]
fn test_a_deferred_tool_still_claims_its_name() {
    // Deferred is a promise that discovery can promote the tool into a later turn's advertised
    // set. It costs nothing today and so is not in `advertised_count`, but it stakes a claim on
    // its name today — otherwise the collision would appear on the turn it is promoted, which is
    // the one moment nothing is checking.
    let deferred = ToolOptions::new().with_exposure(ToolExposure::Deferred);
    let registry = ToolRegistry::builder()
        .register(StubTool::namespaced("mcp.github", "search").shared())
        .register(
            StubTool::namespaced("mcp.jira", "search")
                .with_options(deferred)
                .shared(),
        )
        .build()
        .expect("a valid registry");

    let profile = ToolProfile::builder(profile_id("full"))
        .all_registered()
        .budget(ToolSurfaceBudget::new(0, 24).expect("a valid budget"))
        .build()
        .expect("a valid profile");
    let error = registry
        .assemble(&profile)
        .expect_err("a promotable name must not collide");

    assert!(error.to_string().contains("search"), "{error}");
}

#[test]
fn test_a_switched_off_tool_claims_nothing() {
    // `Disabled` is static, so the tool cannot become advertised without a configuration change.
    // It holds no name and pays no budget.
    let disabled = ToolOptions::new().with_availability(ToolAvailability::Disabled);
    let registry = ToolRegistry::builder()
        .register(StubTool::namespaced("mcp.github", "search").shared())
        .register(
            StubTool::namespaced("mcp.jira", "search")
                .with_options(disabled)
                .shared(),
        )
        .build()
        .expect("a valid registry");

    let profile = ToolProfile::builder(profile_id("full"))
        .all_registered()
        .budget(ToolSurfaceBudget::new(1, 1).expect("a valid budget"))
        .build()
        .expect("a valid profile");
    let surface = registry
        .assemble(&profile)
        .expect("only one `search` is live");

    assert_eq!(surface.advertised_count(), 1);
}

#[test]
fn test_one_lookup_key_may_be_registered_once() {
    let error = ToolRegistry::builder()
        .register(StubTool::bare("read_file").shared())
        .register(StubTool::bare("read_file").shared())
        .build()
        .expect_err("a duplicate lookup key must fail");

    assert!(error.to_string().contains("read_file"), "{error}");
}

#[test]
fn test_registration_validates_the_tool() {
    // Origin and schema names disagreeing is the kind of mistake that otherwise surfaces as a
    // model calling a name nothing routes.
    let mut tool = StubTool::bare("read_file");
    tool.schema = ToolSchema::new(
        "read_files",
        json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
    )
    .expect("a strict schema");

    let error = ToolRegistry::builder()
        .register(tool.shared())
        .build()
        .expect_err("an inconsistent tool must fail registration");

    assert!(error.to_string().contains("read_file"), "{error}");
}

#[test]
fn test_only_advertised_entries_are_counted_and_paid_for() {
    let hidden = ToolOptions::new().with_exposure(ToolExposure::Hidden);
    let deferred = ToolOptions::new().with_exposure(ToolExposure::Deferred);
    let disabled = ToolOptions::new().with_availability(ToolAvailability::Disabled);
    let registry = ToolRegistry::builder()
        .register(StubTool::bare("read_file").shared())
        .register(StubTool::bare("host_probe").with_options(hidden).shared())
        .register(
            StubTool::bare("notebook_edit")
                .with_options(deferred)
                .shared(),
        )
        .register(StubTool::bare("retired").with_options(disabled).shared())
        .build()
        .expect("a valid registry");

    let profile = ToolProfile::builder(profile_id("narrow"))
        .all_registered()
        // One advertised entry allowed, and four tools in the registry. This passes only if
        // withholding a tool from the model actually costs nothing.
        .budget(ToolSurfaceBudget::new(1, 1).expect("a valid budget"))
        .build()
        .expect("a valid profile");

    let surface = registry.assemble(&profile).expect("a bounded surface");

    assert_eq!(surface.len(), 4);
    assert_eq!(surface.advertised_count(), 1);
    assert_eq!(
        surface.advertised_names().collect::<Vec<_>>(),
        ["read_file"]
    );
}

#[test]
fn test_dynamic_availability_counts_as_advertised() {
    // The budget describes the surface the profile declared, not the one a particular turn
    // happened to send. A tool that may be on has to be paid for as though it is.
    let dynamic = ToolOptions::new().with_availability(ToolAvailability::Dynamic);
    let registry = ToolRegistry::builder()
        .register(StubTool::bare("read_file").shared())
        .register(StubTool::bare("web_search").with_options(dynamic).shared())
        .build()
        .expect("a valid registry");

    let profile = ToolProfile::builder(profile_id("narrow"))
        .all_registered()
        .budget(ToolSurfaceBudget::new(0, 1).expect("a valid budget"))
        .build()
        .expect("a valid profile");

    let error = registry
        .assemble(&profile)
        .expect_err("a dynamic entry still occupies a slot");

    assert!(error.to_string().contains("2 entries"), "{error}");
}

#[test]
fn test_a_surface_below_its_floor_fails() {
    let registry = ToolRegistry::builder()
        .register(StubTool::bare("read_file").shared())
        .build()
        .expect("a valid registry");

    let profile = ToolProfile::builder(profile_id("core"))
        .all_registered()
        .budget(ToolSurfaceBudget::new(6, 8).expect("a valid budget"))
        .build()
        .expect("a valid profile");

    let error = registry
        .assemble(&profile)
        .expect_err("a surface that lost its tools must fail");

    let message = error.to_string();
    assert!(message.contains("floor"), "{message}");
    assert!(message.contains("core"), "{message}");
}

#[test]
fn test_a_surface_above_its_ceiling_fails() {
    let mut builder = ToolRegistry::builder();
    for index in 0..25 {
        builder = builder.register(StubTool::bare(&format!("tool_{index}")).shared());
    }
    let registry = builder.build().expect("a valid registry");

    let profile = ToolProfile::builder(profile_id("full"))
        .all_registered()
        .budget(ToolSurfaceBudget::new(14, 24).expect("a valid budget"))
        .build()
        .expect("a valid profile");

    let error = registry
        .assemble(&profile)
        .expect_err("a 25-entry surface must fail a 24-entry ceiling");

    let message = error.to_string();
    assert!(message.contains("25 entries"), "{message}");
    assert!(message.contains("ceiling of 24"), "{message}");
}

#[test]
fn test_the_byte_ceiling_is_separate_from_the_entry_count() {
    let long = "d".repeat(4_000);
    let registry = ToolRegistry::builder()
        .register(StubTool::bare("read_file").with_description(&long).shared())
        .register(StubTool::bare("grep").with_description(&long).shared())
        .build()
        .expect("a valid registry");

    let within_count = ToolSurfaceBudget::new(2, 2).expect("a valid budget");
    let profile = ToolProfile::builder(profile_id("core"))
        .all_registered()
        .budget(within_count.with_max_advertised_bytes(4_096))
        .build()
        .expect("a valid profile");

    // Two entries out of two allowed, and still refused: descriptions grow without the entry
    // count moving, and the per-turn bill follows the bytes.
    let error = registry
        .assemble(&profile)
        .expect_err("an oversized surface must fail its byte ceiling");
    assert!(
        error.to_string().contains("bytes of tool schema"),
        "{error}"
    );

    let generous = ToolProfile::builder(profile_id("core"))
        .all_registered()
        .budget(within_count.with_max_advertised_bytes(20 * 1024))
        .build()
        .expect("a valid profile");
    let surface = registry
        .assemble(&generous)
        .expect("a surface within budget");

    // The reported cost is the sum of the entries' own measure, not a second definition of it.
    let expected: usize = registry
        .tools()
        .map(|tool| {
            tool.schema()
                .advertised_bytes()
                .expect("a renderable schema")
        })
        .sum();
    assert_eq!(surface.advertised_bytes(), expected);
}

#[test]
fn test_registration_order_does_not_change_the_surface() {
    let forwards = ToolRegistry::builder()
        .register(StubTool::bare("read_file").shared())
        .register(StubTool::bare("grep").shared())
        .register(StubTool::bare("glob").shared())
        .build()
        .expect("a valid registry");
    let backwards = ToolRegistry::builder()
        .register(StubTool::bare("glob").shared())
        .register(StubTool::bare("grep").shared())
        .register(StubTool::bare("read_file").shared())
        .build()
        .expect("a valid registry");

    let profile = selecting("core", &["read_file", "grep", "glob"]);
    let first = forwards.assemble(&profile).expect("a surface");
    let second = backwards.assemble(&profile).expect("a surface");

    // A cached prefix is built over the advertised table. Two hosts that install the same tools
    // in different startup order have to produce the same bytes, or the cache never hits.
    assert_eq!(
        first.advertised_names().collect::<Vec<_>>(),
        second.advertised_names().collect::<Vec<_>>()
    );
    assert_eq!(first.advertised_bytes(), second.advertised_bytes());
    assert_eq!(
        first.advertised_names().collect::<Vec<_>>(),
        ["glob", "grep", "read_file"]
    );
}

#[test]
fn test_a_profile_must_declare_a_budget() {
    let error = ToolProfile::builder(profile_id("core"))
        .include(key("read_file"))
        .build()
        .expect_err("a profile with no ceiling must fail");

    assert!(error.to_string().contains("budget"), "{error}");
}

#[test]
fn test_a_profile_cannot_both_take_everything_and_name_things() {
    let error = ToolProfile::builder(profile_id("full"))
        .all_registered()
        .include(key("read_file"))
        .budget(ToolSurfaceBudget::new(0, 24).expect("a valid budget"))
        .build()
        .expect_err("a contradictory selection must fail");

    assert!(error.to_string().contains("one or the other"), "{error}");
}

#[test]
fn test_a_profile_names_each_tool_once() {
    let error = ToolProfile::builder(profile_id("core"))
        .include(key("read_file"))
        .include(key("read_file"))
        .budget(ToolSurfaceBudget::new(0, 8).expect("a valid budget"))
        .build()
        .expect_err("a repeated selection must fail");

    assert!(error.to_string().contains("more than once"), "{error}");
}

#[test]
fn test_a_budget_floor_cannot_exceed_its_ceiling() {
    let error = ToolSurfaceBudget::new(16, 14).expect_err("an inverted budget must fail");

    assert!(error.to_string().contains("cannot exceed"), "{error}");
}

#[test]
fn test_an_assembled_surface_is_what_an_agent_declares() {
    let registry = ToolRegistry::builder()
        .register(StubTool::bare("read_file").shared())
        .register(StubTool::bare("grep").shared())
        .register(StubTool::bare("host_probe").shared())
        .build()
        .expect("a valid registry");

    let surface = registry
        .assemble(&selecting("core", &["read_file", "grep"]))
        .expect("a surface");
    assert_eq!(surface.profile().as_str(), "core");

    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .instructions("Use the assembled surface.")
        .tools(surface.into_tools())
        .build()
        .expect("an assembled surface is valid agent input");

    assert_eq!(agent.tools().len(), 2);
}

#[test]
fn test_a_profile_reports_the_selection_it_was_built_from() {
    let profile = selecting("core", &["read_file", "grep"]);

    match profile.selection() {
        ToolSelection::Explicit(keys) => {
            assert!(keys.contains(&key("read_file")));
            assert!(keys.contains(&key("grep")));
            assert_eq!(keys.len(), 2);
        }
        other => panic!("expected an explicit selection, got {other:?}"),
    }
}
