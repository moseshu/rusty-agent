// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Host-key normalization used by product network-domain composition.

use std::net::IpAddr;

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
