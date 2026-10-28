// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! HTTP transport for curated plugin startup sync.
//!
//! Both proxy modes retain the supplied application network policy. The default proxy mode also
//! keeps the legacy fallback for invalid custom CA configuration; system-proxy mode fails closed.
//! Startup-sync request helpers apply the explicit product User-Agent header.

use std::sync::Arc;

use crate::plugins::http_client_selector::HttpClientSelector;
use http::Method;
use ra_http::ClientRouteClass;
use ra_http::HttpClientFactory;
use ra_http::RequestBuilder;
use ra_http::RouteAwareClientPool;

pub(super) struct StartupSyncHttpClient(Arc<dyn HttpClientSelector>);

impl StartupSyncHttpClient {
    pub(super) fn new(http_client_factory: &HttpClientFactory) -> Self {
        let http_clients = RouteAwareClientPool::new_without_request_logging(
            http_client_factory.clone(),
            ClientRouteClass::Api,
        )
        .with_legacy_custom_ca_fallback();
        Self(Arc::new(http_clients))
    }

    #[cfg(feature = "test-api")]
    pub(super) fn route_aware(clients: Arc<dyn HttpClientSelector>) -> Self {
        Self(clients)
    }

    pub(super) fn request(&self, method: Method, url: &str) -> RequestBuilder {
        self.0.request(method, url)
    }
}
