// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use crate::config::NetworkMode;

use super::*;

use pretty_assertions::assert_eq;

#[test]
fn method_allowed_full_allows_everything() {
    assert!(NetworkMode::Full.allows_method("GET"));
    assert!(NetworkMode::Full.allows_method("POST"));
    assert!(NetworkMode::Full.allows_method("CONNECT"));
}

#[test]
fn method_allowed_limited_allows_only_safe_methods() {
    assert!(NetworkMode::Limited.allows_method("GET"));
    assert!(NetworkMode::Limited.allows_method("HEAD"));
    assert!(NetworkMode::Limited.allows_method("OPTIONS"));
    assert!(!NetworkMode::Limited.allows_method("POST"));
    assert!(!NetworkMode::Limited.allows_method("CONNECT"));
}

#[test]
fn compile_globset_normalizes_trailing_dots() {
    let set = compile_denylist(&["Example.COM.".to_string()]).unwrap();

    assert_eq!(true, set.is_match("example.com"));
    assert_eq!(false, set.is_match("api.example.com"));
}

#[test]
fn compile_globset_normalizes_wildcards() {
    let set = compile_denylist(&["*.Example.COM.".to_string()]).unwrap();

    assert_eq!(true, set.is_match("api.example.com"));
    assert_eq!(false, set.is_match("example.com"));
}

#[test]
fn compile_globset_supports_mid_label_wildcards() {
    let set = compile_denylist(&["region*.v2.argotunnel.com".to_string()]).unwrap();

    assert_eq!(true, set.is_match("region1.v2.argotunnel.com"));
    assert_eq!(true, set.is_match("region.v2.argotunnel.com"));
    assert_eq!(false, set.is_match("xregion1.v2.argotunnel.com"));
    assert_eq!(false, set.is_match("foo.region1.v2.argotunnel.com"));
}

// Keep this table one-for-one with the browser network-policy matchers'
// question-mark contract cases so grammar changes are checked on both sides.
#[test]
fn compile_globset_supports_question_mark_wildcards() -> Result<()> {
    for (pattern, host, expected) in [
        ("api?.example.com", "api1.example.com", true),
        ("api?.example.com", "api.example.com", false),
        ("api?.example.com", "api12.example.com", false),
        ("api??.example.com", "api12.example.com", true),
        ("api??.example.com", "api1.example.com", false),
        ("api*?.example.com", "api.example.com", false),
        ("api*?.example.com", "api1.example.com", true),
        ("api*?.example.com", "api123.example.com", true),
        ("api?example.com", "api.example.com", true),
        ("*.api?.example.com", "api1.example.com", false),
        ("*.api?.example.com", "www.api1.example.com", true),
        ("*.api?.example.com", "nested.www.api1.example.com", true),
        ("*.api?.example.com", "www.api12.example.com", false),
        ("**.api?.example.com", "api1.example.com", true),
        ("**.api?.example.com", "www.api1.example.com", true),
        ("**.api?.example.com", "nested.www.api1.example.com", true),
        ("**.api?.example.com", "api12.example.com", false),
        ("**.api?.example.com", "www.api12.example.com", false),
        (" API?.EXAMPLE.COM. ", "API1.EXAMPLE.COM.", true),
    ] {
        let patterns = [pattern.to_string()];
        for set in [compile_allowlist(&patterns)?, compile_denylist(&patterns)?] {
            assert_eq!(
                set.is_match(normalize_host(host)),
                expected,
                "pattern {pattern}, host {host}"
            );
        }
    }
    Ok(())
}

#[test]
fn compile_globset_normalizes_apex_and_subdomains() {
    let set = compile_denylist(&["**.Example.COM.".to_string()]).unwrap();

    assert_eq!(true, set.is_match("example.com"));
    assert_eq!(true, set.is_match("api.example.com"));
}

#[test]
fn compile_globset_normalizes_bracketed_ipv6_literals() {
    let set = compile_denylist(&["[::1]".to_string()]).unwrap();

    assert_eq!(true, set.is_match("::1"));
}

#[test]
fn compile_globset_preserves_scoped_ipv6_literals() {
    let set = compile_denylist(&["[fe80::1%25lo0]".to_string()]).unwrap();

    assert_eq!(true, set.is_match("fe80::1%lo0"));
    assert_eq!(false, set.is_match("fe80::1%lo1"));
    assert_eq!(false, set.is_match("fe80::1"));
}

#[test]
fn is_loopback_host_handles_localhost_variants() {
    assert!(is_loopback_host(&Host::parse("localhost").unwrap()));
    assert!(is_loopback_host(&Host::parse("localhost.").unwrap()));
    assert!(is_loopback_host(&Host::parse("LOCALHOST").unwrap()));
    assert!(!is_loopback_host(&Host::parse("notlocalhost").unwrap()));
}

#[test]
fn is_loopback_host_handles_ip_literals() {
    assert!(is_loopback_host(&Host::parse("127.0.0.1").unwrap()));
    assert!(is_loopback_host(&Host::parse("::1").unwrap()));
    assert!(!is_loopback_host(&Host::parse("1.2.3.4").unwrap()));
}

#[test]
fn is_non_public_ip_rejects_private_and_loopback_ranges() {
    assert!(is_non_public_ip("127.0.0.1".parse().unwrap()));
    assert!(is_non_public_ip("10.0.0.1".parse().unwrap()));
    assert!(is_non_public_ip("192.168.0.1".parse().unwrap()));
    assert!(is_non_public_ip("100.64.0.1".parse().unwrap()));
    assert!(is_non_public_ip("192.0.0.1".parse().unwrap()));
    assert!(is_non_public_ip("192.0.2.1".parse().unwrap()));
    assert!(is_non_public_ip("198.18.0.1".parse().unwrap()));
    assert!(is_non_public_ip("198.51.100.1".parse().unwrap()));
    assert!(is_non_public_ip("203.0.113.1".parse().unwrap()));
    assert!(is_non_public_ip("240.0.0.1".parse().unwrap()));
    assert!(is_non_public_ip("0.1.2.3".parse().unwrap()));
    assert!(!is_non_public_ip("8.8.8.8".parse().unwrap()));

    assert!(is_non_public_ip("::ffff:127.0.0.1".parse().unwrap()));
    assert!(is_non_public_ip("::ffff:10.0.0.1".parse().unwrap()));
    assert!(!is_non_public_ip("::ffff:8.8.8.8".parse().unwrap()));

    assert!(is_non_public_ip("::1".parse().unwrap()));
    assert!(is_non_public_ip("fe80::1".parse().unwrap()));
    assert!(is_non_public_ip("fc00::1".parse().unwrap()));
}
