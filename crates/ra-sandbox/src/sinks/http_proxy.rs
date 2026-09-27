//! The HTTP proxy sink, behind the `http-sink` feature.
//!
//! Gated because its client brings a TLS stack with a C build step, which the other sinks and the
//! backends do not need; a host that posts events to a collector turns it on.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use ra_core::sandbox::{
    DeliveryMode, EventPayloadPolicy, EventSink, OnErrorPolicy, SandboxSessionEvent, SinkError,
    event_to_json_line,
};

use super::append_locked;

/// How long a proxy post waits by default, in seconds.
pub const DEFAULT_HTTP_PROXY_TIMEOUT_S: f64 = 5.0;

/// Posts each event as JSON to an endpoint — a local daemon, or a remote collector.
///
/// A post that fails — no connection, a timeout, an error status — is appended to the spool file,
/// when there is one, as the line the JSONL sinks write, and then reported as the sink's failure.
#[derive(Debug)]
pub struct HttpProxySink {
    endpoint: String,
    headers: Vec<(String, String)>,
    timeout: Duration,
    spool_path: Option<PathBuf>,
    mode: DeliveryMode,
    on_error: OnErrorPolicy,
    payload_policy: Option<EventPayloadPolicy>,
    client: reqwest::Client,
}

impl HttpProxySink {
    /// Posts to `endpoint`, in the background, logging failures.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            headers: Vec::new(),
            timeout: Duration::from_secs_f64(DEFAULT_HTTP_PROXY_TIMEOUT_S),
            spool_path: None,
            mode: DeliveryMode::BestEffort,
            on_error: OnErrorPolicy::Log,
            payload_policy: None,
            client: reqwest::Client::new(),
        }
    }

    /// Sends `headers` with every post. They are copied now, so a caller changing its own map later
    /// changes nothing. A `content-type` given here replaces the JSON one.
    #[must_use]
    pub fn with_headers<K, V>(mut self, headers: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.headers = headers
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        self
    }

    /// Waits `timeout_s` seconds for a post instead.
    #[must_use]
    pub fn with_timeout_s(mut self, timeout_s: f64) -> Self {
        self.timeout = Duration::try_from_secs_f64(timeout_s).unwrap_or(self.timeout);
        self
    }

    /// Spools failed posts to `path`.
    #[must_use]
    pub fn with_spool_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.spool_path = Some(path.into());
        self
    }

    /// Delivers in `mode` instead.
    #[must_use]
    pub const fn with_mode(mut self, mode: DeliveryMode) -> Self {
        self.mode = mode;
        self
    }

    /// Handles a failure by `policy` instead.
    #[must_use]
    pub const fn with_on_error(mut self, policy: OnErrorPolicy) -> Self {
        self.on_error = policy;
        self
    }

    /// Sees events through `policy`, over the instrumentation's.
    #[must_use]
    pub const fn with_payload_policy(mut self, policy: EventPayloadPolicy) -> Self {
        self.payload_policy = Some(policy);
        self
    }

    /// The headers every post carries, the JSON content type first.
    #[must_use]
    pub fn request_headers(&self) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> =
            vec![("content-type".to_owned(), "application/json".to_owned())];
        for (name, value) in &self.headers {
            if let Some(existing) = headers
                .iter_mut()
                .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            {
                existing.1.clone_from(value);
            } else {
                headers.push((name.clone(), value.clone()));
            }
        }
        headers
    }

    async fn post(&self, body: Vec<u8>) -> Result<(), String> {
        let mut request = self
            .client
            .post(&self.endpoint)
            .timeout(self.timeout)
            .body(body);
        for (name, value) in self.request_headers() {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        let response = response
            .error_for_status()
            .map_err(|error| error.to_string())?;
        // Read the body, so the request is complete before the sink reports success.
        response.bytes().await.map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[async_trait]
impl EventSink for HttpProxySink {
    fn type_name(&self) -> &'static str {
        "HttpProxySink"
    }

    fn mode(&self) -> DeliveryMode {
        self.mode
    }

    fn on_error(&self) -> OnErrorPolicy {
        self.on_error
    }

    fn payload_policy(&self) -> Option<&EventPayloadPolicy> {
        self.payload_policy.as_ref()
    }

    async fn handle(&self, event: SandboxSessionEvent) -> Result<(), SinkError> {
        let body = serde_json::to_vec(&event)?;
        if let Err(message) = self.post(body).await {
            if let Some(spool_path) = &self.spool_path {
                let line = event_to_json_line(&event);
                let spool_path = spool_path.clone();
                // Best effort: a spool that cannot be written does not replace the post's failure.
                let _ =
                    tokio::task::spawn_blocking(move || append_locked(&spool_path, &line)).await;
            }
            return Err(format!("http proxy sink POST failed: {message}").into());
        }
        Ok(())
    }
}
