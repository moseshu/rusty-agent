//! Responses compact transport sharing provider authentication and error mapping.

use std::collections::BTreeMap;

use ra_core::error::{Error, Result};
use reqwest::Url;
use serde_json::{Value, json};

use super::OpenAiResponsesCompactionMode;
use crate::openai::{
    apply_transport_headers,
    auth::OpenAiAuth,
    error::{ResponseFacts, behavior_error, response_failure, transport_error},
};

#[derive(Clone)]
pub(super) struct CompactionClient {
    auth: OpenAiAuth,
    http: reqwest::Client,
    url: Url,
}

impl CompactionClient {
    pub(super) fn new(auth: OpenAiAuth) -> Result<Self> {
        auth.validate()?;
        let url =
            Url::parse(&format!("{}/responses/compact", auth.base_url())).map_err(|error| {
                Error::caller("OpenAI compaction base URL is invalid").with_source(error)
            })?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(Error::caller(
                "OpenAI compaction endpoint must be an HTTP(S) URL",
            ));
        }
        let http = reqwest::Client::builder()
            .build()
            .map_err(transport_error)?;
        Ok(Self { auth, http, url })
    }

    pub(super) async fn compact(
        &self,
        model: &str,
        mode: OpenAiResponsesCompactionMode,
        response_id: Option<&str>,
        input: Vec<Value>,
    ) -> Result<Value> {
        let mut body = json!({"model": model});
        if mode == OpenAiResponsesCompactionMode::PreviousResponseId {
            body["previous_response_id"] = json!(response_id);
        } else {
            body["input"] = json!(input);
        }
        let mut request = self.http.post(self.url.clone()).header(
            "user-agent",
            concat!("rusty-agent/", env!("CARGO_PKG_VERSION")),
        );
        if let Some(key) = self.auth.api_key() {
            request = request.bearer_auth(key);
        }
        let response = apply_transport_headers(request, &self.auth, &BTreeMap::new())?
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;
        let facts = ResponseFacts::read(&response);
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) if !facts.status().is_success() => {
                return Err(response_failure(&facts, &Value::Null)
                    .with_source(error)
                    .into_error());
            }
            Err(error) => return Err(transport_error(error)),
        };
        let payload = match serde_json::from_slice::<Value>(&bytes) {
            Ok(payload) => payload,
            Err(error) if !facts.status().is_success() => {
                return Err(response_failure(&facts, &Value::Null)
                    .with_source(error)
                    .into_error());
            }
            Err(error) => {
                return Err(
                    behavior_error("OpenAI compaction returned a non-JSON response")
                        .with_source(error),
                );
            }
        };
        if !facts.status().is_success() {
            return Err(response_failure(&facts, &payload).into_error());
        }
        Ok(payload)
    }
}
