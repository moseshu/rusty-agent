//! Compaction is a model-input projection and must not reset runtime control state.

use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use ra_context::compaction::{
    CompactionLimits, CompactionPolicy, anchor::AnchorRetention, project_compacted_model_input,
};
use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result, ToolErrorKind},
    item::{
        AgentId, CallId, ItemId, ModelInputItem, ModelResponse, RunItem, RunItemKind, ToolCall,
    },
    state::{RunId, RunState, ToolOutcome, ToolUse},
    tool::{
        ResourceClaim, ResourceId, Tool, ToolConcurrency, ToolContext, ToolLookupKey, ToolOptions,
        ToolOrigin, ToolOutput, ToolSchema,
    },
    usage::{RequestUsage, Usage},
};
use ra_runtime::{
    agent::AgentBinding,
    turn::{TurnSettlementRequest, prepare::TurnActionSurface, settle_turn},
};
use serde_json::json;

struct FailingClaimTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    calls: Arc<AtomicUsize>,
    claim_evaluations: Arc<AtomicUsize>,
    claim: ResourceClaim,
}

impl FailingClaimTool {
    fn new(calls: Arc<AtomicUsize>, claim_evaluations: Arc<AtomicUsize>) -> Self {
        Self {
            origin: ToolOrigin::new("run_tests").expect("a valid tool origin"),
            schema: ToolSchema::new(
                "run_tests",
                json!({
                    "type": "object",
                    "properties": { "attempt": { "type": "integer" } },
                    "required": ["attempt"],
                    "additionalProperties": false
                }),
            )
            .expect("a valid tool schema"),
            calls,
            claim_evaluations,
            claim: ResourceClaim::exclusive(
                ResourceId::workspace("compaction-control-state")
                    .expect("a valid workspace resource"),
            ),
        }
    }
}

#[async_trait]
impl Tool for FailingClaimTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new()
            .with_concurrency(ToolConcurrency::Parallel)
            .with_max_no_progress_streak(NonZeroU32::new(3).expect("three is non-zero"))
    }

    async fn resource_claims(&self, _context: &ToolContext<'_>) -> Result<Vec<ResourceClaim>> {
        self.claim_evaluations.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.claim.clone()])
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::tool(
            ToolErrorKind::ExecutionFailed,
            self.origin.qualified_name(),
            "the test command still fails",
        ))
    }
}

fn agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .build()
        .expect("a valid agent")
}

fn binding() -> AgentBinding {
    AgentBinding::direct(agent())
}

fn run_context() -> Arc<RunContext> {
    let agent = agent();
    Arc::new(RunContext::new(
        RunId::new("run-compaction-control"),
        &agent,
    ))
}

fn tool_call(call_id: &str, attempt: u64) -> RunItem {
    RunItem::new(
        ItemId::new(format!("item-{call_id}")),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            "run_tests",
            json!({ "attempt": attempt }),
        )),
    )
}

async fn settle_failure(
    surface: &TurnActionSurface,
    state: &mut RunState,
    call_id: &str,
    attempt: u64,
) -> Vec<RunItem> {
    let response = ModelResponse::new(vec![tool_call(call_id, attempt)]);
    let cancel = CancelScope::root();
    let (tool_use, tool_failure) = state.trackers_mut();
    let settled = settle_turn(TurnSettlementRequest::new(
        &binding(),
        &response,
        surface,
        run_context(),
        &cancel,
        tool_use,
        tool_failure,
        Default::default(),
    ))
    .await
    .expect("a model-visible tool failure is a settled turn");
    settled.new_step_items().to_vec()
}

fn identity() -> ToolUse {
    ToolUse::Tool(ToolLookupKey::bare("run_tests").expect("a valid lookup key"))
}

#[tokio::test]
async fn compaction_checkpoint_and_replay_preserve_failure_claim_and_budget_control_state() {
    let calls = Arc::new(AtomicUsize::new(0));
    let claim_evaluations = Arc::new(AtomicUsize::new(0));
    let surface = TurnActionSurface::new(
        vec![Arc::new(FailingClaimTool::new(
            Arc::clone(&calls),
            Arc::clone(&claim_evaluations),
        ))],
        Vec::new(),
    )
    .expect("one tool is a valid surface");
    let mut state = RunState::start(RunId::new("run-compaction-control"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("the first segment initializes the state");
    state.record_usage(&Usage::from_request(RequestUsage::new(800, 120)));

    for (call_id, attempt) in [("call-1", 1), ("call-2", 2)] {
        let items = settle_failure(&surface, &mut state, call_id, attempt).await;
        state.record_generated_items(items);
    }
    let before_compaction = state.clone();
    assert_eq!(
        before_compaction
            .tool_failure()
            .no_progress_streak(&AgentId::new("coder"), &identity()),
        2,
        "the fixture compacts one failure before the breaker threshold"
    );

    let compacted = project_compacted_model_input(
        state.generated_items(),
        CompactionPolicy::new(
            CompactionLimits::new(None, None, Some(4_000)).expect("a total-token trigger"),
            AnchorRetention::new(0, 0, 2).expect("the latest call and observation remain visible"),
        )
        .expect("a total-token trigger converges with this retention"),
        [],
        "The first failed test observation is summarized; the latest remains verbatim.",
    )
    .expect("the older tool observation is compactable");
    assert_eq!(
        compacted.compacted_item_ids(),
        &[ItemId::new("item-call-1"), ItemId::new("call-1.output")]
    );

    // This is the whole point of the fixture, and it is a contrast rather than a single value: the
    // model input the provider would now receive has no trace of the first failure, while the
    // run state still counts it. A projection that reached into `RunState` would make the second
    // half of this follow the first, and the breaker would forget along with the conversation.
    assert_eq!(
        compacted
            .items()
            .iter()
            .filter_map(ModelInputItem::call_id)
            .collect::<Vec<_>>(),
        [&CallId::new("call-2"), &CallId::new("call-2")],
        "the replaced attempt is gone from the model's view of the conversation"
    );
    assert_eq!(
        compacted
            .items()
            .iter()
            .map(ModelInputItem::label)
            .collect::<Vec<_>>(),
        ["compaction", "tool_call", "tool_call_output"],
        "the retained call keeps its result, so the projection is sendable as it stands"
    );
    assert_eq!(
        state
            .tool_failure()
            .no_progress_streak(&AgentId::new("coder"), &identity()),
        2,
        "the breaker still counts the attempt the model can no longer see"
    );
    assert_eq!(state.usage_totals(), before_compaction.usage_totals());
    assert_eq!(
        claim_evaluations.load(Ordering::SeqCst),
        calls.load(Ordering::SeqCst),
        "compaction admitted nothing, so no claim was released or reacquired"
    );

    let checkpoint = serde_json::to_string(&state).expect("the control state checkpoints");
    let mut restored: RunState = serde_json::from_str(&checkpoint).expect("the state resumes");
    assert_eq!(restored.tool_failure(), before_compaction.tool_failure());
    assert_eq!(restored.usage_totals(), before_compaction.usage_totals());
    let before_failure_entry = before_compaction
        .tool_failure()
        .agent(&AgentId::new("coder"))
        .and_then(|failures| failures.entry(&identity()))
        .expect("the pre-compaction failures are tracked")
        .clone();

    // Replaying an already-settled observation after resume must be idempotent. The matching
    // `call_id` is enough to preserve the later streak even if an older projector rewrites its
    // presentation differently.
    restored.trackers_mut().1.record_turn(
        &AgentId::new("coder"),
        [ToolOutcome::failed(
            identity(),
            CallId::new("call-2"),
            &json!({ "attempt": 2 }),
            &json!({ "replayed": true }),
            "tool.execution_failed",
        )],
    );
    assert_eq!(
        restored
            .tool_failure()
            .agent(&AgentId::new("coder"))
            .and_then(|failures| failures.entry(&identity())),
        Some(&before_failure_entry),
        "replay must retain the failure count and every evidence fingerprint"
    );
    assert_eq!(restored.usage_totals(), before_compaction.usage_totals());

    let third = settle_failure(&surface, &mut restored, "call-3", 3).await;
    restored.record_generated_items(third);
    assert_eq!(
        restored
            .tool_failure()
            .no_progress_streak(&AgentId::new("coder"), &identity()),
        3,
        "the failure immediately after compaction still reaches the original threshold"
    );

    let refused = settle_failure(&surface, &mut restored, "call-4", 4).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the fourth call was not re-executed"
    );
    let error_code = refused.iter().find_map(|item| match item.kind() {
        RunItemKind::ToolCallOutput(output) => output.output()["error"]["code"]
            .as_str()
            .map(ToOwned::to_owned),
        _ => None,
    });
    assert_eq!(error_code.as_deref(), Some("tool.no_progress"));
    assert_eq!(restored.usage_totals(), before_compaction.usage_totals());
}
