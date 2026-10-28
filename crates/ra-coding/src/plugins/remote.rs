// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Transport state for public plugin-service metadata, without account authentication.

use super::http_client_selector::HttpClientSelector;
use http::Method;
use ra_http::{ClientRouteClass, HttpClientFactory, RequestBuilder, RouteAwareClientPool};
use std::sync::Arc;

pub(crate) const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/";

/// The upstream service endpoint and shared application-policy-aware HTTP transport.
/// Account identity, product SKU, cookies and backend mutations are not installed here.
#[derive(Debug, Clone)]
pub struct RemotePluginServiceConfig {
    /// Base URL retained for compatibility with the source configuration key.
    pub chatgpt_base_url: String,
    pub(crate) http_clients: Arc<dyn HttpClientSelector>,
}

impl RemotePluginServiceConfig {
    /// Creates service transport using the application's outbound policy and route resolution.
    #[must_use]
    pub fn new(chatgpt_base_url: String, http_client_factory: HttpClientFactory) -> Self {
        Self {
            chatgpt_base_url,
            http_clients: Arc::new(RouteAwareClientPool::new_without_request_logging(
                http_client_factory,
                ClientRouteClass::Api,
            )),
        }
    }

    pub(crate) fn http_request(&self, method: Method, url: &str) -> RequestBuilder {
        self.http_clients
            .request(method, url)
            .header("originator", "rusty-agent")
            .header(
                "user-agent",
                concat!("rusty-agent/", env!("CARGO_PKG_VERSION")),
            )
    }
}

impl PartialEq for RemotePluginServiceConfig {
    fn eq(&self, other: &Self) -> bool {
        self.chatgpt_base_url == other.chatgpt_base_url
    }
}
impl Eq for RemotePluginServiceConfig {}
