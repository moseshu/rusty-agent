//! Agent-owned sticky decisions across transfers and serialized recovery.

use super::*;
use ra_core::tool::{Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput};

struct OwnedProbe {
    origin: ToolOrigin,
    schema: ToolSchema,
    calls: Arc<AtomicUsize>,
    policies: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for OwnedProbe {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ToolApprovalPolicy::Dynamic)
    }
    async fn needs_approval(&self, _: &ToolContext<'_>) -> Result<bool> {
        self.policies.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("owned work"))
    }
}

fn schema(name: &str) -> ToolSchema {
    ToolSchema::new(
        name,
        json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
    )
    .unwrap()
}

fn call(id: &str, name: &str) -> ModelResponse {
    ModelResponse::new(vec![item(
        id,
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), name, json!({}))),
    )])
}

fn owned_agent(
    id: &str,
    target: &str,
    calls: &Arc<AtomicUsize>,
    policies: &Arc<AtomicUsize>,
) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name("same-name")
        .tool(Arc::new(OwnedProbe {
            origin: ToolOrigin::new("operate").unwrap(),
            schema: schema("operate"),
            calls: calls.clone(),
            policies: policies.clone(),
        }))
        .handoff(HandoffSpec::new(
            AgentId::new(target),
            schema(&format!("to_{target}")),
        ))
        .build()
        .unwrap()
}

fn roundtrip(state: &RunState, serialized: bool) -> RunState {
    if serialized {
        serde_json::from_str(&serde_json::to_string(state).unwrap()).unwrap()
    } else {
        state.clone()
    }
}

#[tokio::test]
async fn sticky_decisions_stay_with_agents_across_return_handoffs() {
    for streamed in [false, true] {
        for serialized in [false, true] {
            for approved in [false, true] {
                let first_calls = Arc::new(AtomicUsize::new(0));
                let second_calls = Arc::new(AtomicUsize::new(0));
                let policies = Arc::new(AtomicUsize::new(0));
                let first = owned_agent("first", "second", &first_calls, &policies);
                let second = owned_agent("second", "first", &second_calls, &policies);
                let model = ScriptedModel::new(vec![
                    call("first-1", "operate"),
                    call("transfer-1", "to_second"),
                    call("second-1", "operate"),
                    call("second-2", "operate"),
                    call("transfer-2", "to_first"),
                    call("first-2", "operate"),
                    ModelResponse::new(vec![message("done", "done")]),
                ]);
                let request = |state: Option<RunState>| {
                    let current =
                        if state.as_ref().and_then(RunState::current_agent) == Some(second.id()) {
                            second.clone()
                        } else {
                            first.clone()
                        };
                    let request = if state.is_none() {
                        run_request(first.clone(), second.clone(), &model, RunConfig::new())
                    } else {
                        RunRequest::new(
                            AgentBinding::direct(current),
                            Arc::new(FixedResolver {
                                model: model.clone(),
                            }),
                            RunId::new("run-handoff"),
                            CancelScope::root(),
                            Vec::new(),
                        )
                        .with_config(
                            RunConfig::new().with_agent_registry(
                                AgentRegistry::builder()
                                    .register(first.clone())
                                    .register(second.clone())
                                    .build()
                                    .unwrap(),
                            ),
                        )
                    };
                    match state {
                        Some(state) => request.with_state(state),
                        None => request,
                    }
                };
                let initial = drive(request(None), streamed).await.unwrap();
                let approval = initial.outcome().interruptions()[0].clone();
                let mut state = initial.state().clone();
                if approved {
                    state.approve(&approval, true).unwrap();
                } else {
                    state
                        .reject_with_message(&approval, true, "first denied")
                        .unwrap();
                }
                let next = drive(request(Some(roundtrip(&state, serialized))), streamed)
                    .await
                    .unwrap();
                assert_eq!(next.outcome().interruptions().len(), 1);
                let approval = &next.outcome().interruptions()[0];
                assert_eq!(
                    approval.provenance().unwrap().agent_id(),
                    &AgentId::new("second")
                );
                assert_eq!(policies.load(Ordering::SeqCst), 2);
                let mut state = next.state().clone();
                if approved {
                    state
                        .reject_with_message(approval, true, "second denied")
                        .unwrap();
                } else {
                    state.approve(approval, true).unwrap();
                }
                let result = drive(request(Some(roundtrip(&state, serialized))), streamed)
                    .await
                    .unwrap();
                assert_eq!(result.final_text(), "done");
                assert_eq!(policies.load(Ordering::SeqCst), 2);
                assert_eq!(
                    first_calls.load(Ordering::SeqCst),
                    if approved { 2 } else { 0 }
                );
                assert_eq!(
                    second_calls.load(Ordering::SeqCst),
                    if approved { 0 } else { 2 }
                );
                let denied = if approved {
                    "second denied"
                } else {
                    "first denied"
                };
                let outputs = serde_json::to_string(result.state().generated_items()).unwrap();
                assert_eq!(outputs.matches(denied).count(), 2);
            }
        }
    }
}

/// The reference's `test_legacy_sticky_decisions_require_approval_after_restore`: a sticky
/// decision restored from a checkpoint that could not name its owner must not decide a later
/// call. The pending call is answered exactly, so only the restored rule could decide the next
/// one; an owned rule restored the same way does decide it, which keeps the comparison honest.
#[tokio::test]
async fn legacy_sticky_decisions_reprompt_after_checkpoint_restore() {
    for approved in [false, true] {
        for legacy in [true, false] {
            let calls = Arc::new(AtomicUsize::new(0));
            let policies = Arc::new(AtomicUsize::new(0));
            let first = owned_agent("first", "second", &calls, &policies);
            let second = owned_agent("second", "first", &calls, &policies);
            let model = ScriptedModel::new(vec![
                call("pending", "operate"),
                call("later", "operate"),
                ModelResponse::new(vec![message("done", "done")]),
            ]);
            let initial = Runner::run(run_request(
                first.clone(),
                second.clone(),
                &model,
                RunConfig::new(),
            ))
            .await
            .unwrap();
            let mut wire = serde_json::to_value(initial.state()).unwrap();
            let decision = if approved { "allow" } else { "deny" };
            if legacy {
                // Schema 14 is the last one written before rules carried an owner.
                wire["schema_version"] = json!(14);
                wire["permission_rules"] = json!([{"decision": decision, "tool_name": "operate"}]);
            } else {
                wire["permission_rules"] =
                    json!([{"decision": decision, "tool_name": "operate", "agent_id": "first"}]);
            }
            let mut state: RunState = serde_json::from_value(wire).unwrap();
            state
                .approve(&initial.outcome().interruptions()[0], false)
                .unwrap();
            let result = Runner::run(
                run_request(first.clone(), second.clone(), &model, RunConfig::new())
                    .with_state(state),
            )
            .await
            .unwrap();
            if !legacy {
                assert!(result.outcome().interruptions().is_empty());
                assert_eq!(result.final_text(), "done");
                assert_eq!(calls.load(Ordering::SeqCst), if approved { 2 } else { 1 });
                continue;
            }
            let interruptions = result.outcome().interruptions();
            assert_eq!(interruptions.len(), 1, "the later call must ask again");
            let RunItemKind::ToolApproval(approval) = interruptions[0].kind() else {
                panic!("expected a tool approval");
            };
            assert_eq!(approval.call_id(), &CallId::new("later"));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let mut state = result.state().clone();
            state.approve(&interruptions[0], false).unwrap();
            let result = Runner::run(
                run_request(first.clone(), second.clone(), &model, RunConfig::new())
                    .with_state(state),
            )
            .await
            .unwrap();
            assert_eq!(result.final_text(), "done");
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }
    }
}
