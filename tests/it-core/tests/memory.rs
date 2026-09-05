//! The long-term memory contract: what it asks for, what it answers, and what it refuses to model.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    memory::{
        MemoryAnchor, MemoryBudget, MemoryCursor, MemoryExcerpt, MemoryHit, MemoryHits,
        MemoryListRequest, MemoryListing, MemoryMatchMode, MemoryReadRequest, MemoryRecord,
        MemoryRecordId, MemoryRecordKind, MemorySearchRequest, MemoryStore, MemoryStoreError,
    },
};

fn budget() -> MemoryBudget {
    MemoryBudget::new(10, 4096)
}

fn record_id(value: &str) -> MemoryRecordId {
    MemoryRecordId::new(value)
}

/// A store that records what it was handed and answers with fixtures.
///
/// It exists to prove the trait is usable behind an `Arc<dyn MemoryStore>` and that a request
/// arrives at an implementation the way the builder assembled it. It is not a reference
/// implementation.
#[derive(Default)]
struct FixtureStore {
    last_search: Mutex<Option<MemorySearchRequest>>,
}

#[async_trait]
impl MemoryStore for FixtureStore {
    async fn list(&self, request: MemoryListRequest) -> Result<MemoryListing> {
        if request.scope() == Some(&record_id("missing")) {
            return Err(MemoryStoreError::NotFound {
                record: record_id("missing"),
            }
            .into_error("memory_list"));
        }
        Ok(MemoryListing::new(vec![MemoryRecord::new(
            record_id("m1"),
            "MEMORY.md",
            MemoryRecordKind::Record,
        )])
        .continuing_at(MemoryCursor::new("1")))
    }

    async fn read(&self, request: MemoryReadRequest) -> Result<MemoryExcerpt> {
        Ok(
            MemoryExcerpt::new(request.record().clone(), "MEMORY.md", "pins its toolchain")
                .at("lines 12-14"),
        )
    }

    async fn search(&self, request: MemorySearchRequest) -> Result<MemoryHits> {
        *self.last_search.lock().expect("the fixture lock") = Some(request);
        Ok(MemoryHits::new(vec![
            MemoryHit::new(record_id("m1"), "MEMORY.md", "pins its toolchain")
                .at("line 12")
                .continuing_at(MemoryAnchor::new("12")),
        ]))
    }
}

/// Nothing in the contract names a path, a directory, or a line.
///
/// This is the whole correction the module exists in its present form to record. A store backed by
/// rows or embeddings has no directory tree and no line numbering, so a contract that asked for
/// them would force one to be invented — and an invented layout is a fact callers then rely on. The
/// assertion is compile-time: these are the only ways to name things, and none of them is a path.
#[test]
fn test_every_handle_is_opaque_and_store_issued() {
    let record = record_id("row:4821");
    let anchor = MemoryAnchor::new("offset:900");
    let cursor = MemoryCursor::new("page:2");

    assert_eq!(record.as_str(), "row:4821");
    assert_eq!(anchor.as_str(), "offset:900");
    assert_eq!(cursor.as_str(), "page:2");

    // A request names a record by that handle and nothing else: there is no path to resolve, no
    // parent to traverse, and so nothing for a caller to construct that the store did not issue.
    let read = MemoryReadRequest::new(record.clone(), budget()).starting_at(anchor.clone());
    assert_eq!(read.record(), &record);
    assert_eq!(read.anchor(), Some(&anchor));
}

/// What a store shows a caller is the store's own words, and nothing parses them.
///
/// The display string is what lets one contract serve a file store, a database, and a vector index
/// without any of them lying: each says where a result sits in terms that are true of itself.
#[test]
fn test_a_store_describes_a_location_in_its_own_terms() {
    let file = MemoryHit::new(record_id("MEMORY.md"), "MEMORY.md", "text").at("MEMORY.md:12");
    let rows = MemoryHit::new(record_id("42"), "decisions", "text").at("row 4821");
    let chunks = MemoryHit::new(record_id("v7"), "notes", "text").at("chunk 7 of 19");

    assert_eq!(file.location(), Some("MEMORY.md:12"));
    assert_eq!(rows.location(), Some("row 4821"));
    assert_eq!(chunks.location(), Some("chunk 7 of 19"));

    // Absent is legal: a store with nothing useful to say about position says nothing.
    assert!(
        MemoryHit::new(record_id("x"), "x", "text")
            .location()
            .is_none()
    );
}

/// A search asks for text and a budget, and nothing about relevance.
///
/// The defaults are the assertion. Case-insensitive, because the caller is a model paraphrasing
/// something it has not read; `Any`, because a caller that meant a conjunction says so. Neither is
/// a ranking parameter, and the absence is the contract: a `k` here would describe a top-k ranked
/// store, and a lexical one would have to fake the order.
#[test]
fn test_a_search_request_carries_no_notion_of_relevance() {
    let request = MemorySearchRequest::new(vec!["toolchain".to_owned()], budget());

    assert_eq!(request.queries(), ["toolchain"]);
    assert_eq!(request.mode(), MemoryMatchMode::Any);
    assert!(!request.is_case_sensitive());
    assert!(request.scope().is_none());
    assert!(request.cursor().is_none());
}

/// Every option a caller sets survives to the store unchanged.
#[test]
fn test_a_search_request_keeps_what_the_caller_set() {
    let request =
        MemorySearchRequest::new(vec!["pin".to_owned(), "toolchain".to_owned()], budget())
            .matching(MemoryMatchMode::All)
            .under(record_id("decisions"))
            .case_sensitive()
            .from_cursor(MemoryCursor::new("42"));

    assert_eq!(request.mode(), MemoryMatchMode::All);
    assert_eq!(request.scope(), Some(&record_id("decisions")));
    assert!(request.is_case_sensitive());
    assert_eq!(request.cursor().map(MemoryCursor::as_str), Some("42"));
}

/// Both halves of a budget reach the store, and neither may be zero.
///
/// The count and the byte ceiling are independent: a thousand tiny records and one enormous one are
/// the same count and wildly different budgets. Zero is normalized rather than accepted because a
/// budget of nothing describes an operation with no possible useful answer — and because a caller
/// clamping a model's request against a ceiling of zero panics, which is a host's configuration
/// mistake turning into a crashed turn.
#[test]
fn test_a_budget_bounds_both_count_and_bytes_and_is_never_zero() {
    let budget = MemoryBudget::new(25, 8192);
    assert_eq!(budget.items(), 25);
    assert_eq!(budget.bytes(), 8192);

    let degenerate = MemoryBudget::new(0, 0);
    assert_eq!(degenerate.items(), 1);
    assert_eq!(degenerate.bytes(), 1);
}

/// More results exist exactly when a cursor says so, and there is no second flag to disagree.
///
/// A `truncated` boolean beside the cursor would be a second encoding of one fact, and the pair can
/// contradict: a truncated answer carrying no cursor is a dead end nothing can continue from, and a
/// caller cannot tell that from a bug.
#[test]
fn test_an_answer_reports_more_results_only_through_its_cursor() {
    let complete = MemoryListing::new(vec![MemoryRecord::new(
        record_id("a"),
        "a",
        MemoryRecordKind::Record,
    )]);
    assert!(complete.next_cursor().is_none());

    let partial = MemoryListing::new(Vec::new()).continuing_at(MemoryCursor::new("7"));
    assert_eq!(partial.next_cursor().map(MemoryCursor::as_str), Some("7"));

    let hits = MemoryHits::new(Vec::new());
    assert!(hits.next_cursor().is_none());
    assert_eq!(
        hits.continuing_at(MemoryCursor::new("9"))
            .next_cursor()
            .map(MemoryCursor::as_str),
        Some("9")
    );
}

/// A read that stopped short hands back a position, not a number to do arithmetic on.
///
/// This is what replaced line offsets. A caller cannot compute where to resume without knowing the
/// store's layout, so the store says — and a caller that never has to compute one can never compute
/// a wrong one.
#[test]
fn test_an_excerpt_resumes_from_a_position_the_store_issued() {
    let whole = MemoryExcerpt::new(record_id("m1"), "MEMORY.md", "all of it");
    assert!(whole.next_anchor().is_none());

    let partial = MemoryExcerpt::new(record_id("m1"), "MEMORY.md", "the first part")
        .continuing_at(MemoryAnchor::new("offset:900"));
    assert_eq!(
        partial.next_anchor().map(MemoryAnchor::as_str),
        Some("offset:900")
    );
}

/// A hit says where a read of it picks up, so a fragment is somewhere to go.
///
/// A match is a fragment and the paragraph around it is what a caller usually wants next. Without
/// the anchor the only way there is re-reading the record from its start.
#[test]
fn test_a_hit_offers_the_position_a_read_continues_from() {
    let hit = MemoryHit::new(record_id("m1"), "MEMORY.md", "the matching text")
        .continuing_at(MemoryAnchor::new("12"));

    assert_eq!(hit.anchor().map(MemoryAnchor::as_str), Some("12"));
    assert!(
        MemoryHit::new(record_id("m1"), "MEMORY.md", "text")
            .anchor()
            .is_none()
    );
}

/// A store's refusal travels as a value, so the layer above writes the sentence from the value.
#[test]
fn test_a_store_refusal_survives_as_a_typed_cause() {
    let error = MemoryStoreError::NotFound {
        record: record_id("decisions/2026"),
    }
    .into_error("memory_read");

    let recovered = MemoryStoreError::of(&error).expect("the typed cause survives the wrapping");
    assert!(
        matches!(recovered, MemoryStoreError::NotFound { record } if record.as_str() == "decisions/2026")
    );
}

/// A refusal the caller could have avoided is invalid input; only the store failing is not.
#[test]
fn test_refusals_are_classified_by_who_could_have_prevented_them() {
    let correctable = [
        MemoryStoreError::NotFound {
            record: record_id("a"),
        },
        MemoryStoreError::WrongKind {
            record: record_id("a"),
            expected: MemoryRecordKind::Record,
        },
        MemoryStoreError::InvalidCursor {
            cursor: MemoryCursor::new("z"),
        },
        MemoryStoreError::InvalidAnchor {
            anchor: MemoryAnchor::new("z"),
        },
        MemoryStoreError::EmptyQuery,
    ];
    for failure in correctable {
        assert_eq!(
            failure.kind(),
            ToolErrorKind::InvalidInput,
            "`{failure}` is something the caller could have asked differently"
        );
    }

    assert_eq!(
        MemoryStoreError::Unavailable {
            reason: "connection refused".to_owned(),
        }
        .kind(),
        ToolErrorKind::ExecutionFailed
    );
}

/// An unreachable store keeps its reason out of the sentence a model reads.
#[test]
fn test_an_unavailable_store_does_not_narrate_its_own_plumbing() {
    let failure = MemoryStoreError::Unavailable {
        reason: "sqlite: database is locked".to_owned(),
    };

    assert!(
        !failure.to_string().contains("sqlite"),
        "the model-facing sentence carries the plumbing: {failure}"
    );
}

/// The contract is usable behind a trait object, and a request reaches a store as it was built.
#[tokio::test]
async fn test_a_store_is_reachable_behind_a_trait_object() {
    let fixture = Arc::new(FixtureStore::default());
    let store: Arc<dyn MemoryStore> = fixture.clone();

    let found = store
        .search(
            MemorySearchRequest::new(vec!["toolchain".to_owned()], budget())
                .matching(MemoryMatchMode::All),
        )
        .await
        .expect("the fixture answers");
    assert_eq!(found.hits().len(), 1);
    assert_eq!(found.hits()[0].record(), &record_id("m1"));

    let seen = fixture
        .last_search
        .lock()
        .expect("the fixture lock")
        .clone()
        .expect("the store saw the request");
    assert_eq!(seen.mode(), MemoryMatchMode::All);
    assert_eq!(seen.budget().items(), 10);
    assert_eq!(seen.budget().bytes(), 4096);

    let excerpt = store
        .read(MemoryReadRequest::new(record_id("m1"), budget()))
        .await
        .expect("the fixture answers");
    assert_eq!(excerpt.location(), Some("lines 12-14"));

    let listing = store
        .list(MemoryListRequest::new(budget()))
        .await
        .expect("the fixture answers");
    assert_eq!(listing.records()[0].kind(), MemoryRecordKind::Record);
    assert!(listing.next_cursor().is_some());
}

/// A store reports a refusal through the same error channel every other contract uses.
#[tokio::test]
async fn test_a_store_refusal_arrives_as_a_framework_error() {
    let store: Arc<dyn MemoryStore> = Arc::new(FixtureStore::default());

    let error: Error = store
        .list(MemoryListRequest::new(budget()).under(record_id("missing")))
        .await
        .expect_err("the fixture refuses this scope");

    assert!(matches!(
        MemoryStoreError::of(&error),
        Some(MemoryStoreError::NotFound { .. })
    ));
}

#[test]
fn versioned_exposure_survives_tool_output_persistence() {
    use ra_core::memory::{MemoryExposure, MemoryRevision};
    use ra_core::tool::{ObservationMetadata, ToolOutput};
    let one = MemoryExposure::new(
        record_id("row:4821"),
        MemoryRevision::new("v1"),
        Some(MemoryAnchor::new("chunk:7")),
        "first",
    );
    let other = MemoryExposure::new(
        record_id("row:4821"),
        MemoryRevision::new("v2"),
        Some(MemoryAnchor::new("chunk:7")),
        "second",
    );
    assert_ne!(one.token(), other.token());
    let output = ToolOutput::text("first")
        .with_metadata(ObservationMetadata::new().with_memory_exposures(vec![one.clone()]));
    let restored: ToolOutput =
        serde_json::from_value(serde_json::to_value(output).unwrap()).unwrap();
    assert_eq!(restored.metadata().memory_exposures(), &[one]);
}

#[test]
fn encoded_budget_includes_handles_escaping_and_pagination() {
    use ra_core::memory::memory_response_json;
    let page = MemoryExcerpt::new(record_id("row:4821"), "row", "quote: \"\\\"\\n")
        .continuing_at(MemoryAnchor::new("chunk:2"));
    let encoded = memory_response_json(&page).unwrap();
    let restored: MemoryExcerpt = serde_json::from_str(&encoded).unwrap();
    assert_eq!(restored, page);
    assert!(encoded.len() > page.text().len());
}
