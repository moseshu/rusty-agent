//! Contracts for agent declarations and handoff specifications.

use ra_core::{
    agent::{AgentDefinition, AgentId, AgentSpec, HandoffSpec, HistoryProjection},
    tool::ToolSchema,
};
use serde_json::json;

fn schema(name: &str) -> ToolSchema {
    ToolSchema::new(
        name,
        json!({
            "type": "object",
            "properties": {
                "reason": { "type": "string" }
            },
            "required": ["reason"],
            "additionalProperties": false
        }),
    )
    .unwrap()
}

#[test]
fn handoff_spec_projects_a_strict_model_definition_without_history_by_default() {
    let handoff = HandoffSpec::new(
        AgentId::new("reviewer"),
        schema("delegate_review").with_description("Ask a reviewer to inspect the change."),
    );

    assert_eq!(handoff.history_projection(), &HistoryProjection::None);
    let definition = handoff.model_definition();
    assert_eq!(definition.target_agent().as_str(), "reviewer");
    assert_eq!(definition.name(), "delegate_review");
    assert_eq!(
        definition.description(),
        Some("Ask a reviewer to inspect the change.")
    );
    assert!(definition.strict());
}

#[test]
fn agent_definition_alias_has_one_declaration_shape_and_validates_handoffs() {
    let agent: std::sync::Arc<AgentDefinition> = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .handoff(
            HandoffSpec::new(AgentId::new("reviewer"), schema("delegate_review"))
                .with_history_projection(HistoryProjection::LastItems(3)),
        )
        .build()
        .unwrap();

    assert_eq!(agent.handoffs().len(), 1);
    assert_eq!(
        agent.handoffs()[0].history_projection(),
        &HistoryProjection::LastItems(3)
    );
}

#[test]
fn agent_definition_rejects_self_transfers_duplicate_names_and_empty_history_windows() {
    let self_transfer = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .handoff(HandoffSpec::new(
            AgentId::new("planner"),
            schema("delegate_planner"),
        ))
        .build()
        .unwrap_err();
    assert!(self_transfer
        .to_string()
        .contains("cannot hand off control to itself"));

    let duplicate_name = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .handoffs([
            HandoffSpec::new(AgentId::new("reviewer"), schema("delegate")),
            HandoffSpec::new(AgentId::new("writer"), schema("delegate")),
        ])
        .build()
        .unwrap_err();
    assert!(duplicate_name
        .to_string()
        .contains("advertises the model-facing action name `delegate` more than once"));

    let empty_window = AgentSpec::builder()
        .id(AgentId::new("planner"))
        .name("Planner")
        .handoff(
            HandoffSpec::new(AgentId::new("reviewer"), schema("delegate_review"))
                .with_history_projection(HistoryProjection::LastItems(0)),
        )
        .build()
        .unwrap_err();
    assert!(empty_window
        .to_string()
        .contains("must retain at least one item"));
}
