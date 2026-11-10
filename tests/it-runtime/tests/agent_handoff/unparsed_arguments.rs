//! A handoff whose argument text is not JSON, as the reference invokes handoffs: a handoff without
//! a handler, or with a callback that takes no input, transfers as usual, the callback running;
//! one whose handler reads the payload, typed or custom, fails the run before the handler runs,
//! as the reference refuses typed input it cannot parse. The same holds when the transfer waits for
//! an approval in its turn and resumes from a checkpoint.
use super::*;
use serde_json::Value;

fn unparsed_transfer(id: &str, call_id: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::unparsed(
            CallId::new(call_id),
            "transfer_to_reviewer",
            "{bad",
        )),
    )
}

/// A type whose own `Deserialize` accepts anything, the text included. It is still not handed
/// text that was not JSON.
#[derive(schemars::JsonSchema)]
struct Lenient(#[allow(dead_code)] Value);

impl<'de> serde::Deserialize<'de> for Lenient {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Lenient)
    }
}

struct Counting(Arc<AtomicUsize>);

#[async_trait]
impl ra_core::agent::HandoffInvocationHandler for Counting {
    async fn on_handoff(&self, _: &RunContext, _: &Value) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// How the handoff is declared, and whether its handler reads the payload.
#[derive(Clone, Copy, Debug)]
enum Declared {
    NoHandler,
    CallbackWithoutInput,
    Typed,
    LenientTyped,
    Custom,
}

impl Declared {
    const ALL: [Self; 5] = [
        Self::NoHandler,
        Self::CallbackWithoutInput,
        Self::Typed,
        Self::LenientTyped,
        Self::Custom,
    ];

    const fn reads_payload(self) -> bool {
        matches!(self, Self::Typed | Self::LenientTyped | Self::Custom)
    }

    fn handoff(self, target: &Arc<AgentSpec>, calls: &Arc<AtomicUsize>) -> HandoffSpec {
        let handoff = target.as_handoff().unwrap();
        let calls = Arc::clone(calls);
        match self {
            Self::NoHandler => handoff,
            Self::CallbackWithoutInput => handoff.with_on_handoff_fn(move |_: &RunContext| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(()))
            }),
            Self::Typed => handoff
                .with_input_type(move |_: &RunContext, _: TransferReason| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok(()))
                })
                .unwrap(),
            Self::LenientTyped => handoff
                .with_input_type(move |_: &RunContext, _: Lenient| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok(()))
                })
                .unwrap(),
            Self::Custom => handoff.with_on_handoff(Arc::new(Counting(calls))),
        }
    }
}

fn registry(source: &Arc<AgentSpec>, target: &Arc<AgentSpec>) -> RunConfig {
    RunConfig::new().with_agent_registry(
        AgentRegistry::builder()
            .register(Arc::clone(source))
            .register(Arc::clone(target))
            .build()
            .unwrap(),
    )
}

/// Item 10: in the turn that makes it.
#[tokio::test]
async fn an_unparsed_handoff_transfers_unless_its_handler_reads_the_payload() {
    for streamed in [false, true] {
        for declared in Declared::ALL {
            let calls = Arc::new(AtomicUsize::new(0));
            let target = reviewer(Vec::new());
            let source = planner(declared.handoff(&target, &calls), Vec::new());
            let model = ScriptedModel::new(vec![
                ModelResponse::new(vec![unparsed_transfer("transfer", "handoff")]),
                ModelResponse::new(vec![message("done", "reviewed")]),
            ]);
            let result = drive(
                run_request(
                    Arc::clone(&source),
                    Arc::clone(&target),
                    &model,
                    registry(&source, &target),
                ),
                streamed,
            )
            .await;
            let label = format!("{declared:?} streamed={streamed}");
            if declared.reads_payload() {
                let error = result.unwrap_err();
                assert_eq!(error.code(), "provider.behavior", "{label}: {error}");
                assert_eq!(calls.load(Ordering::SeqCst), 0, "{label}");
                assert_eq!(model.calls.load(Ordering::SeqCst), 1, "{label}");
            } else {
                let result = result.unwrap();
                assert_eq!(result.last_agent().id(), target.id(), "{label}");
                assert_eq!(result.final_text(), "reviewed", "{label}");
                let expected = usize::from(matches!(declared, Declared::CallbackWithoutInput));
                assert_eq!(calls.load(Ordering::SeqCst), expected, "{label}");
            }
        }
    }
}

/// Item 10: a transfer that waits for an approval in its turn resumes, from a checkpoint written
/// and read back, still carrying the text.
#[tokio::test]
async fn an_unparsed_handoff_resumed_after_an_approval_keeps_its_text() {
    for declared in Declared::ALL {
        let calls = Arc::new(AtomicUsize::new(0));
        let probe_calls = Arc::new(AtomicUsize::new(0));
        let target = reviewer(Vec::new());
        let source = planner(declared.handoff(&target, &calls), Vec::new())
            .to_builder()
            .tool(Arc::new(ApprovalProbe {
                origin: ra_core::tool::ToolOrigin::new("probe").unwrap(),
                schema: ToolSchema::new(
                    "probe",
                    json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
                )
                .unwrap(),
                calls: Arc::clone(&probe_calls),
            }))
            .build()
            .unwrap();
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![
                item(
                    "probe-call",
                    RunItemKind::ToolCall(ToolCall::new(CallId::new("probe"), "probe", json!({}))),
                ),
                unparsed_transfer("transfer", "handoff"),
            ]),
            ModelResponse::new(vec![message("done", "reviewed")]),
        ]);
        let config = registry(&source, &target);
        let first = Runner::run(run_request(
            Arc::clone(&source),
            Arc::clone(&target),
            &model,
            config.clone(),
        ))
        .await
        .unwrap();
        let label = format!("{declared:?}");
        let pending = first.state().pending_handoff().expect("the transfer waits");
        let RunItemKind::HandoffCall(call) = pending.calls()[0].kind() else {
            panic!("{label}: expected a handoff call");
        };
        assert_eq!(call.unparsed_arguments(), Some("{bad"), "{label}");

        let mut checkpoint: RunState =
            serde_json::from_str(&serde_json::to_string(first.state()).unwrap()).unwrap();
        let ra_runtime::runner::RunOutcome::Interrupted { items } = first.outcome() else {
            panic!("{label}: expected an approval, got {:?}", first.outcome());
        };
        checkpoint.approve(&items[0], false).unwrap();
        let resumed = Runner::run(
            run_request(Arc::clone(&source), Arc::clone(&target), &model, config)
                .with_state(checkpoint),
        )
        .await;
        assert_eq!(probe_calls.load(Ordering::SeqCst), 1, "{label}");
        if declared.reads_payload() {
            let error = resumed.unwrap_err();
            assert_eq!(error.code(), "provider.behavior", "{label}: {error}");
            assert_eq!(calls.load(Ordering::SeqCst), 0, "{label}");
        } else {
            let resumed = resumed.unwrap();
            assert_eq!(resumed.last_agent().id(), target.id(), "{label}");
            let expected = usize::from(matches!(declared, Declared::CallbackWithoutInput));
            assert_eq!(calls.load(Ordering::SeqCst), expected, "{label}");
        }
    }
}
