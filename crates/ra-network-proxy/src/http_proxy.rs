// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use rama_http::{HeaderMap, HeaderName, header};
pub(crate) fn remove_hop_by_hop_request_headers(headers: &mut HeaderMap) {
    while let Some(raw_connection) = headers.get(header::CONNECTION).cloned() {
        headers.remove(header::CONNECTION);
        if let Ok(raw_connection) = raw_connection.to_str() {
            let connection_headers: Vec<String> = raw_connection
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            for token in connection_headers {
                if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
                    headers.remove(name);
                }
            }
        }
    }
    for name in [
        &header::KEEP_ALIVE,
        &header::PROXY_CONNECTION,
        &header::PROXY_AUTHORIZATION,
        &header::TRAILER,
        &header::TRANSFER_ENCODING,
        &header::UPGRADE,
    ] {
        headers.remove(name);
    }

    // codespell:ignore te,TE
    // 0x74,0x65 is ASCII "te" (the HTTP TE hop-by-hop header).
    if let Ok(short_hop_header_name) = HeaderName::from_bytes(&[0x74, 0x65]) {
        headers.remove(short_hop_header_name);
    }
}
