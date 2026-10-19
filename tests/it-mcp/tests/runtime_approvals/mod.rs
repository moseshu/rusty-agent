//! Approval policies use the same frozen MCP tools as discovery and serialized recovery.

mod guardrails;

use super::*;
use ra_core::context::RunContext;

fn named_call(id: &str, naming: McpToolNaming) -> ModelResponse {
    let name = if naming == McpToolNaming::ServerPrefixed {
        "mcp_docs__first"
    } else {
        "first"
    };
    let mut tool_call = ToolCall::new(CallId::new(id), name, json!({}));
    if naming == McpToolNaming::Namespaced {
        tool_call = tool_call.with_namespace("mcp__docs");
    }
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(tool_call),
    )])
}

async fn approval_server(
    policy: RequireApprovalSetting,
    observations: Arc<Observations>,
    app: Arc<AppState>,
) -> InProcessToolServer {
    InProcessToolServer::connect(
        "docs",
        Catalog { app, observations },
        McpClientOptions::default()
            .with_cache_tools_list(true)
            .with_require_approval(policy),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn server_approval_pauses_and_serialized_decisions_control_execution_and_later_calls() {
    for streamed in [false, true] {
        for naming in [
            McpToolNaming::Original,
            McpToolNaming::ServerPrefixed,
            McpToolNaming::Namespaced,
        ] {
            for approve in [false, true] {
                for remember in [false, true] {
                    let app = Arc::new(AppState::default());
                    let observations = Arc::new(Observations::default());
                    let mut server = approval_server(
                        RequireApprovalSetting::Always,
                        observations.clone(),
                        app.clone(),
                    )
                    .await;
                    let agent = AgentSpec::builder()
                        .id(AgentId::new("public"))
                        .name("Public")
                        .tool_resolver(tool_resolver(
                            server.client(),
                            McpDiscoveryOptions::new().with_naming(naming),
                        ))
                        .build()
                        .unwrap();
                    let first_model = ScriptedModel::new(vec![named_call("one", naming)]);
                    let paused = drive(
                        request(
                            agent.clone().into(),
                            first_model.clone(),
                            app.clone(),
                            "approval-run",
                        ),
                        streamed,
                    )
                    .await
                    .unwrap();
                    let RunOutcome::Interrupted { items } = paused.outcome() else {
                        panic!("expected approval")
                    };
                    assert_eq!(items.len(), 1);
                    assert_eq!(first_model.requests.lock().unwrap().len(), 1);
                    assert!(observations.calls.lock().unwrap().is_empty());
                    let RunItemKind::ToolApproval(approval) = items[0].kind() else {
                        panic!("expected tool approval")
                    };
                    assert_eq!(
                        approval.recipient_binding(),
                        Some(&json!(["docs", "first", 0]))
                    );
                    let mut state = paused.state().clone();
                    if approve {
                        state.approve(&items[0], remember).unwrap();
                    } else {
                        state.reject(&items[0], remember).unwrap();
                    }
                    let state =
                        serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
                    let model =
                        ScriptedModel::new(vec![named_call("two", naming), final_response()]);
                    let result = drive(
                        request(agent.into(), model.clone(), app, "approval-run").with_state(state),
                        streamed,
                    )
                    .await
                    .unwrap();
                    if remember {
                        assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
                        assert_eq!(model.requests.lock().unwrap().len(), 2);
                    } else {
                        let RunOutcome::Interrupted { items } = result.outcome() else {
                            panic!("second call needs a new decision")
                        };
                        assert_eq!(items.len(), 1);
                        assert_eq!(model.requests.lock().unwrap().len(), 1);
                    }
                    let expected = if approve {
                        if remember { 2 } else { 1 }
                    } else {
                        0
                    };
                    assert_eq!(observations.calls.lock().unwrap().len(), expected);
                    assert!(result.state().generated_items().iter().any(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == "one")));
                    server.cleanup().await.unwrap();
                }
            }
        }
    }
}

#[tokio::test]
async fn dynamic_policy_receives_live_context_original_tool_and_discovering_public_agent() {
    for streamed in [false, true] {
        for asynchronous in [false, true] {
            let app = Arc::new(AppState::default());
            let observations = Arc::new(Observations::default());
            let seen = Arc::new(Mutex::new(vec![]));
            let observed = seen.clone();
            let callback = move |run: &RunContext,
                                 agent: &ra_core::context::RunAgent,
                                 mut tool: rmcp::model::Tool| {
                assert_eq!(run.run_id().as_str(), "dynamic-run");
                assert_eq!(agent.id().as_str(), "public");
                assert_eq!(agent.name(), "Public");
                let stage = run
                    .app_context::<AppState>()
                    .unwrap()
                    .stage
                    .load(Ordering::SeqCst);
                observed
                    .lock()
                    .unwrap()
                    .push((tool.name.to_string(), stage));
                let required = tool.name == "first" && stage == 0;
                tool.name = "callback mutation".into();
                Ok(required)
            };
            let policy = if asynchronous {
                let callback = Arc::new(callback);
                RequireApprovalSetting::dynamic(move |run, agent, tool| {
                    let callback = callback.clone();
                    Box::pin(async move {
                        tokio::task::yield_now().await;
                        callback(run, agent, tool)
                    })
                })
            } else {
                RequireApprovalSetting::dynamic_sync(callback)
            };
            let mut server = approval_server(policy, observations.clone(), app.clone()).await;
            let public = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public")
                .tool_resolver(tool_resolver(
                    server.client(),
                    McpDiscoveryOptions::new().with_naming(McpToolNaming::ServerPrefixed),
                ))
                .build()
                .unwrap();
            let execution = public
                .to_builder()
                .id(AgentId::new("private"))
                .name("Private")
                .build()
                .unwrap();
            let agent = AgentBinding::prepared(public, execution);
            let paused = drive(
                request(
                    agent.clone(),
                    ScriptedModel::new(vec![named_call("one", McpToolNaming::ServerPrefixed)]),
                    app.clone(),
                    "dynamic-run",
                ),
                streamed,
            )
            .await
            .unwrap();
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("expected approval")
            };
            assert_eq!(*seen.lock().unwrap(), [("first".to_owned(), 0)]);
            assert!(observations.calls.lock().unwrap().is_empty());
            let mut state = paused.state().clone();
            state.approve(&items[0], false).unwrap();
            let state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
            let result = drive(
                request(
                    agent,
                    ScriptedModel::new(vec![call("two", "mcp_docs__second"), final_response()]),
                    app,
                    "dynamic-run",
                )
                .with_state(state),
                streamed,
            )
            .await
            .unwrap();
            assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
            assert_eq!(
                *seen.lock().unwrap(),
                [("first".to_owned(), 0), ("second".to_owned(), 1)]
            );
            assert_eq!(*observations.calls.lock().unwrap(), ["first", "second"]);
            assert!(
                server
                    .client()
                    .cached_tools()
                    .unwrap()
                    .iter()
                    .any(|tool| tool.name == "first")
            );
            server.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn dynamic_policy_errors_stop_the_run_before_any_mcp_execution() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        let mut server = approval_server(
            RequireApprovalSetting::dynamic_sync(|_, _, _| {
                Err(Error::caller("approval predicate failed"))
            }),
            observations.clone(),
            app.clone(),
        )
        .await;
        let agent = AgentSpec::builder()
            .id(AgentId::new("public"))
            .name("Public")
            .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
            .build()
            .unwrap();
        let model = ScriptedModel::new(vec![call("one", "first"), final_response()]);
        let error = drive(
            request(agent.into(), model.clone(), app, "error-run"),
            streamed,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("approval predicate failed"));
        assert!(observations.calls.lock().unwrap().is_empty());
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn dynamic_policy_is_cancelled_without_running_the_mcp_tool() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        let entered = Arc::new(tokio::sync::Notify::new());
        let callback_entered = entered.clone();
        let policy = RequireApprovalSetting::dynamic(move |_, _, _| {
            let entered = callback_entered.clone();
            Box::pin(async move {
                entered.notify_one();
                std::future::pending::<Result<bool>>().await
            })
        });
        let mut server = approval_server(policy, observations.clone(), app.clone()).await;
        let agent = AgentSpec::builder()
            .id(AgentId::new("public"))
            .name("Public")
            .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
            .build()
            .unwrap();
        let model = ScriptedModel::new(vec![call("one", "first")]);
        let cancel = CancelScope::root();
        let task = tokio::spawn(drive(
            RunRequest::new(
                agent.into(),
                Arc::new(Resolver(model.clone())),
                RunId::new("cancelled-run"),
                cancel.clone(),
                vec![],
            )
            .with_app_context(app),
            streamed,
        ));
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        cancel.cancel(CancelReason::UserInterrupt);
        let result = task.await.unwrap();
        assert!(result.unwrap_err().is_cancelled());
        assert!(observations.calls.lock().unwrap().is_empty());
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn deferred_mcp_tools_keep_server_approval_after_tool_search_and_resume() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        let mut server = approval_server(
            RequireApprovalSetting::Always,
            observations.clone(),
            app.clone(),
        )
        .await;
        let agent = AgentSpec::builder()
            .id(AgentId::new("public"))
            .name("Public")
            .tool_resolver(tool_resolver(
                server.client(),
                McpDiscoveryOptions::new().with_tool_search(true),
            ))
            .build()
            .unwrap();
        let search = ToolCall::new(
            CallId::new("search"),
            "tool_search",
            json!({"query":"first"}),
        )
        .with_kind(ra_core::item::ToolCallKind::ToolSearch);
        let search = ModelResponse::new(vec![RunItem::new(
            ItemId::new("search"),
            RunItemKind::ToolCall(search),
        )]);
        let tool_call =
            ToolCall::new(CallId::new("one"), "first", json!({})).with_namespace("first");
        let invocation = ModelResponse::new(vec![RunItem::new(
            ItemId::new("one"),
            RunItemKind::ToolCall(tool_call),
        )]);
        let model = ScriptedModel::new(vec![search, invocation]);
        let paused = drive(
            request(
                agent.clone().into(),
                model.clone(),
                app.clone(),
                "deferred-run",
            ),
            streamed,
        )
        .await
        .unwrap();
        let RunOutcome::Interrupted { items } = paused.outcome() else {
            panic!("expected approval")
        };
        assert_eq!(items.len(), 1);
        assert!(model.names(0).iter().all(|name| name != "first"));
        assert!(observations.calls.lock().unwrap().is_empty());
        let mut state = paused.state().clone();
        state.approve(&items[0], false).unwrap();
        let state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
        let result = drive(
            request(
                agent.into(),
                ScriptedModel::new(vec![final_response()]),
                app,
                "deferred-run",
            )
            .with_state(state),
            streamed,
        )
        .await
        .unwrap();
        assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
        assert_eq!(*observations.calls.lock().unwrap(), ["first"]);
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn remembered_dynamic_decisions_skip_the_callback_for_the_same_tool_only() {
    for streamed in [false, true] {
        for approve in [false, true] {
            let app = Arc::new(AppState::default());
            let observations = Arc::new(Observations::default());
            let seen = Arc::new(Mutex::new(vec![]));
            let observed = seen.clone();
            let policy = RequireApprovalSetting::dynamic(move |_, _, tool| {
                let observed = observed.clone();
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    observed.lock().unwrap().push(tool.name.to_string());
                    Ok(true)
                })
            });
            let mut server = approval_server(policy, observations.clone(), app.clone()).await;
            let agent = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public")
                .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
                .build()
                .unwrap();
            let paused = drive(
                request(
                    agent.clone().into(),
                    ScriptedModel::new(vec![call("one", "first")]),
                    app.clone(),
                    "remembered-run",
                ),
                streamed,
            )
            .await
            .unwrap();
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("expected approval")
            };
            let mut state = paused.state().clone();
            if approve {
                state.approve(&items[0], true).unwrap();
            } else {
                state.reject(&items[0], true).unwrap();
            }
            let state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
            let model = ScriptedModel::new(vec![call("two", "first"), call("three", "second")]);
            let result = drive(
                request(agent.into(), model.clone(), app, "remembered-run").with_state(state),
                streamed,
            )
            .await
            .unwrap();
            let RunOutcome::Interrupted { items } = result.outcome() else {
                panic!("another tool still requires approval")
            };
            assert_eq!(items.len(), 1);
            let RunItemKind::ToolApproval(approval) = items[0].kind() else {
                panic!("expected tool approval")
            };
            assert_eq!(
                approval.recipient_binding(),
                Some(&json!(["docs", "second", 0]))
            );
            assert_eq!(*seen.lock().unwrap(), ["first", "second"]);
            assert_eq!(
                *observations.calls.lock().unwrap(),
                if approve {
                    vec!["first", "first"]
                } else {
                    vec![]
                }
            );
            assert_eq!(model.requests.lock().unwrap().len(), 2);
            server.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn deferred_dynamic_policy_runs_after_search_with_current_context_and_resumes_once() {
    for streamed in [false, true] {
        for required in [false, true] {
            let app = Arc::new(AppState::default());
            let observations = Arc::new(Observations::default());
            let seen = Arc::new(Mutex::new(vec![]));
            let observed = seen.clone();
            let policy = RequireApprovalSetting::dynamic(move |run, agent, tool| {
                let observed = observed.clone();
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    assert_eq!(agent.id().as_str(), "public");
                    assert_eq!(run.run_id().as_str(), "dynamic-deferred-run");
                    observed.lock().unwrap().push((
                        tool.name.to_string(),
                        run.app_context::<AppState>()
                            .unwrap()
                            .stage
                            .load(Ordering::SeqCst),
                    ));
                    Ok(required)
                })
            });
            let mut server = approval_server(policy, observations.clone(), app.clone()).await;
            let agent = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public")
                .tool_resolver(tool_resolver(
                    server.client(),
                    McpDiscoveryOptions::new().with_tool_search(true),
                ))
                .build()
                .unwrap();
            let search = ToolCall::new(
                CallId::new("search"),
                "tool_search",
                json!({"query":"first"}),
            )
            .with_kind(ra_core::item::ToolCallKind::ToolSearch);
            let search = ModelResponse::new(vec![RunItem::new(
                ItemId::new("search"),
                RunItemKind::ToolCall(search),
            )]);
            let invocation =
                ToolCall::new(CallId::new("one"), "first", json!({})).with_namespace("first");
            let invocation = ModelResponse::new(vec![RunItem::new(
                ItemId::new("one"),
                RunItemKind::ToolCall(invocation),
            )]);
            let callback_seen = seen.clone();
            let current_app = app.clone();
            let mut script = vec![search, invocation];
            if !required {
                script.push(final_response());
            }
            let model = Arc::new(ScriptedModel {
                responses: Mutex::new(script.into()),
                requests: Mutex::new(vec![]),
                before_response: Some(Arc::new(move |index| {
                    if index <= 1 {
                        assert!(callback_seen.lock().unwrap().is_empty());
                    }
                    if index == 1 {
                        current_app.stage.store(7, Ordering::SeqCst);
                    }
                })),
            });
            let result = drive(
                request(
                    agent.clone().into(),
                    model.clone(),
                    app.clone(),
                    "dynamic-deferred-run",
                ),
                streamed,
            )
            .await
            .unwrap();
            assert!(model.names(0).iter().all(|name| name != "first"));
            assert_eq!(*seen.lock().unwrap(), [("first".to_owned(), 7)]);
            if required {
                let RunOutcome::Interrupted { items } = result.outcome() else {
                    panic!("expected approval")
                };
                assert!(observations.calls.lock().unwrap().is_empty());
                let mut state = result.state().clone();
                state.approve(&items[0], false).unwrap();
                let state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
                let resumed = drive(
                    request(
                        agent.into(),
                        ScriptedModel::new(vec![final_response()]),
                        app,
                        "dynamic-deferred-run",
                    )
                    .with_state(state),
                    streamed,
                )
                .await
                .unwrap();
                assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
            } else {
                assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
            }
            assert_eq!(*seen.lock().unwrap(), [("first".to_owned(), 7)]);
            assert_eq!(*observations.calls.lock().unwrap(), ["first"]);
            server.cleanup().await.unwrap();
        }
    }
}
