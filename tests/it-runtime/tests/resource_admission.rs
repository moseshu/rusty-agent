use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    context::RunContext,
    error::Result,
    item::{AgentId, CallId, ItemId, ModelResponse, RunItem, RunItemKind, ToolCall},
    state::{RunId, ToolFailureTracker, ToolUseTracker},
    tool::{
        ResourceClaim, ResourceId, Tool, ToolConcurrency, ToolContext, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    turn::{TurnSettlementRequest, prepare::TurnActionSurface, settle_turn},
};
use serde_json::json;

struct TestClaimTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    entered: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    completed: Arc<AtomicUsize>,
}

impl TestClaimTool {
    fn new(
        name: &str,
        claims: Vec<ResourceClaim>,
        entered: Arc<AtomicUsize>,
        release: Arc<AtomicBool>,
        completed: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            options: ToolOptions::new()
                .with_concurrency(ToolConcurrency::Parallel)
                .with_resource_claims(claims),
            entered,
            release,
            completed,
        }
    }

    fn legacy(
        name: &str,
        concurrency: ToolConcurrency,
        entered: Arc<AtomicUsize>,
        release: Arc<AtomicBool>,
        completed: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            options: ToolOptions::new().with_concurrency(concurrency),
            entered,
            release,
            completed,
        }
    }
}

#[async_trait]
impl Tool for TestClaimTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("done"))
    }
}

struct DynamicTestClaimTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    entered: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    completed: Arc<AtomicUsize>,
}

impl DynamicTestClaimTool {
    fn new(
        name: &str,
        entered: Arc<AtomicUsize>,
        release: Arc<AtomicBool>,
        completed: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            origin: ToolOrigin::new(name).unwrap(),
            schema: ToolSchema::new(
                name,
                json!({
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string" }
                    },
                    "required": ["workspace"],
                    "additionalProperties": false
                }),
            )
            .unwrap(),
            entered,
            release,
            completed,
        }
    }
}

#[async_trait]
impl Tool for DynamicTestClaimTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_concurrency(ToolConcurrency::Parallel)
    }

    async fn resource_claims(&self, context: &ToolContext<'_>) -> Result<Vec<ResourceClaim>> {
        let ws_name = context
            .arguments()
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default");
        let ws = ResourceId::workspace(ws_name.to_owned())?;
        Ok(vec![ResourceClaim::exclusive(ws)])
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("dynamic_done"))
    }
}

fn binding() -> AgentBinding {
    AgentBinding::direct(
        AgentSpec::builder()
            .id(AgentId::new("resource-admission-agent"))
            .name("Resource Admission Agent")
            .build()
            .unwrap(),
    )
}

fn run_context() -> Arc<RunContext> {
    let agent = AgentSpec::builder()
        .id(AgentId::new("resource-admission-agent"))
        .name("Resource Admission Agent")
        .build()
        .unwrap();
    Arc::new(RunContext::new(
        RunId::new("run-resource-admission"),
        agent.as_ref(),
    ))
}

fn surface(tools: Vec<Arc<dyn Tool>>) -> TurnActionSurface {
    TurnActionSurface::new(tools, Vec::new()).unwrap()
}

fn tool_call(item_id: &str, call_id: &str, name: &str, args: serde_json::Value) -> RunItem {
    RunItem::new(
        ItemId::new(item_id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, args)),
    )
}

fn output_for<'a>(items: &'a [RunItem], call_id: &str) -> &'a ra_core::item::ToolCallOutput {
    items
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCallOutput(output) if output.call_id().as_str() == call_id => {
                Some(output)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("call `{call_id}` has no matching output"))
}

async fn wait_for_count(counter: &AtomicUsize, target: usize) {
    tokio::time::timeout(Duration::from_millis(500), async {
        while counter.load(Ordering::SeqCst) < target {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("counter did not reach {target} within timeout"));
}

#[tokio::test]
async fn test_identical_exclusive_claims_serialize() {
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let ws = ResourceId::workspace("repo-a").unwrap();
    let tool_1: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "tool_exclusive_1",
        vec![ResourceClaim::exclusive(ws.clone())],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));
    let tool_2: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "tool_exclusive_2",
        vec![ResourceClaim::exclusive(ws)],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![tool_1, tool_2]);
        let response = ModelResponse::new(vec![
            tool_call("item-1", "call-1", "tool_exclusive_1", json!({})),
            tool_call("item-2", "call-2", "tool_exclusive_2", json!({})),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        let mut failure_tracker = ToolFailureTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run_context(),
            &cancel,
            &mut tracker,
            &mut failure_tracker,
            Default::default(),
        ))
        .await
    });

    // Wait until one tool enters
    wait_for_count(&entered, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Second exclusive tool MUST NOT enter while first holds the exclusive claim
    assert_eq!(entered.load(Ordering::SeqCst), 1);
    assert_eq!(completed.load(Ordering::SeqCst), 0);

    // Release first tool
    release.store(true, Ordering::SeqCst);

    task.await
        .expect("settlement joins")
        .expect("both settle successfully");

    assert_eq!(entered.load(Ordering::SeqCst), 2);
    assert_eq!(completed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_identical_shared_claims_run_in_parallel() {
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let ws = ResourceId::workspace("repo-shared").unwrap();
    let tool_1: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "read_tool_1",
        vec![ResourceClaim::shared(ws.clone())],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));
    let tool_2: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "read_tool_2",
        vec![ResourceClaim::shared(ws)],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![tool_1, tool_2]);
        let response = ModelResponse::new(vec![
            tool_call("item-1", "call-1", "read_tool_1", json!({})),
            tool_call("item-2", "call-2", "read_tool_2", json!({})),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        let mut failure_tracker = ToolFailureTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run_context(),
            &cancel,
            &mut tracker,
            &mut failure_tracker,
            Default::default(),
        ))
        .await
    });

    // Both shared tools should enter concurrently before release
    wait_for_count(&entered, 2).await;
    assert_eq!(completed.load(Ordering::SeqCst), 0);

    release.store(true, Ordering::SeqCst);

    task.await
        .expect("settlement joins")
        .expect("both settle successfully");

    assert_eq!(completed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_disjoint_exclusive_claims_run_in_parallel() {
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let ws_a = ResourceId::workspace("repo-alpha").unwrap();
    let ws_b = ResourceId::workspace("repo-beta").unwrap();

    let tool_a: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "write_alpha",
        vec![ResourceClaim::exclusive(ws_a)],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));
    let tool_b: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "write_beta",
        vec![ResourceClaim::exclusive(ws_b)],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![tool_a, tool_b]);
        let response = ModelResponse::new(vec![
            tool_call("item-1", "call-1", "write_alpha", json!({})),
            tool_call("item-2", "call-2", "write_beta", json!({})),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        let mut failure_tracker = ToolFailureTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run_context(),
            &cancel,
            &mut tracker,
            &mut failure_tracker,
            Default::default(),
        ))
        .await
    });

    // Because resources are disjoint (alpha != beta), both write tools enter concurrently!
    wait_for_count(&entered, 2).await;
    assert_eq!(completed.load(Ordering::SeqCst), 0);

    release.store(true, Ordering::SeqCst);

    task.await
        .expect("settlement joins")
        .expect("both settle successfully");

    assert_eq!(completed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_deadlock_prevention_on_inverted_resource_order() {
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let res_1 = ResourceId::workspace("res-1").unwrap();
    let res_2 = ResourceId::workspace("res-2").unwrap();

    // Tool A claims (res_1, res_2), Tool B claims (res_2, res_1)
    let tool_a: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "tool_a",
        vec![
            ResourceClaim::exclusive(res_1.clone()),
            ResourceClaim::exclusive(res_2.clone()),
        ],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));
    let tool_b: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "tool_b",
        vec![
            ResourceClaim::exclusive(res_2),
            ResourceClaim::exclusive(res_1),
        ],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![tool_a, tool_b]);
        let response = ModelResponse::new(vec![
            tool_call("item-1", "call-1", "tool_a", json!({})),
            tool_call("item-2", "call-2", "tool_b", json!({})),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        let mut failure_tracker = ToolFailureTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run_context(),
            &cancel,
            &mut tracker,
            &mut failure_tracker,
            Default::default(),
        ))
        .await
    });

    // Exactly one should acquire both permits and enter
    wait_for_count(&entered, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(entered.load(Ordering::SeqCst), 1);

    release.store(true, Ordering::SeqCst);

    task.await
        .expect("settlement joins")
        .expect("both settle without deadlock");

    assert_eq!(entered.load(Ordering::SeqCst), 2);
    assert_eq!(completed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_legacy_tools_compatibility_with_claims() {
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let ws = ResourceId::workspace("workspace-x").unwrap();

    // Legacy exclusive tool (no claims, default exclusive)
    let legacy_exclusive: Arc<dyn Tool> = Arc::new(TestClaimTool::legacy(
        "legacy_exclusive",
        ToolConcurrency::Exclusive,
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    // Claim-based tool
    let claimed_tool: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "claimed_tool",
        vec![ResourceClaim::exclusive(ws)],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![legacy_exclusive, claimed_tool]);
        let response = ModelResponse::new(vec![
            tool_call("item-1", "call-1", "legacy_exclusive", json!({})),
            tool_call("item-2", "call-2", "claimed_tool", json!({})),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        let mut failure_tracker = ToolFailureTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run_context(),
            &cancel,
            &mut tracker,
            &mut failure_tracker,
            Default::default(),
        ))
        .await
    });

    // Legacy exclusive and claimed tool cannot overlap
    wait_for_count(&entered, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(entered.load(Ordering::SeqCst), 1);

    release.store(true, Ordering::SeqCst);

    task.await.expect("settlement joins").expect("both settle");

    assert_eq!(entered.load(Ordering::SeqCst), 2);
    assert_eq!(completed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_dynamic_resource_claims_execution() {
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let dyn_tool: Arc<dyn Tool> = Arc::new(DynamicTestClaimTool::new(
        "dyn_workspace_tool",
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    // Two calls targeting different dynamic workspaces should run concurrently
    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![dyn_tool]);
        let response = ModelResponse::new(vec![
            tool_call(
                "item-1",
                "call-1",
                "dyn_workspace_tool",
                json!({ "workspace": "ws-1" }),
            ),
            tool_call(
                "item-2",
                "call-2",
                "dyn_workspace_tool",
                json!({ "workspace": "ws-2" }),
            ),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        let mut failure_tracker = ToolFailureTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run_context(),
            &cancel,
            &mut tracker,
            &mut failure_tracker,
            Default::default(),
        ))
        .await
    });

    // Both calls target disjoint dynamic workspaces (ws-1 != ws-2) so both enter in parallel
    wait_for_count(&entered, 2).await;
    release.store(true, Ordering::SeqCst);

    task.await
        .expect("settlement joins")
        .expect("dynamic calls settle");

    assert_eq!(completed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_mixed_shared_and_exclusive_on_same_resource() {
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let ws = ResourceId::workspace("shared-repo").unwrap();
    let reader: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "reader_tool",
        vec![ResourceClaim::shared(ws.clone())],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));
    let writer: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "writer_tool",
        vec![ResourceClaim::exclusive(ws)],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![reader, writer]);
        let response = ModelResponse::new(vec![
            tool_call("item-1", "call-1", "reader_tool", json!({})),
            tool_call("item-2", "call-2", "writer_tool", json!({})),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        let mut failure_tracker = ToolFailureTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run_context(),
            &cancel,
            &mut tracker,
            &mut failure_tracker,
            Default::default(),
        ))
        .await
    });

    // Reader and writer targeting the same resource cannot execute simultaneously
    wait_for_count(&entered, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(entered.load(Ordering::SeqCst), 1);

    release.store(true, Ordering::SeqCst);

    task.await.expect("settlement joins").expect("both settle");

    assert_eq!(entered.load(Ordering::SeqCst), 2);
    assert_eq!(completed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_exclusive_tool_serializes_against_parallel_claim_tools() {
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let ws = ResourceId::workspace("shared-repo").unwrap();
    // Tool 1 declares Exclusive concurrency (no fine-grained claims)
    let tool_1: Arc<dyn Tool> = Arc::new(TestClaimTool::legacy(
        "exclusive_tool",
        ToolConcurrency::Exclusive,
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    let tool_2: Arc<dyn Tool> = Arc::new(TestClaimTool::new(
        "parallel_tool",
        vec![ResourceClaim::shared(ws)],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    let task = tokio::spawn(async move {
        let binding = binding();
        let surface = surface(vec![tool_1, tool_2]);
        let response = ModelResponse::new(vec![
            tool_call("item-1", "call-1", "exclusive_tool", json!({})),
            tool_call("item-2", "call-2", "parallel_tool", json!({})),
        ]);
        let cancel = CancelScope::root();
        let mut tracker = ToolUseTracker::new();
        let mut failure_tracker = ToolFailureTracker::new();
        settle_turn(TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run_context(),
            &cancel,
            &mut tracker,
            &mut failure_tracker,
            Default::default(),
        ))
        .await
    });

    // Exclusive tool serializes: only 1 tool enters
    wait_for_count(&entered, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(entered.load(Ordering::SeqCst), 1);

    release.store(true, Ordering::SeqCst);

    task.await.expect("settlement joins").expect("both settle");

    assert_eq!(entered.load(Ordering::SeqCst), 2);
    assert_eq!(completed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_dynamic_claim_failure_does_not_abort_turn() {
    let release = Arc::new(AtomicBool::new(true));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let dyn_tool: Arc<dyn Tool> = Arc::new(DynamicTestClaimTool::new(
        "dyn_tool",
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    ));

    let binding = binding();
    let surface = surface(vec![dyn_tool]);
    let response = ModelResponse::new(vec![
        // Invalid workspace argument with trailing space causes ResourceId validation failure
        tool_call(
            "item-1",
            "call-1",
            "dyn_tool",
            json!({ "workspace": "invalid " }),
        ),
        // Valid workspace argument
        tool_call(
            "item-2",
            "call-2",
            "dyn_tool",
            json!({ "workspace": "valid_repo" }),
        ),
    ]);
    let cancel = CancelScope::root();
    let mut tracker = ToolUseTracker::new();
    let mut failure_tracker = ToolFailureTracker::new();

    let execution = settle_turn(TurnSettlementRequest::new(
        &binding,
        &response,
        &surface,
        run_context(),
        &cancel,
        &mut tracker,
        &mut failure_tracker,
        Default::default(),
    ))
    .await
    .expect("turn completes successfully without aborting");

    // The second call successfully executed
    assert_eq!(completed.load(Ordering::SeqCst), 1);

    let first_output = output_for(execution.new_step_items(), "call-1");
    let second_output = output_for(execution.new_step_items(), "call-2");

    assert!(first_output.is_error());
    assert!(!second_output.is_error());
}

#[tokio::test]
async fn test_cancellation_releases_resource_claims() {
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let ws = ResourceId::workspace("releasing-repo").unwrap();
    // Tool 1 has a 20ms timeout, claims exclusive access to ws, and never finishes on its own
    let mut tool_1 = TestClaimTool::new(
        "timed_out_tool",
        vec![ResourceClaim::exclusive(ws.clone())],
        Arc::clone(&entered),
        Arc::new(AtomicBool::new(false)),
        Arc::clone(&completed),
    );
    tool_1.options = ToolOptions::new()
        .with_concurrency(ToolConcurrency::Parallel)
        .with_resource_claim(ResourceClaim::exclusive(ws.clone()))
        .with_timeout(Duration::from_millis(20))
        .with_timeout_behavior(ra_core::tool::ToolTimeoutBehavior::ModelVisible);
    let tool_1: Arc<dyn Tool> = Arc::new(tool_1);

    // Tool 2 claims exclusive access to the same ws and finishes immediately
    let tool_2 = TestClaimTool::new(
        "waiting_tool",
        vec![ResourceClaim::exclusive(ws)],
        Arc::clone(&entered),
        Arc::new(AtomicBool::new(true)),
        Arc::clone(&completed),
    );
    let tool_2: Arc<dyn Tool> = Arc::new(tool_2);

    let binding = binding();
    let surface = surface(vec![tool_1, tool_2]);
    let response = ModelResponse::new(vec![
        tool_call("item-1", "call-1", "timed_out_tool", json!({})),
        tool_call("item-2", "call-2", "waiting_tool", json!({})),
    ]);
    let cancel = CancelScope::root();
    let mut tracker = ToolUseTracker::new();
    let mut failure_tracker = ToolFailureTracker::new();

    let execution = settle_turn(TurnSettlementRequest::new(
        &binding,
        &response,
        &surface,
        run_context(),
        &cancel,
        &mut tracker,
        &mut failure_tracker,
        Default::default(),
    ))
    .await
    .expect("turn completes");

    // Both tools entered execution (tool 2 was unblocked when tool 1 timed out and released locks)
    assert_eq!(entered.load(Ordering::SeqCst), 2);
    // Tool 2 completed successfully
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(output_for(execution.new_step_items(), "call-1").is_error());
    assert!(!output_for(execution.new_step_items(), "call-2").is_error());
}

#[tokio::test]
async fn test_approval_does_not_hold_resource_locks() {
    let release = Arc::new(AtomicBool::new(true));
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let ws = ResourceId::workspace("approval-repo").unwrap();

    // Tool 1 requires approval and claims exclusive access on ws
    let mut tool_1 = TestClaimTool::new(
        "approval_tool",
        vec![ResourceClaim::exclusive(ws.clone())],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    );
    tool_1.options = ToolOptions::new()
        .with_concurrency(ToolConcurrency::Parallel)
        .with_approval(ra_core::tool::ToolApprovalPolicy::Always)
        .with_resource_claim(ResourceClaim::exclusive(ws.clone()));
    let tool_1: Arc<dyn Tool> = Arc::new(tool_1);

    // Tool 2 is a parallel tool that claims shared access on ws
    let tool_2 = TestClaimTool::new(
        "parallel_tool",
        vec![ResourceClaim::shared(ws)],
        Arc::clone(&entered),
        Arc::clone(&release),
        Arc::clone(&completed),
    );
    let tool_2: Arc<dyn Tool> = Arc::new(tool_2);

    let binding = binding();
    let surface = surface(vec![tool_1, tool_2]);
    let response = ModelResponse::new(vec![
        tool_call("item-1", "call-1", "approval_tool", json!({})),
        tool_call("item-2", "call-2", "parallel_tool", json!({})),
    ]);
    let cancel = CancelScope::root();
    let mut tracker = ToolUseTracker::new();
    let mut failure_tracker = ToolFailureTracker::new();

    let execution = settle_turn(TurnSettlementRequest::new(
        &binding,
        &response,
        &surface,
        run_context(),
        &cancel,
        &mut tracker,
        &mut failure_tracker,
        Default::default(),
    ))
    .await
    .expect("turn completes with interruption");

    // Tool 1 produced an interruption awaiting approval
    match execution.next_step() {
        ra_core::step::NextStep::Interruption { items } => {
            assert_eq!(items.len(), 1);
        }
        other => panic!("expected Interruption next_step, got {other:?}"),
    }
    // Tool 2 was NOT blocked by tool 1 and completed successfully
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(!output_for(execution.new_step_items(), "call-2").is_error());
}

struct SlotTrackingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    active_approval: Arc<AtomicUsize>,
    peak_approval: Arc<AtomicUsize>,
    active_claims: Arc<AtomicUsize>,
    peak_claims: Arc<AtomicUsize>,
    active_call: Arc<AtomicUsize>,
    peak_call: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for SlotTrackingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn needs_approval(&self, _context: &ToolContext<'_>) -> Result<bool> {
        let cur = self.active_approval.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_approval.fetch_max(cur, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(15)).await;
        self.active_approval.fetch_sub(1, Ordering::SeqCst);
        Ok(false)
    }

    async fn resource_claims(&self, _context: &ToolContext<'_>) -> Result<Vec<ResourceClaim>> {
        let cur = self.active_claims.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_claims.fetch_max(cur, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(15)).await;
        self.active_claims.fetch_sub(1, Ordering::SeqCst);
        let ws = ResourceId::workspace("test-ws")?;
        Ok(vec![ResourceClaim::shared(ws)])
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        let cur = self.active_call.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_call.fetch_max(cur, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(15)).await;
        self.active_call.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::text("done"))
    }
}

#[tokio::test]
async fn test_concurrency_slot_limits_needs_approval_and_claims() {
    let active_approval = Arc::new(AtomicUsize::new(0));
    let peak_approval = Arc::new(AtomicUsize::new(0));
    let active_claims = Arc::new(AtomicUsize::new(0));
    let peak_claims = Arc::new(AtomicUsize::new(0));
    let active_call = Arc::new(AtomicUsize::new(0));
    let peak_call = Arc::new(AtomicUsize::new(0));

    let tool = SlotTrackingTool {
        origin: ToolOrigin::new("slot_tool").unwrap(),
        schema: ToolSchema::new(
            "slot_tool",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .unwrap(),
        options: ToolOptions::new()
            .with_concurrency(ToolConcurrency::Parallel)
            .with_approval(ra_core::tool::ToolApprovalPolicy::Dynamic),
        active_approval: Arc::clone(&active_approval),
        peak_approval: Arc::clone(&peak_approval),
        active_claims: Arc::clone(&active_claims),
        peak_claims: Arc::clone(&peak_claims),
        active_call: Arc::clone(&active_call),
        peak_call: Arc::clone(&peak_call),
    };
    let tool: Arc<dyn Tool> = Arc::new(tool);

    let binding = binding();
    let surface = surface(vec![tool]);
    let response = ModelResponse::new(vec![
        tool_call("item-1", "call-1", "slot_tool", json!({})),
        tool_call("item-2", "call-2", "slot_tool", json!({})),
        tool_call("item-3", "call-3", "slot_tool", json!({})),
        tool_call("item-4", "call-4", "slot_tool", json!({})),
        tool_call("item-5", "call-5", "slot_tool", json!({})),
        tool_call("item-6", "call-6", "slot_tool", json!({})),
    ]);
    let cancel = CancelScope::root();
    let mut tracker = ToolUseTracker::new();
    let mut failure_tracker = ToolFailureTracker::new();

    let execution = settle_turn(
        TurnSettlementRequest::new(
            &binding,
            &response,
            &surface,
            run_context(),
            &cancel,
            &mut tracker,
            &mut failure_tracker,
            Default::default(),
        )
        .with_max_function_tool_concurrency(2),
    )
    .await
    .expect("turn completes");

    assert_eq!(
        execution
            .new_step_items()
            .iter()
            .filter(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(_)))
            .count(),
        6
    );
    // Peak concurrency in third-party hooks and call() must be strictly bounded by max_function_tool_concurrency (2)
    assert!(peak_approval.load(Ordering::SeqCst) <= 2);
    assert!(peak_claims.load(Ordering::SeqCst) <= 2);
    assert!(peak_call.load(Ordering::SeqCst) <= 2);
}

#[tokio::test]
async fn test_cancellation_of_exclusive_holder_unblocks_waiting_claims() {
    let entered_1 = Arc::new(AtomicBool::new(false));
    let entered_2 = Arc::new(AtomicBool::new(false));
    let completed_2 = Arc::new(AtomicBool::new(false));

    let ws = ResourceId::workspace("cancelled-holder-repo").unwrap();

    struct BlockedHolderTool {
        origin: ToolOrigin,
        schema: ToolSchema,
        options: ToolOptions,
        entered: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for BlockedHolderTool {
        fn origin(&self) -> &ToolOrigin {
            &self.origin
        }
        fn schema(&self) -> &ToolSchema {
            &self.schema
        }
        fn options(&self) -> ToolOptions {
            self.options.clone()
        }
        async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
            self.entered.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(ToolOutput::text("holder done"))
        }
    }

    struct WaitingTool {
        origin: ToolOrigin,
        schema: ToolSchema,
        options: ToolOptions,
        entered: Arc<AtomicBool>,
        completed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for WaitingTool {
        fn origin(&self) -> &ToolOrigin {
            &self.origin
        }
        fn schema(&self) -> &ToolSchema {
            &self.schema
        }
        fn options(&self) -> ToolOptions {
            self.options.clone()
        }
        async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
            self.entered.store(true, Ordering::SeqCst);
            self.completed.store(true, Ordering::SeqCst);
            Ok(ToolOutput::text("waiter done"))
        }
    }

    let tool_1 = BlockedHolderTool {
        origin: ToolOrigin::new("holder_tool").unwrap(),
        schema: ToolSchema::new(
            "holder_tool",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .unwrap(),
        options: ToolOptions::new()
            .with_concurrency(ToolConcurrency::Parallel)
            .with_resource_claim(ResourceClaim::exclusive(ws.clone())),
        entered: Arc::clone(&entered_1),
    };
    let tool_2 = WaitingTool {
        origin: ToolOrigin::new("waiting_tool").unwrap(),
        schema: ToolSchema::new(
            "waiting_tool",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .unwrap(),
        options: ToolOptions::new()
            .with_concurrency(ToolConcurrency::Parallel)
            .with_resource_claim(ResourceClaim::exclusive(ws)),
        entered: Arc::clone(&entered_2),
        completed: Arc::clone(&completed_2),
    };

    let cancel = CancelScope::root();
    let cancel_for_task = cancel.clone();
    let entered_for_task = Arc::clone(&entered_1);

    tokio::spawn(async move {
        while !entered_for_task.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel_for_task.cancel(ra_core::cancel::CancelReason::UserInterrupt);
    });

    let binding = binding();
    let surface = surface(vec![Arc::new(tool_1), Arc::new(tool_2)]);
    let response = ModelResponse::new(vec![
        tool_call("item-1", "call-1", "holder_tool", json!({})),
        tool_call("item-2", "call-2", "waiting_tool", json!({})),
    ]);
    let mut tracker = ToolUseTracker::new();
    let mut failure_tracker = ToolFailureTracker::new();

    let result = settle_turn(TurnSettlementRequest::new(
        &binding,
        &response,
        &surface,
        run_context(),
        &cancel,
        &mut tracker,
        &mut failure_tracker,
        Default::default(),
    ))
    .await;

    assert!(result.is_err() || cancel.is_cancelled());
}
