//! Codex's input-queue activity behavior through real thread steering and collaboration waits.

use super::*;
use ra_core::agent::control::{AgentControlPort, MessageDeliveryMode};
use ra_runtime::agent::control::AgentControl;
use ra_tools::agent_ns::{CollaborationKind, CollaborationTool, WaitAgentTimeoutOptions};

struct ObservedWait {
    inner: CollaborationTool,
    entered: Arc<Notify>,
}

#[async_trait]
impl Tool for ObservedWait {
    fn origin(&self) -> &ToolOrigin {
        self.inner.origin()
    }
    fn schema(&self) -> &ToolSchema {
        self.inner.schema()
    }
    fn options(&self) -> ToolOptions {
        self.inner.options()
    }
    async fn is_enabled_with_services(
        &self,
        context: &ra_core::context::RunContext,
        services: &ra_core::tool::ToolServices,
    ) -> Result<bool> {
        self.inner.is_enabled_with_services(context, services).await
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.entered.notify_one();
        self.inner.call(context).await
    }
}

fn waiting_thread(script: &Arc<Script>, entered: &Arc<Notify>) -> (AgentThread, AgentControl) {
    let control = AgentControl::new();
    let wait = ObservedWait {
        inner: CollaborationTool::new(
            CollaborationKind::WaitAgent,
            WaitAgentTimeoutOptions::new().with_bounds(60_000, 0, 60_000),
        )
        .unwrap(),
        entered: entered.clone(),
    };
    let agent = AgentSpec::builder()
        .id(AgentId::new("waiter"))
        .name("Waiter")
        .tools(vec![Arc::new(wait) as Arc<dyn Tool>])
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new("waiting-thread"),
        AgentBinding::direct(agent),
        Arc::new(Resolver(script.clone())),
    )
    .with_agent_control(control.clone())
    .build();
    (thread, control)
}

fn assert_steered_observation(input: &[ModelInputItem]) {
    let output = input
        .iter()
        .rev()
        .find_map(|item| {
            let ModelInputItem::ToolCallOutput(output) = item else {
                return None;
            };
            ToolOutput::from_stored(output.output()).unwrap()
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(output.as_text().unwrap()).unwrap(),
        json!({"message":"Wait interrupted by new input.","timed_out":false})
    );
}

#[tokio::test]
async fn steering_wakes_a_wait_and_delivers_input_once_at_the_next_model_call() {
    let script = Arc::new(Script::default());
    let entered = Arc::new(Notify::new());
    let (thread, control) = waiting_thread(&script, &entered);
    script.push(Step::Respond(tool_call(
        "wait-response",
        "wait-call",
        "wait_agent",
    )));
    script.push(Step::Respond(final_message("done", "handled new input")));
    let turn = started(&thread, text("wait for work")).await;
    timeout(PATIENCE, entered.notified()).await.unwrap();
    assert_eq!(script.calls().len(), 1);
    assert!(matches!(
        thread.steer_turn(&text("new user input"), &turn),
        SteerSubmission::Steered { .. }
    ));
    assert_eq!(
        completes(&thread, &turn).await.as_deref(),
        Some("handled new input")
    );
    let calls = script.calls();
    assert_eq!(calls.len(), 2);
    assert_steered_observation(&calls[1].input);
    assert_eq!(
        texts(&calls[1].input)
            .iter()
            .filter(|text| *text == "new user input")
            .count(),
        1
    );
    thread.shutdown().await;
    control.shutdown().await;
}

#[tokio::test]
async fn already_queued_steering_takes_priority_over_mail_without_consuming_either() {
    for with_mail in [false, true] {
        let script = Arc::new(Script::default());
        let entered = Arc::new(Notify::new());
        let (thread, control) = waiting_thread(&script, &entered);
        let (release, wait) = gate();
        script.push(Step::After(
            wait,
            tool_call("wait-response", "wait-call", "wait_agent"),
        ));
        script.push(Step::Respond(final_message("done", "handled queued input")));
        let turn = started(&thread, text("wait for work")).await;
        script.wait_for_calls(1).await;
        assert!(matches!(
            thread.steer_turn(&text("queued steering"), &turn),
            SteerSubmission::Steered { .. }
        ));
        if with_mail {
            control
                .root()
                .send(
                    "/root",
                    "queued mail".to_owned(),
                    MessageDeliveryMode::QueueOnly,
                )
                .await
                .unwrap();
        }
        release.notify_one();
        completes(&thread, &turn).await;
        let calls = script.calls();
        assert_steered_observation(&calls[1].input);
        let texts = texts(&calls[1].input);
        assert_eq!(
            texts
                .iter()
                .filter(|text| *text == "queued steering")
                .count(),
            1
        );
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.contains("queued mail"))
                .count(),
            usize::from(with_mail)
        );
        thread.shutdown().await;
        control.shutdown().await;
    }
}

#[tokio::test]
async fn interrupting_a_wait_does_not_leave_stale_activity_in_the_next_turn() {
    let script = Arc::new(Script::default());
    let entered = Arc::new(Notify::new());
    let (thread, control) = waiting_thread(&script, &entered);
    script.push(Step::Respond(tool_call(
        "first-wait",
        "first-call",
        "wait_agent",
    )));
    started(&thread, text("first turn")).await;
    timeout(PATIENCE, entered.notified()).await.unwrap();
    assert!(thread.interrupt());
    assert!(matches!(
        next_event(&thread).await,
        ThreadEvent::TurnStarted { .. }
    ));
    assert!(matches!(
        next_event(&thread).await,
        ThreadEvent::TurnAborted { .. }
    ));
    script.push(Step::Respond(tool_call(
        "second-wait",
        "second-call",
        "wait_agent",
    )));
    script.push(Step::Respond(final_message("done", "second turn done")));
    let turn = started(&thread, text("second turn")).await;
    timeout(PATIENCE, entered.notified()).await.unwrap();
    assert_eq!(script.calls().len(), 2);
    assert!(matches!(
        thread.steer_turn(&text("second steering"), &turn),
        SteerSubmission::Steered { .. }
    ));
    completes(&thread, &turn).await;
    assert_steered_observation(&script.calls()[2].input);
    thread.shutdown().await;
    control.shutdown().await;
}
