// Adapted from openai/codex (Apache-2.0) @ e58bf8f5233b51e7cd1679d947e3028c1ed91b06, modified.
//! Connection-owned helper headers and shared refreshes.

mod challenge;

use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use ra_core::error::{Error, Result};
use reqwest::{
    Request, Response, StatusCode,
    header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue},
};
use serde::{
    Deserialize,
    de::{MapAccess, Visitor},
};
use std::{
    ffi::OsString,
    fmt,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use url::{Origin, Url};

const HELPER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HELPER_OUTPUT_BYTES: usize = 64 * 1024;
type HelperResult = std::result::Result<Arc<HeaderMap>, HelperFailure>;
type CachedHeaders = Shared<BoxFuture<'static, HelperResult>>;

#[derive(Clone, Copy, Debug)]
pub(super) enum HelperFailure {
    Message(&'static str),
    Process(super::HttpHeadersHelperProcessError),
}

impl fmt::Display for HelperFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => f.write_str(message),
            Self::Process(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for HelperFailure {}

impl From<&'static str> for HelperFailure {
    fn from(message: &'static str) -> Self {
        Self::Message(message)
    }
}

impl From<Error> for HelperFailure {
    fn from(error: Error) -> Self {
        std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<super::HttpHeadersHelperProcessError>())
            .map_or(
                Self::Message("MCP HTTP headers helper execution failed"),
                |error| Self::Process(*error),
            )
    }
}

pub(super) type RunHelper = Arc<
    dyn Fn(Vec<(OsString, OsString)>, usize) -> BoxFuture<'static, Result<Vec<u8>>> + Send + Sync,
>;

pub(super) struct HeadersHelper {
    origin: Origin,
    run: RunHelper,
    cache: Mutex<HeadersCache>,
}

struct HeadersCache {
    // Advance even after a failed refresh so concurrent rejections share one attempt.
    refresh_epoch: u64,
    current: CachedHeaders,
    refresh: Option<CachedHeaders>,
}

pub(super) struct RequestHeaders {
    refresh_epoch: u64,
    values: Arc<HeaderMap>,
}

impl HeadersHelper {
    pub(super) fn new(url: &Url, run: RunHelper) -> Self {
        let current = Self::attempt(run.clone());
        Self {
            origin: url.origin(),
            run,
            cache: Mutex::new(HeadersCache {
                refresh_epoch: 0,
                current,
                refresh: None,
            }),
        }
    }

    pub(super) fn matches(&self, url: &Url) -> bool {
        self.origin == url.origin()
    }

    fn attempt(run: RunHelper) -> CachedHeaders {
        async move {
            // A request can drop its waiter without abandoning the shared child. Dropping the
            // connection and all waiters drops the task set and cancels the owned child.
            let mut tasks = tokio::task::JoinSet::new();
            tasks.spawn(async move {
                let env = super::super::process_env::core_env().collect();
                let bytes = tokio::time::timeout(HELPER_TIMEOUT, run(env, MAX_HELPER_OUTPUT_BYTES))
                    .await
                    .map_err(|_| "MCP HTTP headers helper timed out after 10 seconds")??;
                parse_helper_output(&bytes)
                    .map(Arc::new)
                    .map_err(HelperFailure::from)
            });
            tasks
                .join_next()
                .await
                .ok_or("MCP HTTP headers helper task was unavailable")?
                .map_err(|_| "MCP HTTP headers helper task failed")?
        }
        .boxed()
        .shared()
    }

    pub(super) async fn headers(&self) -> std::result::Result<RequestHeaders, HelperFailure> {
        let (refresh_epoch, current) = {
            let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            (cache.refresh_epoch, cache.current.clone())
        };
        current.await.map(|values| RequestHeaders {
            refresh_epoch,
            values,
        })
    }

    async fn refresh(&self, rejected_epoch: u64) -> HelperResult {
        let attempt = {
            let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if cache.refresh_epoch == rejected_epoch {
                cache
                    .refresh
                    .get_or_insert_with(|| Self::attempt(self.run.clone()))
                    .clone()
            } else {
                cache.current.clone()
            }
        };
        let result = attempt.clone().await;
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.refresh_epoch == rejected_epoch
            && cache
                .refresh
                .as_ref()
                .is_some_and(|refresh| refresh.ptr_eq(&attempt))
        {
            cache.refresh_epoch = rejected_epoch.saturating_add(1);
            if result.is_ok() {
                cache.current = attempt;
            }
            cache.refresh = None;
        }
        result
    }

    pub(super) async fn retry(
        &self,
        original: &Request,
        rejected: &RequestHeaders,
        response: &Response,
    ) -> Option<Request> {
        let status = response.status();
        if !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
            || status == StatusCode::FORBIDDEN
                && challenge::has_insufficient_scope(response.headers())
        {
            return None;
        }

        let refreshed = self.refresh(rejected.refresh_epoch).await.ok()?;
        let explicit_authorization = original.headers().contains_key(AUTHORIZATION);
        let differs = |left: &HeaderMap, right: &HeaderMap| {
            left.iter().any(|(name, value)| {
                (!explicit_authorization || name != AUTHORIZATION) && right.get(name) != Some(value)
            })
        };
        if !differs(&rejected.values, &refreshed) && !differs(&refreshed, &rejected.values) {
            return None;
        }
        let mut retry = original.try_clone()?;
        apply_headers(&mut retry, &refreshed);
        Some(retry)
    }
}

impl RequestHeaders {
    pub(super) fn apply(&self, request: &mut Request) {
        apply_headers(request, &self.values);
    }
}

fn apply_headers(request: &mut Request, headers: &HeaderMap) {
    for (name, value) in headers {
        if name != AUTHORIZATION || !request.headers().contains_key(AUTHORIZATION) {
            request.headers_mut().insert(name, value.clone());
        }
    }
}

// Parse entries before normalizing names so exact duplicates also fail.
struct RawHeaderEntries(Vec<(String, String)>);

impl<'de> Deserialize<'de> for RawHeaderEntries {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct EntriesVisitor;
        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = RawHeaderEntries;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object of string header names and values")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or_default());
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(RawHeaderEntries(entries))
            }
        }
        deserializer.deserialize_map(EntriesVisitor)
    }
}

fn parse_helper_output(stdout: &[u8]) -> std::result::Result<HeaderMap, &'static str> {
    if stdout.len() > MAX_HELPER_OUTPUT_BYTES {
        return Err("MCP HTTP headers helper output exceeds 64 KiB");
    }
    let stdout =
        std::str::from_utf8(stdout).map_err(|_| "MCP HTTP headers helper wrote non-UTF-8 data")?;
    let RawHeaderEntries(entries) = serde_json::from_str(stdout.trim())
        .map_err(|_| "MCP HTTP headers helper must output a JSON object of strings")?;
    let mut parsed = HeaderMap::with_capacity(entries.len());
    for (name, value) in entries {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "MCP HTTP headers helper returned an invalid header name")?;
        if matches!(
            name.as_str(),
            "accept"
                | "connection"
                | "content-encoding"
                | "content-length"
                | "content-type"
                | "host"
                | "keep-alive"
                | "last-event-id"
                | "mcp-protocol-version"
                | "mcp-session-id"
                | "origin"
                | "proxy-connection"
                | "referer"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) {
            return Err("MCP HTTP headers helper returned a reserved header");
        }
        if parsed.contains_key(&name) {
            return Err("MCP HTTP headers helper returned duplicate header names");
        }
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| "MCP HTTP headers helper returned an invalid header value")?;
        value.set_sensitive(true);
        parsed.insert(name, value);
    }
    Ok(parsed)
}
