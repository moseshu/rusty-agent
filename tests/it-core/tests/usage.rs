//! Per-request usage detail and the ledger it accumulates into.

use ra_core::usage::{REQUEST_USAGE_SCHEMA_VERSION, RequestUsage, USAGE_SCHEMA_VERSION, Usage};
use serde_json::json;

fn request(input: u64, output: u64, cached: u64, cache_write: u64, reasoning: u64) -> RequestUsage {
    RequestUsage::new(input, output)
        .with_cached_input_tokens(cached)
        .with_cache_write_tokens(cache_write)
        .with_reasoning_tokens(reasoning)
}

/// The details are parts of the counters they belong to, never additions to them: a total that
/// added cached input on top of input would charge the cheapest tokens twice.
#[test]
fn a_request_reports_details_as_subsets_of_the_counters_they_belong_to() {
    let usage = request(1_000, 200, 800, 120, 60);

    assert_eq!(usage.schema_version(), REQUEST_USAGE_SCHEMA_VERSION);
    assert_eq!(usage.input_tokens(), 1_000);
    assert_eq!(usage.output_tokens(), 200);
    assert_eq!(usage.total_tokens(), 1_200);
    assert_eq!(usage.cached_input_tokens(), 800);
    assert_eq!(usage.cache_write_tokens(), 120);
    assert_eq!(usage.reasoning_tokens(), 60);
}

#[test]
fn a_ledger_built_from_one_request_counts_it_and_keeps_its_entry() {
    let ledger = Usage::from_request(request(1_000, 200, 800, 120, 60));

    assert_eq!(ledger.schema_version(), USAGE_SCHEMA_VERSION);
    assert_eq!(ledger.requests(), 1);
    assert_eq!(ledger.input_tokens(), 1_000);
    assert_eq!(ledger.total_tokens(), 1_200);
    assert_eq!(ledger.request_usage_entries().len(), 1);
    assert_eq!(ledger.request_usage_entries()[0].cached_input_tokens(), 800);
}

/// An empty ledger is the honest description of a run that has not called a model yet, which is
/// why there is no separate "absent" state for it.
#[test]
fn an_empty_ledger_reports_no_requests_and_no_tokens() {
    let ledger = Usage::default();

    assert_eq!(ledger.requests(), 0);
    assert_eq!(ledger.total_tokens(), 0);
    assert!(ledger.request_usage_entries().is_empty());
}

/// The aggregate is what a budget meters; the entries are what a cost report reads. Summation
/// destroys the second, which is the whole reason both are kept.
#[test]
fn accumulating_sums_the_counters_and_appends_every_entry_in_order() {
    let ledger = Usage::from_request(request(100_000, 500, 0, 100_000, 0))
        .accumulate(&Usage::from_request(request(150_000, 900, 99_000, 0, 400)))
        .accumulate(&Usage::from_request(request(80_000, 300, 79_000, 0, 100)));

    assert_eq!(ledger.requests(), 3);
    assert_eq!(ledger.input_tokens(), 330_000);
    assert_eq!(ledger.output_tokens(), 1_700);
    assert_eq!(ledger.total_tokens(), 331_700);
    assert_eq!(ledger.cached_input_tokens(), 178_000);
    assert_eq!(ledger.cache_write_tokens(), 100_000);
    assert_eq!(ledger.reasoning_tokens(), 500);

    let per_request: Vec<u64> = ledger
        .request_usage_entries()
        .iter()
        .map(RequestUsage::input_tokens)
        .collect();
    assert_eq!(per_request, vec![100_000, 150_000, 80_000]);
}

/// Only the entries can tell a cold first call followed by a warm prefix from a prefix that is
/// broken every turn. Both read as roughly half the input cached in the aggregate.
#[test]
fn only_the_entries_can_locate_which_call_missed_the_cache() {
    let warming_up = Usage::from_request(request(1_000, 10, 0, 1_000, 0))
        .accumulate(&Usage::from_request(request(1_000, 10, 1_000, 0, 0)));
    let breaking_every_turn = Usage::from_request(request(1_000, 10, 500, 500, 0))
        .accumulate(&Usage::from_request(request(1_000, 10, 500, 500, 0)));

    assert_eq!(
        warming_up.cached_input_tokens(),
        breaking_every_turn.cached_input_tokens()
    );

    let first_call_hit_rate = |ledger: &Usage| {
        let entry = &ledger.request_usage_entries()[0];
        entry.cached_input_tokens() * 100 / entry.input_tokens()
    };
    assert_eq!(first_call_hit_rate(&warming_up), 0);
    assert_eq!(first_call_hit_rate(&breaking_every_turn), 50);
}

/// A totals-only projection still describes every token and every request it summarizes. Dropping
/// the entries is what keeps a record that restates the totals repeatedly from growing with the
/// square of the session.
#[test]
fn dropping_the_entries_keeps_every_total() {
    let ledger = Usage::from_request(request(10, 2, 4, 6, 1))
        .accumulate(&Usage::from_request(request(20, 3, 8, 0, 2)));
    let totals = ledger.without_entries();

    assert!(totals.request_usage_entries().is_empty());
    assert_eq!(totals.requests(), ledger.requests());
    assert_eq!(totals.input_tokens(), ledger.input_tokens());
    assert_eq!(totals.output_tokens(), ledger.output_tokens());
    assert_eq!(totals.cached_input_tokens(), ledger.cached_input_tokens());
    assert_eq!(totals.cache_write_tokens(), ledger.cache_write_tokens());
    assert_eq!(totals.reasoning_tokens(), ledger.reasoning_tokens());

    // And a projection still accumulates: a resumed run carries totals it can no longer itemize
    // and must keep counting from them rather than starting over.
    let continued = totals.accumulate(&Usage::from_request(request(5, 1, 0, 0, 0)));
    assert_eq!(continued.requests(), 3);
    assert_eq!(continued.input_tokens(), 35);
    assert_eq!(continued.request_usage_entries().len(), 1);
}

#[test]
fn a_ledger_round_trips_through_json_with_its_entries() {
    let ledger = Usage::from_request(request(10, 2, 4, 6, 1))
        .accumulate(&Usage::from_request(request(20, 3, 8, 0, 2)));

    let encoded = serde_json::to_value(&ledger).unwrap();
    assert_eq!(encoded["requests"], json!(2));
    assert_eq!(
        encoded["request_usage_entries"].as_array().unwrap().len(),
        2
    );

    let restored: Usage = serde_json::from_value(encoded).unwrap();
    assert_eq!(restored, ledger);
}

/// A record written before per-request accounting existed carries totals and nothing else. It stays
/// readable and keeps counting, because the alternative — refusing it, or treating its totals as
/// zero — turns an old session into either an error or a free one.
#[test]
fn a_record_without_per_request_detail_still_reports_its_totals() {
    let legacy = json!({
        "schema_version": 1,
        "input_tokens": 4_000,
        "output_tokens": 900,
        "cached_input_tokens": 3_500,
        "cache_write_tokens": 0,
        "reasoning_tokens": 100
    });

    let restored: Usage = serde_json::from_value(legacy).unwrap();

    assert_eq!(restored.requests(), 0);
    assert!(restored.request_usage_entries().is_empty());
    assert_eq!(restored.total_tokens(), 4_900);
    assert_eq!(restored.cached_input_tokens(), 3_500);

    let continued = restored.accumulate(&Usage::from_request(request(100, 10, 0, 0, 0)));
    assert_eq!(continued.total_tokens(), 5_010);
    assert_eq!(continued.requests(), 1);
}

/// Unknown counters are retained rather than summed: they are opaque values here, and a total that
/// dropped what this build does not recognize would describe less than the requests it came from.
#[test]
fn unknown_counters_are_retained_and_the_later_observation_wins() {
    let mut first = serde_json::to_value(Usage::from_request(request(10, 2, 0, 0, 0))).unwrap();
    first["audio_tokens"] = json!(5);
    let mut second = serde_json::to_value(Usage::from_request(request(20, 3, 0, 0, 0))).unwrap();
    second["audio_tokens"] = json!(9);
    second["video_tokens"] = json!(1);

    let first: Usage = serde_json::from_value(first).unwrap();
    let second: Usage = serde_json::from_value(second).unwrap();
    let merged = first.accumulate(&second);

    assert_eq!(merged.input_tokens(), 30);
    assert_eq!(merged.unknown().get("audio_tokens"), Some(&json!(9)));
    assert_eq!(merged.unknown().get("video_tokens"), Some(&json!(1)));
    assert_eq!(
        serde_json::to_value(&merged).unwrap()["audio_tokens"],
        json!(9)
    );
}

/// A provider reporting nonsense should skew a total, not panic a debug build.
#[test]
fn counters_saturate_instead_of_overflowing() {
    let ledger = Usage::from_request(RequestUsage::new(u64::MAX, u64::MAX))
        .accumulate(&Usage::from_request(RequestUsage::new(10, 10)));

    assert_eq!(ledger.input_tokens(), u64::MAX);
    assert_eq!(ledger.total_tokens(), u64::MAX);
    assert_eq!(ledger.requests(), 2);
}

/// A request whose cost the endpoint never reported is still a request that was made. Compatible
/// endpoints routinely omit the usage block, and a ledger that skipped those calls would report a
/// run as having made fewer than it did.
#[test]
fn a_request_that_reported_nothing_is_still_counted() {
    let ledger = Usage::from_request(RequestUsage::default());

    assert_eq!(ledger.requests(), 1);
    assert_eq!(ledger.total_tokens(), 0);
    assert_eq!(ledger.request_usage_entries().len(), 1);
}
