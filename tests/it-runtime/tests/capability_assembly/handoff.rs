//! Capability ownership across agent activation and persisted approval recovery.

use super::*;
use ra_core::{agent::HandoffSpec, state::RunState, tool::ToolApprovalPolicy};
use ra_runtime::agent::AgentRegistry;

async fn drive(request: RunRequest, streamed: bool) -> ra_runtime::runner::RunResult {
    if streamed {
        let mut stream = Runner::run_streamed(request);
        while stream.next_event().await.is_some() {}
        stream.finish().await.unwrap()
    } else {
        Runner::run(request).await.unwrap()
    }
}

fn transfer(target: &str) -> HandoffSpec {
    HandoffSpec::new(
        AgentId::new(target),
        ToolSchema::new(
            format!("transfer_to_{target}"),
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .unwrap(),
    )
}

fn configured_source(
    prepared: bool,
    handoff: HandoffSpec,
) -> (Arc<AgentSpec>, RunConfig, Arc<AtomicUsize>) {
    let capability = TestCapability::new(CapabilityFamily::SHELL)
        .with_tool("exec_command")
        .with_static_section("shell", "static shell instructions")
        .with_section("shell", "run shell instructions")
        .with_deferred_section("deferred shell instructions")
        .with_temperature(0.2)
        .with_processor();
    let static_reads = capability.static_resolutions.clone();
    let tools = capability.tools();
    let mut source = agent().to_builder().handoff(handoff);
    let config = if prepared {
        source = source
            .instructions("do the thing\n\nstatic shell instructions")
            .tools(tools.iter().cloned());
        RunConfig::new().with_prepared_capabilities([capability.into_shared()], tools)
    } else {
        RunConfig::new().with_capability(capability.into_shared())
    };
    let config = config.with_capability(Arc::new(TurnArmedCapability {
        kind: CapabilityFamily::SKILLS,
        turn: 2,
    }));
    (source.build().unwrap(), config, static_reads)
}

fn request_for(
    agent: Arc<AgentSpec>,
    model: &Arc<RecordingModel>,
    config: RunConfig,
) -> RunRequest {
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(FixedResolver {
            model: model.clone(),
        }),
        RunId::new("run-handoff-capabilities"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("go"))],
    )
    .with_config(config)
}

fn assert_source(model: &RecordingModel, turn: usize) {
    assert_eq!(
        model.instructions.lock().unwrap()[turn].as_deref(),
        Some("do the thing\n\nstatic shell instructions\n\nrun shell instructions")
    );
    assert!(model.tools.lock().unwrap()[turn].contains(&"exec_command".to_owned()));
    assert_eq!(model.temperatures.lock().unwrap()[turn], Some(0.2));
    assert!(
        model
            .messages(turn)
            .contains(&"processed by shell".to_owned())
    );
}

fn assert_reviewer(model: &RecordingModel, turn: usize, tools: &[&str]) {
    assert_eq!(
        model.instructions.lock().unwrap()[turn].as_deref(),
        Some("review it")
    );
    assert_eq!(model.tools.lock().unwrap()[turn], tools);
    assert_eq!(model.temperatures.lock().unwrap()[turn], None);
    assert!(
        !model
            .messages(turn)
            .contains(&"processed by shell".to_owned())
    );
    assert!(
        !model
            .messages(turn)
            .contains(&"deferred shell instructions".to_owned())
    );
    assert!(
        !model
            .messages(turn)
            .contains(&"Skills: a skill is loaded by name.".to_owned())
    );
}

#[tokio::test]
async fn handoffs_switch_all_capability_channels_together() {
    for prepared in [false, true] {
        for streamed in [false, true] {
            let (source, config, _) = configured_source(prepared, transfer("reviewer"));
            // Calling a target-owned tool with the same name must not earn the source's prompt.
            let reviewer = AgentSpec::builder()
                .id(AgentId::new("reviewer"))
                .name("Reviewer")
                .instructions("review it")
                .tool(Arc::new(NamedTool::new("exec_command")))
                .build()
                .unwrap();
            let config = config.with_agent_registry(
                AgentRegistry::builder()
                    .register(source.clone())
                    .register(reviewer)
                    .build()
                    .unwrap(),
            );
            let model = RecordingModel::scripted(&["transfer_to_reviewer", "exec_command"]);
            drive(request_for(source, &model, config), streamed).await;
            assert_source(&model, 0);
            assert_reviewer(&model, 1, &["exec_command"]);
            assert_reviewer(&model, 2, &["exec_command"]);
        }
    }
}

#[tokio::test]
async fn self_handoffs_reuse_the_complete_preparation() {
    for prepared in [false, true] {
        for streamed in [false, true] {
            let (source, config, static_reads) = configured_source(prepared, transfer("coder"));
            let config = config.with_agent_registry(
                AgentRegistry::builder()
                    .register(source.clone())
                    .build()
                    .unwrap(),
            );
            let model = RecordingModel::scripted(&["transfer_to_coder", "exec_command"]);
            drive(request_for(source, &model, config), streamed).await;
            for turn in 0..3 {
                assert_source(&model, turn);
            }
            assert_eq!(static_reads.load(Ordering::SeqCst), usize::from(!prepared));
            assert!(
                model
                    .messages(1)
                    .contains(&"Skills: a skill is loaded by name.".to_owned())
            );
            assert!(
                model
                    .messages(2)
                    .contains(&"deferred shell instructions".to_owned())
            );
        }
    }
}

#[tokio::test]
async fn returning_from_another_agent_reuses_the_original_preparation() {
    for prepared in [false, true] {
        for streamed in [false, true] {
            let (source, config, static_reads) = configured_source(prepared, transfer("reviewer"));
            let reviewer = AgentSpec::builder()
                .id(AgentId::new("reviewer"))
                .name("Reviewer")
                .instructions("review it")
                .handoff(transfer("coder"))
                .build()
                .unwrap();
            let config = config.with_agent_registry(
                AgentRegistry::builder()
                    .register(source.clone())
                    .register(reviewer)
                    .build()
                    .unwrap(),
            );
            let model = RecordingModel::scripted(&[
                "transfer_to_reviewer",
                "transfer_to_coder",
                "exec_command",
            ]);
            drive(request_for(source, &model, config), streamed).await;
            assert_source(&model, 0);
            assert_reviewer(&model, 1, &[]);
            assert_source(&model, 2);
            assert_source(&model, 3);
            assert_eq!(static_reads.load(Ordering::SeqCst), usize::from(!prepared));
        }
    }
}

#[tokio::test]
async fn approval_resume_keeps_capabilities_on_the_checkpoint_starting_agent() {
    for prepared in [false, true] {
        for streamed in [false, true] {
            let (source, config, _) = configured_source(prepared, transfer("reviewer"));
            let mut guarded = NamedTool::new("guarded");
            guarded.options = guarded.options.with_approval(ToolApprovalPolicy::Always);
            let reviewer = AgentSpec::builder()
                .id(AgentId::new("reviewer"))
                .name("Reviewer")
                .instructions("review it")
                .tool(Arc::new(guarded))
                .handoff(transfer("coder"))
                .build()
                .unwrap();
            let config = config.with_agent_registry(
                AgentRegistry::builder()
                    .register(source.clone())
                    .register(reviewer.clone())
                    .build()
                    .unwrap(),
            );
            let model = RecordingModel::scripted(&[
                "transfer_to_reviewer",
                "guarded",
                "transfer_to_coder",
                "exec_command",
            ]);
            let paused = drive(request_for(source, &model, config.clone()), streamed).await;
            assert_eq!(model.calls.load(Ordering::SeqCst), 2);
            let mut state: RunState =
                serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
            let approval = state.pending_interruption_items().next().unwrap().clone();
            state.approve(&approval, false).unwrap();
            let resumed = drive(
                RunRequest::new(
                    AgentBinding::direct(reviewer),
                    Arc::new(FixedResolver {
                        model: model.clone(),
                    }),
                    state.run_id().clone(),
                    CancelScope::root(),
                    Vec::new(),
                )
                .with_config(config)
                .with_state(state),
                streamed,
            )
            .await;
            assert_eq!(
                resumed.state().starting_agent(),
                Some(&AgentId::new("coder"))
            );
            assert_source(&model, 0);
            assert_reviewer(&model, 1, &["guarded"]);
            assert_reviewer(&model, 2, &["guarded"]);
            assert_source(&model, 3);
            assert_source(&model, 4);
        }
    }
}

#[tokio::test]
async fn approval_resume_without_a_registered_source_does_not_reassign_capabilities() {
    for prepared in [false, true] {
        for streamed in [false, true] {
            let (source, config, _) = configured_source(prepared, transfer("reviewer"));
            let mut guarded = NamedTool::new("guarded");
            guarded.options = guarded.options.with_approval(ToolApprovalPolicy::Always);
            let reviewer = AgentSpec::builder()
                .id(AgentId::new("reviewer"))
                .name("Reviewer")
                .instructions("review it")
                .tool(Arc::new(guarded))
                .build()
                .unwrap();
            // A registry need only contain the handoff targets, not the opening declaration.
            let config = config.with_agent_registry(
                AgentRegistry::builder()
                    .register(reviewer.clone())
                    .build()
                    .unwrap(),
            );
            let model = RecordingModel::scripted(&["transfer_to_reviewer", "guarded"]);
            let paused = drive(request_for(source, &model, config.clone()), streamed).await;
            let mut state: RunState =
                serde_json::from_value(serde_json::to_value(paused.state()).unwrap()).unwrap();
            let approval = state.pending_interruption_items().next().unwrap().clone();
            state.approve(&approval, false).unwrap();
            drive(
                RunRequest::new(
                    AgentBinding::direct(reviewer),
                    Arc::new(FixedResolver {
                        model: model.clone(),
                    }),
                    state.run_id().clone(),
                    CancelScope::root(),
                    Vec::new(),
                )
                .with_config(config)
                .with_state(state),
                streamed,
            )
            .await;
            assert_reviewer(&model, 1, &["guarded"]);
            assert_reviewer(&model, 2, &["guarded"]);
        }
    }
}
