// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Explicit provider model discovery, identity-bound cache, and `ETag` refresh.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, PoisonError, RwLock, Weak},
    time::Duration,
};

use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderName, HeaderValue, header};
use ra_http::{ClientRouteClass, HttpClient, HttpClientFactory, RouteAwareClientPool};
use ra_model::openai::{
    auth::OpenAiAuth,
    responses::{ResponsesBearerToken, ResponsesCatalogObserver},
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{sync::Mutex, time::timeout};
use url::Url;

use super::{Config, ModelsResponse, model_auth::CommandBearerToken};

const CACHE_FILE: &str = "models_cache.json";
const CACHE_TTL_SECONDS: i64 = 300;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CATALOG_BYTES: usize = 1024 * 1024;
static CACHE_WRITES: Mutex<()> = Mutex::const_new(());

#[derive(Clone, Serialize, Deserialize)]
struct CacheEntry {
    fetched_at: DateTime<Utc>,
    identity: String,
    client_version: String,
    etag: Option<String>,
    #[serde(flatten)]
    catalog: Arc<ModelsResponse>,
}

pub(super) struct RemoteModelCatalog {
    path: PathBuf,
    url: Url,
    headers: HeaderMap,
    static_key: Option<String>,
    command: Option<Arc<CommandBearerToken>>,
    disabled: bool,
    client: HttpClient,
    client_factory: HttpClientFactory,
    request_max_retries: u64,
    identity_seed: Sha256,
    current: RwLock<Option<CacheEntry>>,
    refresh: Mutex<()>,
}

/// Shares catalog state and credentials across active threads with equivalent discovery settings.
#[derive(Default)]
pub(crate) struct RemoteModelCatalogs(StdMutex<Vec<Weak<RemoteModelCatalog>>>);

impl RemoteModelCatalogs {
    pub(super) fn share(&self, candidate: RemoteModelCatalog) -> Arc<RemoteModelCatalog> {
        let mut catalogs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        catalogs.retain(|catalog| catalog.strong_count() != 0);
        if let Some(existing) = catalogs.iter().filter_map(Weak::upgrade).find(|catalog| {
            catalog.path == candidate.path
                && catalog.identity(catalog.static_key.as_deref())
                    == candidate.identity(candidate.static_key.as_deref())
                && catalog.command.as_deref().map(CommandBearerToken::config)
                    == candidate.command.as_deref().map(CommandBearerToken::config)
                && catalog.disabled == candidate.disabled
                && catalog.client_factory == candidate.client_factory
                && catalog.request_max_retries == candidate.request_max_retries
        }) {
            return existing;
        }
        let catalog = Arc::new(candidate);
        catalogs.push(Arc::downgrade(&catalog));
        catalog
    }
}

enum Refresh<'a> {
    Startup { offline: bool },
    AuthChanged,
    Etag(&'a str),
}

impl RemoteModelCatalog {
    pub(super) fn new(
        config: &Config,
        url: &str,
        auth: &OpenAiAuth,
        static_key: Option<String>,
        command: Option<Arc<CommandBearerToken>>,
        disabled: bool,
    ) -> Result<Self, &'static str> {
        let mut url = Url::parse(url).map_err(|_| "model_catalog_url must be an absolute URL")?;
        if !matches!(url.scheme(), "https" | "http") {
            return Err("model_catalog_url must use HTTP or HTTPS");
        }
        {
            let mut query = url.query_pairs_mut();
            if let Some(params) = &config.model_provider.query_params {
                let mut params: Vec<_> = params.iter().collect();
                params.sort_by_key(|(name, _)| *name);
                query.extend_pairs(params.into_iter().map(|(key, value)| (key, value.as_str())));
            }
            query.append_pair("client_version", client_version());
        }
        let mut identity_seed = Sha256::new();
        for part in [
            "models-cache-v1",
            &config.model_provider_id,
            &config.model_provider.name,
            auth.base_url(),
            url.as_str(),
        ] {
            hash_field(&mut identity_seed, part.as_bytes());
        }
        let mut headers = HeaderMap::new();
        for (name, value) in auth.default_headers() {
            if let (Ok(name), Ok(value)) =
                (HeaderName::try_from(name), HeaderValue::from_str(value))
                && name != header::AUTHORIZATION
            {
                hash_field(&mut identity_seed, name.as_str().as_bytes());
                hash_field(&mut identity_seed, value.as_bytes());
                headers.insert(name, value);
            }
        }
        let client_factory = config.http_client_factory();
        Ok(Self {
            path: config.codex_home.join(CACHE_FILE).to_path_buf(),
            url,
            headers,
            static_key,
            command,
            disabled,
            client: RouteAwareClientPool::new_without_redirects_or_request_logging(
                client_factory.clone(),
                ClientRouteClass::Api,
            )
            .into_client(),
            client_factory,
            request_max_retries: config
                .model_provider
                .request_max_retries
                .unwrap_or(4)
                .min(100),
            identity_seed,
            current: RwLock::new(None),
            refresh: Mutex::new(()),
        })
    }

    pub(super) fn command(&self) -> Option<Arc<CommandBearerToken>> {
        self.command.clone()
    }

    fn identity(&self, key: Option<&str>) -> String {
        let mut digest = self.identity_seed.clone();
        hash_field(&mut digest, key.unwrap_or_default().as_bytes());
        format!("{:x}", digest.finalize())
    }

    pub(super) fn snapshot(&self) -> Option<Arc<ModelsResponse>> {
        let identity = match &self.command {
            Some(command) => self.identity(Some(&command.last_resolved_token()?)),
            None => self.identity(self.static_key.as_deref()),
        };
        let snapshot = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = snapshot
            .as_ref()
            .filter(|entry| entry.identity == identity)?;
        Some(Arc::clone(&entry.catalog))
    }

    pub(super) async fn load(&self, offline: bool) {
        self.refresh_catalog(Refresh::Startup { offline }).await;
    }

    pub(super) async fn refresh_after_auth_change(&self) {
        if self.command.is_some()
            && timeout(REFRESH_TIMEOUT, self.refresh_catalog(Refresh::AuthChanged))
                .await
                .is_err()
        {
            tracing::warn!("provider model catalog refresh after credential change timed out");
        }
    }

    async fn refresh_catalog(&self, reason: Refresh<'_>) {
        if self.disabled {
            return;
        }
        let _guard = self.refresh.lock().await;
        let key = match &self.command {
            Some(command) => {
                if matches!(reason, Refresh::Etag(_)) {
                    command.last_resolved_token()
                } else {
                    let Ok(Ok(key)) = timeout(REFRESH_TIMEOUT, command.token(None)).await else {
                        tracing::warn!("failed to resolve provider model catalog credentials");
                        return;
                    };
                    Some(key)
                }
            }
            None => self.static_key.clone(),
        };
        let identity = self.identity(key.as_deref());
        let current = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|entry| entry.identity == identity)
            .cloned();
        if matches!(reason, Refresh::AuthChanged) && current.is_some() {
            return;
        }
        let cached = self
            .read_cache()
            .await
            .filter(|entry| entry.identity == identity && entry.client_version == client_version());
        if let Refresh::Etag(etag) = reason
            && let Some(mut entry) = current
                .as_ref()
                .filter(|entry| entry.etag.as_deref() == Some(etag))
                .cloned()
        {
            if cached.as_ref().is_some_and(|cached| {
                cached.etag.as_deref() == Some(etag) && !is_fresh(cached, CACHE_TTL_SECONDS / 2)
            }) {
                entry.fetched_at = Utc::now();
                self.store(&entry).await;
                self.publish(entry);
            }
            return;
        }
        let cached = cached.filter(|entry| {
            is_fresh(entry, CACHE_TTL_SECONDS)
                && match reason {
                    Refresh::Etag(etag) => entry.etag.as_deref() == Some(etag),
                    _ => current
                        .as_ref()
                        .is_none_or(|current| current.fetched_at <= entry.fetched_at),
                }
        });
        if let Some(entry) = cached {
            self.publish(entry);
            return;
        }
        if matches!(reason, Refresh::Startup { .. })
            && current.is_some_and(|entry| is_fresh(&entry, CACHE_TTL_SECONDS))
        {
            return;
        }
        if matches!(reason, Refresh::Startup { offline: true }) || key.is_none() {
            return;
        }
        self.fetch_and_update(identity, key.as_deref()).await;
    }

    async fn fetch_and_update(&self, identity: String, key: Option<&str>) {
        let started_at = Utc::now();
        let result = timeout(REFRESH_TIMEOUT, self.fetch(key))
            .await
            .unwrap_or(Err("catalog request timed out"));
        match result {
            Ok((catalog, etag)) => {
                let entry = CacheEntry {
                    fetched_at: Utc::now(),
                    identity,
                    client_version: client_version().to_owned(),
                    etag,
                    catalog: Arc::new(catalog),
                };
                self.store(&entry).await;
                self.publish(entry);
            }
            Err(reason) => {
                tracing::warn!(
                    reason,
                    "failed to refresh configured provider model catalog"
                );
                self.publish(CacheEntry {
                    fetched_at: DateTime::<Utc>::MIN_UTC,
                    identity: identity.clone(),
                    client_version: client_version().to_owned(),
                    etag: None,
                    catalog: Arc::new(ModelsResponse::default()),
                });
                self.invalidate(&identity, started_at).await;
            }
        }
    }

    fn publish(&self, entry: CacheEntry) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Some(entry);
    }

    async fn read_cache(&self) -> Option<CacheEntry> {
        let contents = tokio::fs::read(&self.path).await.ok()?;
        serde_json::from_slice(&contents).ok()
    }

    async fn store(&self, entry: &CacheEntry) {
        let _guard = CACHE_WRITES.lock().await;
        self.write_entry(entry).await;
    }

    async fn write_entry(&self, entry: &CacheEntry) {
        let Ok(contents) = serde_json::to_string(entry) else {
            return;
        };
        let path = self.path.clone();
        if !matches!(
            tokio::task::spawn_blocking(move || ra_paths::write_atomically(&path, &contents)).await,
            Ok(Ok(()))
        ) {
            tracing::warn!("failed to persist provider model catalog");
        }
    }

    async fn invalidate(&self, identity: &str, before: DateTime<Utc>) {
        let _guard = CACHE_WRITES.lock().await;
        if let Some(mut entry) = self.read_cache().await
            && entry.identity == identity
            && entry.client_version == client_version()
            && entry.fetched_at <= before
        {
            entry.fetched_at = DateTime::<Utc>::MIN_UTC;
            entry.etag = None;
            entry.catalog = Arc::new(ModelsResponse::default());
            self.write_entry(&entry).await;
        }
    }

    async fn fetch(
        &self,
        key: Option<&str>,
    ) -> Result<(ModelsResponse, Option<String>), &'static str> {
        for attempt in 0.. {
            let mut request = self
                .client
                .get(self.url.as_str())
                .headers(self.headers.clone());
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let result = request.send().await;
            let delay = match &result {
                Ok(response) if response.status().is_server_error() => {
                    ra_http::RetryAfter::from_headers(response.headers())
                }
                Err(ra_http::HttpError::Request(error)) if !error.is_builder() => None,
                Err(ra_http::HttpError::Timeout) => None,
                _ => return decode(result.map_err(|_| "catalog transport failed")?).await,
            };
            if attempt >= self.request_max_retries {
                return Err("catalog retry budget exhausted");
            }
            drop(result);
            if let Some(delay) = delay {
                tokio::time::sleep_until(delay.deadline()).await;
            } else {
                let base = 200u128.saturating_mul(
                    2u128.saturating_pow(u32::try_from(attempt).unwrap_or(u32::MAX)),
                );
                let jitter = rand::rng().random_range(900..1100);
                let millis = u64::try_from(base.saturating_mul(jitter) / 1000).unwrap_or(u64::MAX);
                tokio::time::sleep(Duration::from_millis(millis)).await;
            }
        }
        unreachable!("catalog retries are bounded")
    }
}

#[async_trait::async_trait]
impl ResponsesCatalogObserver for RemoteModelCatalog {
    async fn observe_models_etag(&self, etag: &str) {
        self.refresh_catalog(Refresh::Etag(etag)).await;
    }
}

async fn decode(
    mut response: ra_http::HttpResponse,
) -> Result<(ModelsResponse, Option<String>), &'static str> {
    if !response.status().is_success() {
        return Err("catalog endpoint returned an unsuccessful status");
    }
    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut contents = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "catalog body read failed")?
    {
        if contents.len().saturating_add(chunk.len()) > MAX_CATALOG_BYTES {
            return Err("catalog response exceeds the byte limit");
        }
        contents.extend_from_slice(&chunk);
    }
    let catalog: ModelsResponse =
        serde_json::from_slice(&contents).map_err(|_| "invalid model catalog")?;
    let mut slugs = HashSet::new();
    if catalog
        .models
        .iter()
        .any(|model| model.slug.trim().is_empty() || !slugs.insert(&model.slug))
    {
        return Err("catalog model slugs must be unique and non-empty");
    }
    Ok((catalog, etag))
}

fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}

fn client_version() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION_MAJOR"),
        ".",
        env!("CARGO_PKG_VERSION_MINOR"),
        ".",
        env!("CARGO_PKG_VERSION_PATCH"),
    )
}

fn is_fresh(entry: &CacheEntry, ttl: i64) -> bool {
    entry.fetched_at != DateTime::<Utc>::MIN_UTC
        && Utc::now().signed_duration_since(entry.fetched_at) <= chrono::Duration::seconds(ttl)
}
