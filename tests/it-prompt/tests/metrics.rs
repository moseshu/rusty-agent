use ra_core::usage::Usage;
use ra_prompt::metrics::{
    calculate_cache_hit_rate, calculate_cache_hit_rate_from_usage, meets_cache_hit_target,
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
    let usage = Usage::new(1000, 200).with_cached_input_tokens(800);

    let rate = calculate_cache_hit_rate_from_usage(&usage);
    assert_eq!(rate, 0.8);
    assert!(meets_cache_hit_target(&usage, 0.7));
    assert!(!meets_cache_hit_target(&usage, 0.85));
}
