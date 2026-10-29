// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(missing_docs, reason = "Preserve upstream configuration records.")]
//! Network-domain wildcards over UTF-8 bytes, independent of filesystem semantics.
//! Construction validates and normalizes patterns and expands domain prefixes.
//! Callers normalize candidate hosts before matching. No IDNA conversion is performed.
//! Byte matching preserves the historical globset wildcard semantics.

use std::net::IpAddr;
/// Normalize host fragments for policy matching (trim whitespace, strip ports/brackets, lowercase).
pub(super) fn normalize_host(host: &str) -> String {
    let host = host.trim();
    if host.starts_with('[')
        && let Some(end) = host.find(']')
    {
        return normalize_dns_host_or_ip_literal(&host[1..end]);
    }

    // The proxy stack should typically hand us a host without a port, but be
    // defensive and strip `:port` when there is exactly one `:`.
    if host.bytes().filter(|b| *b == b':').count() == 1 {
        let host = host.split(':').next().unwrap_or_default();
        return normalize_dns_host_or_ip_literal(host);
    }

    // Avoid mangling unbracketed IPv6 literals, but strip trailing dots so fully qualified domain
    // names are treated the same as their dotless variants.
    normalize_dns_host_or_ip_literal(host)
}

fn normalize_dns_host_or_ip_literal(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    if let Some(ip) = normalize_ip_literal(host) {
        return ip;
    }
    host.to_string()
}

fn normalize_ip_literal(host: &str) -> Option<String> {
    if host.parse::<IpAddr>().is_ok() {
        return Some(host.to_string());
    }
    for delimiter in ["%25", "%"] {
        if let Some((ip, scope)) = host.split_once(delimiter)
            && ip.parse::<IpAddr>().is_ok()
        {
            return Some(format!("{ip}%{scope}"));
        }
    }
    None
}

fn normalize_pattern(pattern: &str) -> String {
    let pattern = pattern.trim();
    if pattern == "*" {
        return "*".to_string();
    }

    let (prefix, remainder) = if let Some(domain) = pattern.strip_prefix("**.") {
        ("**.", domain)
    } else if let Some(domain) = pattern.strip_prefix("*.") {
        ("*.", domain)
    } else {
        ("", pattern)
    };

    let remainder = normalize_host(remainder);
    if prefix.is_empty() {
        remainder
    } else {
        format!("{prefix}{remainder}")
    }
}

pub(crate) fn is_global_wildcard_domain_pattern(pattern: &str) -> bool {
    let normalized = normalize_pattern(pattern);
    expand_domain_pattern(&normalized)
        .iter()
        .any(|candidate| candidate == "*")
}

fn expand_domain_pattern(pattern: &str) -> Vec<String> {
    let pattern = pattern.trim();
    let Some(domain) = pattern
        .strip_prefix("**.")
        .or_else(|| pattern.strip_prefix("*."))
    else {
        return vec![pattern.to_string()];
    };
    let domain = domain.trim();
    if domain.is_empty() {
        return vec![String::new()];
    }
    if pattern.starts_with("**.") {
        vec![domain.to_string(), format!("?*.{domain}")]
    } else {
        vec![format!("?*.{domain}")]
    }
}
