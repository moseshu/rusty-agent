//! Memory tools preserve budgeted pages and expose versioned evidence without usage writes.
use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    error::{BudgetKind, Error, GuardrailStage, Result, ToolErrorKind},
    item::{AgentId, CallId},
    memory::{
        MemoryAnchor, MemoryBudget, MemoryCursor, MemoryExcerpt, MemoryHit, MemoryHits,
        MemoryListRequest, MemoryListing, MemoryReadRequest, MemoryRecord, MemoryRecordId,
        MemoryRecordKind, MemoryRevision, MemorySearchRequest, MemoryStore, MemoryStoreError,
        memory_response_json,
    },
    state::RunId,
    tool::{Tool, ToolContext, ToolOutput},
};
use ra_tools::memory::{MemoryLimits, MemoryListTool, MemoryReadTool, MemorySearchTool};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn id() -> MemoryRecordId {
    MemoryRecordId::new("row:4821")
}
fn excerpt() -> MemoryExcerpt {
    MemoryExcerpt::new(id(), "A \"record\"", "first\nsecond\nEND")
        .at("chunk 7/19")
        .with_revision(MemoryRevision::new("v1"))
}
fn hits() -> MemoryHits {
    MemoryHits::new(vec![
        MemoryHit::new(id(), "row", "snippet")
            .continuing_at(MemoryAnchor::new("opaque:chunk:7"))
            .with_revision(MemoryRevision::new("v1")),
    ])
    .continuing_at(MemoryCursor::new("opaque:page:2"))
}
fn listing() -> MemoryListing {
    MemoryListing::new(vec![MemoryRecord::new(
        id(),
        "row",
        MemoryRecordKind::Record,
    )])
    .continuing_at(MemoryCursor::new("opaque:page:2"))
}
#[derive(Default)]
struct FixtureStore {
    seen: Mutex<Vec<MemoryBudget>>,
    anchors: Mutex<Vec<Option<MemoryAnchor>>>,
    oversized: bool,
    too_many: bool,
    unavailable: bool,
    untyped_failure: bool,
}
#[async_trait]
impl MemoryStore for FixtureStore {
    async fn list(&self, request: MemoryListRequest) -> Result<MemoryListing> {
        self.seen.lock().unwrap().push(request.budget());
        Ok(if self.too_many {
            MemoryListing::new(vec![listing().records()[0].clone(); 2])
        } else {
            listing()
        })
    }
    async fn read(&self, request: MemoryReadRequest) -> Result<MemoryExcerpt> {
        self.seen.lock().unwrap().push(request.budget());
        self.anchors.lock().unwrap().push(request.anchor().cloned());
        if self.unavailable {
            return Err(MemoryStoreError::Unavailable {
                reason: "private connection".into(),
            }
            .into_error("memory_read"));
        }
        if self.untyped_failure {
            return Err(ra_core::error::Error::config(
                "pool exhausted on memory-primary.internal:5432",
            ));
        }
        Ok(if self.oversized {
            MemoryExcerpt::new(id(), "huge", "。".repeat(20_000))
        } else {
            excerpt()
        })
    }
    async fn search(&self, request: MemorySearchRequest) -> Result<MemoryHits> {
        self.seen.lock().unwrap().push(request.budget());
        Ok(if self.too_many {
            MemoryHits::new(vec![hits().hits()[0].clone(); 2])
        } else {
            hits()
        })
    }
}
async fn call(tool: &dyn Tool, args: Value) -> ToolOutput {
    let agent = AgentSpec::builder()
        .id(AgentId::new("probe"))
        .name("Probe")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("memory-test"), &agent);
    let call_id = CallId::new("memory-call");
    match tool
        .call(ToolContext::new(&run, tool, &call_id, &args))
        .await
    {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&ToolContext::new(&run, tool, &call_id, &args), &error)
            .await
            .unwrap()
            .unwrap(),
    }
}
#[tokio::test]
async fn exact_encoded_budget_preserves_entire_read() {
    let size = memory_response_json(&excerpt()).unwrap().len();
    let tool = MemoryReadTool::new(Arc::new(FixtureStore::default()))
        .unwrap()
        .with_limits(MemoryLimits::new().with_max_bytes(size));
    let output = call(&tool, json!({"record":id()})).await;
    assert_eq!(output.as_text().unwrap().len(), size);
    assert_eq!(
        serde_json::from_str::<MemoryExcerpt>(output.as_text().unwrap()).unwrap(),
        excerpt()
    );
    assert_eq!(output.metadata().memory_exposures().len(), 1);
}
#[tokio::test]
async fn search_anchor_is_visible_and_passes_to_read() {
    let store = Arc::new(FixtureStore::default());
    let tool = MemorySearchTool::new(store.clone()).unwrap().with_limits(
        MemoryLimits::new().with_max_bytes(memory_response_json(&hits()).unwrap().len()),
    );
    let output = call(&tool, json!({"queries":["snippet"]})).await;
    let page: MemoryHits = serde_json::from_str(output.as_text().unwrap()).unwrap();
    assert_eq!(page.next_cursor().unwrap().as_str(), "opaque:page:2");
    let hit = &page.hits()[0];
    call(
        &MemoryReadTool::new(store.clone()).unwrap(),
        json!({"record": hit.record(), "anchor": hit.anchor()}),
    )
    .await;
    assert_eq!(
        store.anchors.lock().unwrap()[0],
        Some(MemoryAnchor::new("opaque:chunk:7"))
    );
}
#[tokio::test]
async fn listing_at_exact_budget_retains_cursor_without_exposure() {
    let tool = MemoryListTool::new(Arc::new(FixtureStore::default()))
        .unwrap()
        .with_limits(
            MemoryLimits::new().with_max_bytes(memory_response_json(&listing()).unwrap().len()),
        );
    let output = call(&tool, json!({})).await;
    assert_eq!(
        serde_json::from_str::<MemoryListing>(output.as_text().unwrap()).unwrap(),
        listing()
    );
    assert!(output.metadata().memory_exposures().is_empty());
}
#[tokio::test]
async fn oversized_response_is_refused_without_false_evidence() {
    let tool = MemoryReadTool::new(Arc::new(FixtureStore {
        oversized: true,
        ..Default::default()
    }))
    .unwrap()
    .with_limits(MemoryLimits::new().with_max_bytes(64));
    let output = call(&tool, json!({"record":id()})).await;
    let text = output.as_text().unwrap();
    // No page reached the model, and none of the oversized content did either. What is asserted is
    // that no *page* was delivered — not that the refusal itself fits the page ceiling. A refusal
    // measured against that ceiling has to be empty once the ceiling is small, and an empty text
    // block is one a provider may drop and Anthropic refuses outright.
    assert!(!text.is_empty(), "a refusal with an empty body");
    assert!(!text.contains('。'), "oversized content leaked: {text}");
    assert!(text.contains("exceeded its response budget"), "{text}");
    assert!(output.metadata().memory_exposures().is_empty());
    assert!(output.metadata().guidance().join(" ").contains("budget"));
}
#[tokio::test]
async fn zero_limits_do_not_claim_empty_memory() {
    let store = Arc::new(FixtureStore::default());
    let limits = MemoryLimits::new()
        .with_max_hits(0)
        .with_max_records(0)
        .with_max_bytes(0);
    assert_eq!(limits.max_bytes(), 1);
    let tools: Vec<(Box<dyn Tool>, Value)> = vec![
        (
            Box::new(
                MemorySearchTool::new(store.clone())
                    .unwrap()
                    .with_limits(limits),
            ),
            json!({"queries":["x"]}),
        ),
        (
            Box::new(
                MemoryReadTool::new(store.clone())
                    .unwrap()
                    .with_limits(limits),
            ),
            json!({"record":id()}),
        ),
        (
            Box::new(
                MemoryListTool::new(store.clone())
                    .unwrap()
                    .with_limits(limits),
            ),
            json!({}),
        ),
    ];
    for (tool, args) in tools {
        let output = call(tool.as_ref(), args).await;
        let text = output.as_text().unwrap();
        // A byte ceiling of one admits no page at all, so every entry refuses. It must refuse in
        // words rather than with an empty body, and it must not report the refusal as an empty
        // store — a run told memory is empty stops asking, and this store is not.
        assert!(!text.is_empty(), "a refusal with an empty body");
        assert!(text.contains("exceeded its response budget"), "{text}");
        assert!(!output.metadata().guidance().join(" ").contains("empty"));
    }
    assert!(
        store
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|b| b.bytes() == 1 && b.items() == 1)
    );
}
#[tokio::test]
async fn model_count_is_clamped_to_host_budget() {
    let store = Arc::new(FixtureStore::default());
    let tool = MemorySearchTool::new(store.clone())
        .unwrap()
        .with_limits(MemoryLimits::new().with_max_hits(3));
    call(&tool, json!({"queries":["x"],"max_hits":999})).await;
    assert_eq!(store.seen.lock().unwrap()[0].items(), 3);
}

#[tokio::test]
async fn backend_count_overrun_is_refused_without_evidence() {
    let store = Arc::new(FixtureStore {
        too_many: true,
        ..Default::default()
    });
    let tools: Vec<(Box<dyn Tool>, Value)> = vec![
        (
            Box::new(MemorySearchTool::new(store.clone()).unwrap()),
            json!({"queries":["x"],"max_hits":1}),
        ),
        (
            Box::new(MemoryListTool::new(store).unwrap()),
            json!({"max_records":1}),
        ),
    ];
    for (tool, args) in tools {
        let output = call(tool.as_ref(), args).await;
        let text = output.as_text().unwrap();
        assert!(!text.is_empty(), "a refusal with an empty body");
        assert!(text.contains("exceeded its response budget"), "{text}");
        assert!(output.metadata().memory_exposures().is_empty());
        assert!(output.metadata().guidance().join(" ").contains("budget"));
    }
}
#[tokio::test]
async fn failures_are_actionable_and_hide_backend_details() {
    let tool = MemoryReadTool::new(Arc::new(FixtureStore {
        unavailable: true,
        ..Default::default()
    }))
    .unwrap();
    let output = call(&tool, json!({"record":id()})).await;
    assert!(!output.as_text().unwrap().contains("private connection"));
    assert!(
        output
            .metadata()
            .guidance()
            .join(" ")
            .contains("Continue without memory")
    );
    let output = call(&tool, json!({"recrod":id()})).await;
    assert!(output.as_text().unwrap().contains("Invalid arguments"));
}

/// Pages a UTF-8 record using store-issued byte anchors and exact encoded response sizes.
struct PagedStore(String);
#[async_trait]
impl MemoryStore for PagedStore {
    async fn list(&self, _: MemoryListRequest) -> Result<MemoryListing> {
        Ok(listing())
    }
    async fn search(&self, _: MemorySearchRequest) -> Result<MemoryHits> {
        Ok(hits())
    }
    async fn read(&self, request: MemoryReadRequest) -> Result<MemoryExcerpt> {
        let start = request.anchor().map_or(0, |a| a.as_str().parse().unwrap());
        let mut best = None;
        for end in (start + 1)..=self.0.len() {
            if !self.0.is_char_boundary(end) {
                continue;
            }
            let mut page = MemoryExcerpt::new(id(), "row", &self.0[start..end]);
            if end < self.0.len() {
                page = page.continuing_at(MemoryAnchor::new(end.to_string()));
            }
            if memory_response_json(&page)?.len() <= request.budget().bytes() {
                best = Some(page);
            }
        }
        best.ok_or_else(|| MemoryStoreError::BudgetTooSmall.into_error("memory_read"))
    }
}
#[tokio::test]
async fn successive_budgeted_reads_reconstruct_unicode_record_without_loss() {
    let text = "中文\n\"quoted\"\\content ".repeat(30);
    let tool = MemoryReadTool::new(Arc::new(PagedStore(text.clone())))
        .unwrap()
        .with_limits(MemoryLimits::new().with_max_bytes(180));
    let mut anchor = None;
    let mut recovered = String::new();
    for _ in 0..100 {
        let output = call(&tool, json!({"record":id(),"anchor":anchor})).await;
        assert!(output.as_text().unwrap().len() <= 180);
        let page: MemoryExcerpt = serde_json::from_str(output.as_text().unwrap()).unwrap();
        assert!(!page.text().is_empty());
        recovered.push_str(page.text());
        anchor = page.next_anchor().cloned();
        if anchor.is_none() {
            break;
        }
    }
    assert_eq!(recovered, text);
}

/// A store that reports a failure in its own vocabulary still answers the model in words.
///
/// `MemoryStore` is a third-party extension point, so a backend returning something other than a
/// `MemoryStoreError` is routine rather than a framework bug. These entries take
/// `ToolFailureHandling::Custom`, where declining to handle a failure propagates it and ends the
/// turn — so without a fallback an unrecognised store error takes down a whole run over a memory
/// lookup the run can finish without. The sentence is generic on purpose: the store's own message
/// is developer-facing text this crate did not write and may name a host or a connection.
#[tokio::test]
async fn an_untyped_backend_failure_is_answered_rather_than_fatal() {
    let tool = MemoryReadTool::new(Arc::new(FixtureStore {
        untyped_failure: true,
        ..Default::default()
    }))
    .unwrap();

    let output = call(&tool, json!({"record":id()})).await;
    let text = output.as_text().unwrap();

    assert!(!text.is_empty(), "the turn would have ended instead");
    assert!(
        !text.contains("memory-primary.internal"),
        "the backend's plumbing reached the model: {text}"
    );
    assert!(
        output
            .metadata()
            .guidance()
            .join(" ")
            .contains("Continue without memory"),
        "{:?}",
        output.metadata().guidance()
    );
}

#[tokio::test]
async fn memory_tools_preserve_control_errors_instead_of_returning_observations() {
    let store = Arc::new(FixtureStore::default());
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(MemoryReadTool::new(store.clone()).unwrap()),
        Box::new(MemorySearchTool::new(store.clone()).unwrap()),
        Box::new(MemoryListTool::new(store).unwrap()),
    ];
    let errors = [
        Error::budget(BudgetKind::Tokens, "token budget exhausted"),
        Error::guardrail(GuardrailStage::ToolOutput, "memory-policy", "stop"),
        Error::cancelled("user stopped the run"),
        Error::tool(ToolErrorKind::Cancelled, "memory", "backend cancelled"),
        Error::tool(ToolErrorKind::Cancelled, "memory", "backend cancelled").with_source(
            MemoryStoreError::Unavailable {
                reason: "connection closed".into(),
            },
        ),
    ];
    let agent = AgentSpec::builder()
        .id(AgentId::new("probe"))
        .name("Probe")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("memory-controls"), &agent);
    let call_id = CallId::new("memory-call");
    let args = json!({});
    for tool in tools {
        for error in &errors {
            let output = tool
                .handle_failure(
                    &ToolContext::new(&run, tool.as_ref(), &call_id, &args),
                    error,
                )
                .await
                .unwrap();
            assert!(
                output.is_none(),
                "{} swallowed {}",
                tool.schema().name(),
                error.code()
            );
        }
    }
}
