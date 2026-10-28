// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Preserve source plugin management records and operations."
)]
use http::Method;
use ra_http::RequestBuilder;
use ra_http::RouteAwareClientPool;
use std::fmt::Debug;

/// Builds requests whose URL is also used to resolve their outbound route.
///
/// Implementations must keep route selection coupled to the request URL. Returning a transport
/// client would let callers send a different URL than the one used for route selection.
pub(crate) trait HttpClientSelector: Debug + Send + Sync {
    fn request(&self, method: Method, url: &str) -> RequestBuilder;
}

impl HttpClientSelector for RouteAwareClientPool {
    fn request(&self, method: Method, url: &str) -> RequestBuilder {
        RouteAwareClientPool::request(self, method, url)
    }
}
