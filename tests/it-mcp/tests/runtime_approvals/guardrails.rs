//! Server guardrail declarations use the ordinary approval and guardrail dispatch order.

use super::*;
use ra_core::{
    guardrail::{
        ToolGuardrailFunctionOutput, ToolInputGuardrail, ToolInputGuardrailData,
        ToolOutputGuardrail, ToolOutputGuardrailData,
    },
    tool::ToolGuardrailId,
};

struct RecordingGuardrail {
    id: ToolGuardrailId,
    seen: Arc<Mutex<Vec<String>>>,
    reject: bool,
}
impl RecordingGuardrail {
    fn new(id: &str, seen: Arc<Mutex<Vec<String>>>, reject: bool) -> Arc<Self> {
        Arc::new(Self {
            id: ToolGuardrailId::new(id).unwrap(),
            seen,
            reject,
        })
    }
    fn record(&self, run: &RunContext, origin: &ToolOrigin) -> ToolGuardrailFunctionOutput {
        self.seen.lock().unwrap().push(format!(
            "{}:{}:{}",
            self.id,
            origin.name(),
            run.app_context::<AppState>()
                .unwrap()
                .stage
                .load(Ordering::SeqCst)
        ));
        if self.reject {
            ToolGuardrailFunctionOutput::reject_content(format!("blocked by {}", self.id))
        } else {
            ToolGuardrailFunctionOutput::allow()
        }
    }
}
#[async_trait]
impl ToolInputGuardrail for RecordingGuardrail {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }
    async fn check(
        &self,
        data: &ToolInputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        assert_eq!(data.arguments(), &json!({}));
        Ok(self.record(data.run(), data.origin()))
    }
}
#[async_trait]
impl ToolOutputGuardrail for RecordingGuardrail {
    fn id(&self) -> &ToolGuardrailId {
        &self.id
    }
    async fn check(
        &self,
        data: &ToolOutputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput> {
        assert_eq!(data.arguments(), &json!({}));
        assert_eq!(data.output().as_text(), Some("called first"));
        Ok(self.record(data.run(), data.origin()))
    }
}

#[tokio::test]
async fn mcp_guardrails_preserve_approval_and_serialized_resume_order() {
    for streamed in [false, true] {
        for pre_approval in [false, true] {
            let app = Arc::new(AppState::default());
            let observations = Arc::new(Observations::default());
            let seen = Arc::new(Mutex::new(vec![]));
            let input = RecordingGuardrail::new("input", seen.clone(), false);
            let second_input = RecordingGuardrail::new("second_input", seen.clone(), false);
            let output = RecordingGuardrail::new("output", seen.clone(), false);
            let mut server = InProcessToolServer::connect(
                "docs",
                Catalog {
                    app: app.clone(),
                    observations: observations.clone(),
                },
                McpClientOptions::default()
                    .with_require_approval(true)
                    .with_tool_input_guardrails([input.id.clone(), second_input.id.clone()])
                    .with_tool_output_guardrails([output.id.clone()]),
            )
            .await
            .unwrap();
            let agent = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public")
                .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
                .build()
                .unwrap();
            let config = RunConfig::new()
                .with_pre_approval_tool_input_guardrails(pre_approval)
                .with_tool_input_guardrail(second_input)
                .with_tool_input_guardrail(input)
                .with_tool_output_guardrail(output);
            let paused = drive(
                request(
                    agent.clone().into(),
                    ScriptedModel::new(vec![call("one", "first")]),
                    app.clone(),
                    "guardrail-run",
                )
                .with_config(config.clone()),
                streamed,
            )
            .await
            .unwrap();
            let RunOutcome::Interrupted { items } = paused.outcome() else {
                panic!("expected approval")
            };
            assert_eq!(items.len(), 1);
            assert!(observations.calls.lock().unwrap().is_empty());
            let mut expected: Vec<String> = if pre_approval {
                vec!["input:first:0".into(), "second_input:first:0".into()]
            } else {
                vec![]
            };
            assert_eq!(*seen.lock().unwrap(), expected);
            let mut state = paused.state().clone();
            state.approve(&items[0], false).unwrap();
            let state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
            let resumed = drive(
                request(
                    agent.into(),
                    ScriptedModel::new(vec![final_response()]),
                    app,
                    "guardrail-run",
                )
                .with_config(config)
                .with_state(state),
                streamed,
            )
            .await
            .unwrap();
            assert!(matches!(resumed.outcome(), RunOutcome::Completed { .. }));
            assert_eq!(*observations.calls.lock().unwrap(), ["first"]);
            expected.extend([
                "input:first:0".into(),
                "second_input:first:0".into(),
                "output:first:1".into(),
            ]);
            assert_eq!(*seen.lock().unwrap(), expected);
            server.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn server_input_and_output_guardrails_can_reject_their_respective_boundaries() {
    for streamed in [false, true] {
        for reject_input in [false, true] {
            let app = Arc::new(AppState::default());
            let observations = Arc::new(Observations::default());
            let seen = Arc::new(Mutex::new(vec![]));
            let input = RecordingGuardrail::new("input", seen.clone(), reject_input);
            let output = RecordingGuardrail::new("output", seen.clone(), !reject_input);
            let mut server = InProcessToolServer::connect(
                "docs",
                Catalog {
                    app: app.clone(),
                    observations: observations.clone(),
                },
                McpClientOptions::default()
                    .with_tool_input_guardrails([input.id.clone()])
                    .with_tool_output_guardrails([output.id.clone()]),
            )
            .await
            .unwrap();
            let agent = AgentSpec::builder()
                .id(AgentId::new("public"))
                .name("Public")
                .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
                .build()
                .unwrap();
            let config = RunConfig::new()
                .with_tool_input_guardrail(input)
                .with_tool_output_guardrail(output);
            let result = drive(
                request(
                    agent.into(),
                    ScriptedModel::new(vec![call("one", "first"), final_response()]),
                    app,
                    "rejected-run",
                )
                .with_config(config),
                streamed,
            )
            .await
            .unwrap();
            assert!(matches!(result.outcome(), RunOutcome::Completed { .. }));
            assert_eq!(
                observations.calls.lock().unwrap().len(),
                usize::from(!reject_input)
            );
            let expected: Vec<String> = if reject_input {
                vec!["input:first:0".into()]
            } else {
                vec!["input:first:0".into(), "output:first:1".into()]
            };
            assert_eq!(*seen.lock().unwrap(), expected);
            let message = if reject_input {
                "blocked by input"
            } else {
                "blocked by output"
            };
            let output = result
                .new_items()
                .iter()
                .find_map(|item| match item.kind() {
                    RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == "one" => {
                        Some(output)
                    }
                    _ => None,
                })
                .unwrap();
            let output: ToolOutput = serde_json::from_value(output.output().clone()).unwrap();
            assert_eq!(output.as_text(), Some(message));
            server.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn missing_server_guardrail_registration_fails_before_the_model_call() {
    for streamed in [false, true] {
        let app = Arc::new(AppState::default());
        let observations = Arc::new(Observations::default());
        let mut server = InProcessToolServer::connect(
            "docs",
            Catalog {
                app: app.clone(),
                observations: observations.clone(),
            },
            McpClientOptions::default()
                .with_tool_input_guardrails([ToolGuardrailId::new("missing_input").unwrap()]),
        )
        .await
        .unwrap();
        let agent = AgentSpec::builder()
            .id(AgentId::new("public"))
            .name("Public")
            .tool_resolver(tool_resolver(server.client(), McpDiscoveryOptions::new()))
            .build()
            .unwrap();
        let model = ScriptedModel::new(vec![final_response()]);
        let error = drive(
            request(agent.into(), model.clone(), app, "missing-run"),
            streamed,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("missing_input"));
        assert!(model.requests.lock().unwrap().is_empty());
        assert!(observations.calls.lock().unwrap().is_empty());
        server.cleanup().await.unwrap();
    }
}
