//! Contracts for immutable agent declaration registration.

use std::sync::Arc;

use ra_core::{
    agent::{AgentId, AgentSpec, HandoffSpec},
    tool::ToolSchema,
};
use ra_runtime::agent::AgentRegistry;
use serde_json::json;

fn schema(name: &str) -> ToolSchema {
    ToolSchema::new(
        name,
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    )
    .unwrap()
}

fn definition(id: &str) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name(id)
        .build()
        .unwrap()
}

#[test]
fn registry_resolves_registered_handoff_targets_in_stable_identity_order() {
    let reviewer = definition("reviewer");
    let planner = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("planner")
        .handoff(HandoffSpec::new(
            AgentId::new("reviewer"),
            schema("delegate_review"),
        ))
        .build()
        .unwrap();

    let registry = AgentRegistry::builder()
        .register(Arc::clone(&reviewer))
        .register(Arc::clone(&planner))
        .build()
        .unwrap();

    assert_eq!(registry.len(), 2);
    assert!(registry.contains(&AgentId::new("planner")));
    assert!(Arc::ptr_eq(
        registry.get(&AgentId::new("reviewer")).unwrap(),
        &reviewer
    ));
    assert_eq!(
        registry
            .definitions()
            .map(|agent| agent.id().as_str())
            .collect::<Vec<_>>(),
        ["planner", "reviewer"]
    );
}

#[test]
fn registry_rejects_duplicate_id_and_unresolved_handoff_target() {
    let duplicate = AgentRegistry::builder()
        .register(definition("planner"))
        .register(definition("planner"))
        .build()
        .unwrap_err();
    assert!(duplicate.to_string().contains("registered more than once"));

    let unresolved = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("planner")
        .handoff(HandoffSpec::new(
            AgentId::new("reviewer"),
            schema("delegate_review"),
        ))
        .build()
        .unwrap();
    let error = AgentRegistry::builder()
        .register(unresolved)
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("unregistered agent `reviewer`"));
}
