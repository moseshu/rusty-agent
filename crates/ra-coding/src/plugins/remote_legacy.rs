// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Public featured-plugin fetch; the source account-auth branch is intentionally absent.
#![allow(
    missing_docs,
    reason = "Retain the source featured fetch errors and operation."
)]

use super::remote::RemotePluginServiceConfig;
use http::{Method, StatusCode};
use ra_protocol::server::protocol::Product;
use std::time::Duration;
use url::Url;

const REMOTE_FEATURED_PLUGIN_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum RemotePluginFetchError {
    #[error("invalid chatgpt base url for remote featured plugin request: {0}")]
    InvalidBaseUrl(#[source] url::ParseError),

    #[error("failed to send remote featured plugin request to {url}: {source}")]
    Request {
        url: String,
        #[source]
        source: ra_http::HttpError,
    },

    #[error("remote featured plugin request to {url} failed with status {status}: {body}")]
    UnexpectedStatus {
        url: String,
        status: StatusCode,
        body: String,
    },

    #[error("failed to parse remote featured plugin response from {url}: {source}")]
    Decode {
        url: String,
        #[source]
        source: serde_json::Error,
    },
}

pub async fn fetch_remote_featured_plugin_ids(
    config: &RemotePluginServiceConfig,
    product: Option<Product>,
) -> Result<Vec<String>, RemotePluginFetchError> {
    let base_url = config.chatgpt_base_url.trim_end_matches('/');
    let mut url = Url::parse(&format!("{base_url}/plugins/featured"))
        .map_err(RemotePluginFetchError::InvalidBaseUrl)?;
    url.query_pairs_mut().append_pair(
        "platform",
        product.unwrap_or(Product::Codex).to_app_platform(),
    );
    let url = url.to_string();
    let request = config
        .http_request(Method::GET, &url)
        .timeout(REMOTE_FEATURED_PLUGIN_FETCH_TIMEOUT);

    let response = request
        .send()
        .await
        .map_err(|source| RemotePluginFetchError::Request {
            url: url.clone(),
            source,
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(RemotePluginFetchError::UnexpectedStatus { url, status, body });
    }

    serde_json::from_str(&body).map_err(|source| RemotePluginFetchError::Decode {
        url: url.clone(),
        source,
    })
}
