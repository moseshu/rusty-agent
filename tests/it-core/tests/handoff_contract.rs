//! Typed handoff declarations, adapted from the upstream handoff tool tests.

use std::sync::{Arc, Mutex};

use ra_core::{
    agent::{AgentId, AgentSpec, HandoffSpec, HistoryProjection},
    context::RunContext,
    state::RunId,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
struct TransferInput {
    reason: String,
    priority: u32,
}

fn target() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("billing"))
        .name("Billing Agent")
        .handoff_description("Handles refunds.")
        .build()
        .unwrap()
}

#[test]
fn defaults_overrides_and_agent_cloning_preserve_the_handoff_contract() {
    let target = target();
    let handoff = target.as_handoff().unwrap();
    assert_eq!(handoff.schema().name(), "transfer_to_billing_agent");
    assert_eq!(
        handoff.schema().description(),
        Some("Handoff to the Billing Agent agent to handle the request. Handles refunds.")
    );
    assert_eq!(handoff.history_projection(), &HistoryProjection::Full);
    assert_eq!(HistoryProjection::default(), HistoryProjection::Full);
    assert!(handoff.schema().strict_json_schema());
    assert_eq!(
        handoff.schema().input_schema()["additionalProperties"],
        false
    );
    assert_eq!(
        target.to_builder().build().unwrap().handoff_description(),
        Some("Handles refunds.")
    );
    let overridden = handoff
        .with_tool_name("route_billing")
        .unwrap()
        .with_tool_description("Billing");
    assert_eq!(overridden.schema().name(), "route_billing");
    assert_eq!(overridden.schema().description(), Some("Billing"));
    assert_eq!(overridden.target_agent(), target.id());
}

#[tokio::test]
async fn typed_arguments_are_strictly_validated_before_the_callback() {
    let target = target();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let handoff = target
        .as_handoff()
        .unwrap()
        .with_input_type(move |context: &RunContext, input: TransferInput| {
            assert_eq!(context.run_id().as_str(), "typed-handoff");
            recorded.lock().unwrap().push(input);
            std::future::ready(Ok(()))
        })
        .unwrap();
    assert_eq!(
        handoff.schema().description(),
        target.as_handoff().unwrap().schema().description()
    );
    assert_eq!(
        handoff.schema().input_schema()["additionalProperties"],
        false
    );
    let context = RunContext::new(RunId::new("typed-handoff"), &target);
    for invalid in [
        json!(null),
        json!({}),
        json!({"reason": "refund", "priority": "1"}),
        json!({"reason": "refund", "priority": 1, "unexpected": true}),
    ] {
        let error = handoff.invoke(&context, &invalid).await.unwrap_err();
        assert_eq!(error.code(), "provider.behavior");
        assert!(seen.lock().unwrap().is_empty());
    }
    handoff
        .invoke(&context, &json!({"reason": "refund", "priority": 1}))
        .await
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [TransferInput {
            reason: "refund".to_owned(),
            priority: 1
        }]
    );
}

#[tokio::test]
async fn untyped_callbacks_ignore_arguments_and_static_enablement_is_respected() {
    let target = target();
    let calls = Arc::new(Mutex::new(0));
    let recorded = Arc::clone(&calls);
    let handoff = target.as_handoff().unwrap().with_on_handoff_fn(move |_| {
        *recorded.lock().unwrap() += 1;
        std::future::ready(Ok(()))
    });
    let context = RunContext::new(RunId::new("untyped"), &target);
    handoff.invoke(&context, &json!(null)).await.unwrap();
    assert_eq!(*calls.lock().unwrap(), 1);
    assert!(
        !handoff
            .with_is_enabled(false)
            .is_enabled(&context)
            .await
            .unwrap()
    );
}

#[test]
fn nullable_input_root_is_rejected_when_declaring_the_handoff() {
    let error = target()
        .as_handoff()
        .unwrap()
        .with_input_type(|_: &RunContext, _: Option<TransferInput>| std::future::ready(Ok(())))
        .unwrap_err();
    assert_eq!(error.code(), "config");
    assert!(error.to_string().contains("root"));
}

#[test]
fn colliding_derived_names_are_deferred_to_runtime_enablement_and_collision_policy() {
    let first = target();
    let second = AgentSpec::builder()
        .id(AgentId::new("other"))
        .name("billing agent")
        .build()
        .unwrap();
    let source = AgentSpec::builder()
        .id(AgentId::new("triage"))
        .name("Triage")
        .handoffs([
            HandoffSpec::from_agent(&first).unwrap(),
            second.as_handoff().unwrap(),
        ])
        .build()
        .unwrap();
    assert_eq!(source.handoffs().len(), 2);
}

#[tokio::test]
async fn strict_model_schema_does_not_override_type_defaults_or_unknown_field_policy() {
    #[derive(Deserialize, JsonSchema)]
    struct DefaultInput {
        #[serde(default)]
        reason: String,
    }
    let target = target();
    let handoff = target
        .as_handoff()
        .unwrap()
        .with_input_type(|_: &RunContext, input: DefaultInput| {
            assert_eq!(input.reason, "");
            std::future::ready(Ok(()))
        })
        .unwrap();
    assert_eq!(
        handoff.schema().input_schema()["required"],
        json!(["reason"])
    );
    handoff
        .invoke(
            &RunContext::new(RunId::new("defaults"), &target),
            &json!({"extra": true}),
        )
        .await
        .unwrap();
}
