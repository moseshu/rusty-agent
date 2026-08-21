//! The `OpenAI` provider family: the Responses and Chat protocols share auth, error mapping, and SSE plumbing.

use std::collections::BTreeMap;

use ra_core::error::{Error, Result};
use reqwest::{
    RequestBuilder,
    header::{HeaderMap, HeaderName, HeaderValue},
};

pub mod auth;
pub mod chat;
pub(crate) mod content;
pub(crate) mod error;
pub mod responses;
pub(crate) mod sse;

use self::auth::OpenAiAuth;

/// Applies endpoint and request transport headers with request values taking precedence.
///
/// `RequestBuilder::header` appends values, which leaves two values on the wire when a caller
/// overrides an endpoint default. Build one map and replace each field instead, preserving the
/// resolved-settings rule that a more specific layer wins. `Authorization` remains owned by the
/// adapter and is intentionally excluded from both maps.
pub(crate) fn apply_transport_headers(
    request: RequestBuilder,
    auth: &OpenAiAuth,
    request_headers: &BTreeMap<String, String>,
) -> Result<RequestBuilder> {
    let mut headers = HeaderMap::new();
    if let Some(organization) = auth.organization() {
        insert_header(&mut headers, "openai-organization", organization)?;
    }
    if let Some(project) = auth.project() {
        insert_header(&mut headers, "openai-project", project)?;
    }
    for (name, value) in auth.default_headers() {
        if !name.eq_ignore_ascii_case("authorization") {
            insert_header(&mut headers, name, value)?;
        }
    }
    for (name, value) in request_headers {
        if !name.eq_ignore_ascii_case("authorization") {
            insert_header(&mut headers, name, value)?;
        }
    }
    Ok(request.headers(headers))
}

/// Inserts one header, replacing an earlier value with the same case-insensitive name.
fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
        Error::caller(format!("OpenAI transport header name `{name}` is invalid"))
            .with_source(error)
    })?;
    let value = HeaderValue::from_str(value).map_err(|error| {
        Error::caller(format!(
            "OpenAI transport header `{name}` has an invalid value"
        ))
        .with_source(error)
    })?;
    headers.insert(name, value);
    Ok(())
}
