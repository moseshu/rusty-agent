use ra_core::usage::{RequestUsage, Usage};
use ra_prompt::metrics::{
    calculate_cache_hit_rate, calculate_cache_hit_rate_from_request,
    calculate_cache_hit_rate_from_usage, meets_cache_hit_target,
};

#[test]
fn test_cache_hit_rate_calculation() {
    assert_eq!(calculate_cache_hit_rate(0, 0), 0.0);
    assert_eq!(calculate_cache_hit_rate(0, 1000), 0.0);
    assert_eq!(calculate_cache_hit_rate(500, 1000), 0.5);
    assert_eq!(calculate_cache_hit_rate(750, 1000), 0.75);
    assert_eq!(calculate_cache_hit_rate(1000, 1000), 1.0);
}

#[test]
fn test_cache_hit_rate_from_usage_record() {
    let usage = Usage::from_request(RequestUsage::new(1000, 200).with_cached_input_tokens(800));

    let rate = calculate_cache_hit_rate_from_usage(&usage);
    assert_eq!(rate, 0.8);
    assert!(meets_cache_hit_target(&usage, 0.7));
    assert!(!meets_cache_hit_target(&usage, 0.85));
}

/// A run-wide rate says whether a run was cheap; a per-request rate says which call broke the
/// prefix. Averaging hides the difference between a cold start and a prefix invalidated every turn.
#[test]
fn test_cache_hit_rate_is_answerable_for_a_single_request() {
    let cold_start = RequestUsage::new(1000, 50);
    let warm = RequestUsage::new(1200, 40).with_cached_input_tokens(1000);
    let run = Usage::from_request(cold_start.clone()).accumulate(&Usage::from_request(warm.clone()));

    assert_eq!(calculate_cache_hit_rate_from_request(&cold_start), 0.0);
    assert!(calculate_cache_hit_rate_from_request(&warm) > 0.83);

    let per_request: Vec<f64> = run
        .request_usage_entries()
        .iter()
        .map(calculate_cache_hit_rate_from_request)
        .collect();
    assert_eq!(per_request.len(), 2);
    assert_eq!(per_request[0], 0.0);
    assert!(calculate_cache_hit_rate_from_usage(&run) > per_request[0]);
    assert!(calculate_cache_hit_rate_from_usage(&run) < per_request[1]);
}
