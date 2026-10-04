//! An `OpenAI` Conversations conversation as a [`Session`]: a port of `openai-agents-python`'s
//! `memory/openai_conversations_session.py`.
//!
//! The history lives on the server. The remote conversation is created on first use, items are
//! listed, created and deleted through the Conversations API, a bounded read pages newest first
//! and stops at the bound, and popping deletes the newest remote item.
//!
//! # Deviations from the reference
//!
//! - **Two identities.** The reference's `session_id` is the remote conversation id and does not
//!   exist until the conversation does. Here the session has a local [`SessionId`] from the start
//!   — the one a run's checkpoint records — and the remote [`ProviderConversationId`] is separate,
//!   created lazily as in the reference. A host that restores a checkpoint rebuilds the session
//!   with the same session id and conversation id.
//! - **The session lowers its own items.** The reference's runner cleans items for this session
//!   before saving them — server item ids removed except where the create-item schema requires
//!   them, placeholder ids removed, reasoning it cannot store dropped. This session is handed
//!   [`RunItem`]s and does the same while lowering them to Responses input items. Items read back
//!   are lifted into run records whose identity is the conversation's item id; an item kind with
//!   no provider-neutral counterpart is an error. What a run record holds beyond its model input
//!   — provenance, host data, a raw copy from another provider — is not stored remotely.
//!   Filtering happens before the runtime captures a pending batch; its cursor still counts the
//!   filtered records. Storage fingerprints lower both pending and retrieved items to the same
//!   sanitized wire projection. Callback matching requires conversation identity except for
//!   assistant messages, independently of the general append-identity policy. These policies
//!   are dispatched through neutral Session methods because the runtime cannot depend on this
//!   provider adapter; the reference instead dispatches on its concrete session type.
//! - **No default client.** The reference falls back to its process-wide default `OpenAI` client.
//!   That account plumbing is not ported; the session is given its endpoint configuration.
//! - **Cancellation.** The reference waits for a clear's delete to settle before re-raising a
//!   caller's cancellation. A dropped Rust future cannot wait, so the delete runs on its own task
//!   and holds the session's conversation lock until it settles; the next operation waits for it.

mod items;

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result},
    item::{InputItemDigest, MessageRole, RunItem, RunItemKind},
    model::{ProviderConversationId, ProviderKey},
    session::{Session, SessionId, SessionSettings, resolve_session_limit},
};
use reqwest::{Method, Url};
use serde_json::{Value, json};

use self::items::{
    lift_conversation_item, lower_for_conversation, persistable_items, persistence_digest,
    stored_item_id,
};
use super::{
    apply_transport_headers,
    auth::OpenAiAuth,
    error::{ResponseFacts, behavior_error, response_failure, transport_error},
};

/// The provider alias stored items are attributed to unless the session is given another.
const DEFAULT_PROVIDER: &str = "openai";

/// Creates an empty remote conversation and returns its id.
///
/// The reference's `start_openai_conversations_session`.
///
/// # Errors
///
/// Returns a configuration error for an invalid `auth`, and the provider's error when the
/// conversation cannot be created.
pub async fn start_openai_conversations_session(
    auth: &OpenAiAuth,
) -> Result<ProviderConversationId> {
    ConversationsClient::new(auth.clone())?.create().await
}

/// A [`Session`] whose history is an `OpenAI` Conversations conversation.
///
/// ```no_run
/// # async fn demo() -> ra_core::error::Result<()> {
/// use ra_core::session::Session;
/// use ra_model::openai::{auth::OpenAiAuth, conversations::OpenAiConversationsSession};
///
/// let session = OpenAiConversationsSession::new("sess-demo", OpenAiAuth::new("sk-..."))?
///     .with_conversation_id("conv_123");
/// let history = session.get_items(None).await?;
/// # let _ = history;
/// # Ok(())
/// # }
/// ```
pub struct OpenAiConversationsSession {
    session_id: SessionId,
    session_settings: SessionSettings,
    provider: ProviderKey,
    client: ConversationsClient,
    conversation: Arc<ConversationSlot>,
}

/// The remote conversation id, and the lock that serializes creating and deleting it.
///
/// The id itself sits behind a plain mutex so it can be read and replaced without waiting for an
/// operation in flight, as the reference's `session_id` property and setter can.
#[derive(Default)]
struct ConversationSlot {
    id: Mutex<Option<ProviderConversationId>>,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl ConversationSlot {
    fn get(&self) -> Option<ProviderConversationId> {
        self.id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, id: Option<ProviderConversationId>) {
        *self.id.lock().unwrap_or_else(PoisonError::into_inner) = id;
    }
}

impl OpenAiConversationsSession {
    /// Creates a session that starts a new remote conversation on first use.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when `auth` is invalid.
    pub fn new(session_id: impl Into<SessionId>, auth: OpenAiAuth) -> Result<Self> {
        Ok(Self {
            session_id: session_id.into(),
            session_settings: SessionSettings::new(),
            provider: ProviderKey::new(DEFAULT_PROVIDER),
            client: ConversationsClient::new(auth)?,
            conversation: Arc::new(ConversationSlot::default()),
        })
    }

    /// Continues an existing remote conversation instead of starting one.
    #[must_use]
    pub fn with_conversation_id(self, conversation_id: impl Into<ProviderConversationId>) -> Self {
        self.conversation.set(Some(conversation_id.into()));
        self
    }

    /// Gives the session default settings: a read without an explicit limit uses theirs.
    #[must_use]
    pub const fn with_session_settings(mut self, settings: SessionSettings) -> Self {
        self.session_settings = settings;
        self
    }

    /// The registered provider alias that items read back are attributed to; `openai` by default.
    #[must_use]
    pub fn with_provider(mut self, provider: ProviderKey) -> Self {
        self.provider = provider;
        self
    }

    /// The remote conversation id, or `None` until the first operation that reaches the
    /// conversation creates it.
    #[must_use]
    pub fn conversation_id(&self) -> Option<ProviderConversationId> {
        self.conversation.get()
    }

    /// Points the session at another remote conversation.
    ///
    /// The reference's `session_id` setter: it does not wait for an operation in flight, and a
    /// clear that settles afterwards leaves the new id in place.
    pub fn set_conversation_id(&self, conversation_id: impl Into<ProviderConversationId>) {
        self.conversation.set(Some(conversation_id.into()));
    }

    /// The provider alias items read back are attributed to.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    /// The remote conversation id, creating the conversation if there is none yet.
    ///
    /// Concurrent first operations share one conversation; when creating it fails, the next
    /// waiting operation tries again.
    async fn ensure_conversation(&self) -> Result<ProviderConversationId> {
        let _guard = self.conversation.lock.lock().await;
        if let Some(id) = self.conversation.get() {
            return Ok(id);
        }
        let id = self.client.create().await?;
        self.conversation.set(Some(id.clone()));
        Ok(id)
    }
}

impl fmt::Debug for OpenAiConversationsSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiConversationsSession")
            .field("session_id", &self.session_id)
            .field("session_settings", &self.session_settings)
            .field("provider", &self.provider)
            .field("conversation_id", &self.conversation.get())
            .field("auth", &self.client.auth)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Session for OpenAiConversationsSession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    fn session_settings(&self) -> Option<&SessionSettings> {
        Some(&self.session_settings)
    }

    /// Always `true`: the conversation stores items under ids of its own.
    fn ignore_ids_for_matching(&self) -> bool {
        true
    }

    fn prepare_items_for_persistence(&self, items: Vec<RunItem>) -> Vec<RunItem> {
        persistable_items(items)
    }

    async fn item_digest_for_persistence(&self, item: &RunItem) -> Result<InputItemDigest> {
        persistence_digest(item).await
    }

    // The reference removes conversation identities from assistant history only. Other kinds
    // still require their identity when a callback reconstructs an item.
    fn matches_reconstructed_history_item(&self, item: &RunItem) -> bool {
        matches!(item.kind(), RunItemKind::Message(message) if message.role() == MessageRole::Assistant)
    }

    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        let conversation = self.ensure_conversation().await?;
        let limit = resolve_session_limit(limit, Some(&self.session_settings));
        if limit == Some(0) {
            return Ok(Vec::new());
        }
        self.client
            .list_items(&conversation, limit)
            .await?
            .iter()
            .map(|item| lift_conversation_item(item, &self.provider))
            .collect()
    }

    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let lowered = lower_for_conversation(&items).await?;
        if lowered.is_empty() {
            return Ok(());
        }
        let conversation = self.ensure_conversation().await?;
        self.client.create_items(&conversation, lowered).await
    }

    async fn pop_item(&self) -> Result<Option<RunItem>> {
        let conversation = self.ensure_conversation().await?;
        let newest = self.client.list_items(&conversation, Some(1)).await?;
        let Some(item) = newest.first() else {
            return Ok(None);
        };
        // Lifted before the delete, so an item this build cannot read is not removed unseen.
        let lifted = lift_conversation_item(item, &self.provider)?;
        self.client
            .delete_item(&conversation, stored_item_id(item)?)
            .await?;
        Ok(Some(lifted))
    }

    async fn clear(&self) -> Result<()> {
        let guard = Arc::clone(&self.conversation.lock).lock_owned().await;
        let Some(conversation) = self.conversation.get() else {
            return Ok(());
        };
        let client = self.client.clone();
        let slot = Arc::clone(&self.conversation);
        let delete = tokio::spawn(async move {
            let _guard = guard;
            client.delete(&conversation).await?;
            // A conversation set while the delete was in flight replaces this one; keep it.
            let mut current = slot.id.lock().unwrap_or_else(PoisonError::into_inner);
            if current.as_ref() == Some(&conversation) {
                *current = None;
            }
            Ok(())
        });
        delete.await.map_err(|error| {
            Error::caller("the OpenAI conversation delete task failed").with_source(error)
        })?
    }
}

/// The Conversations endpoints this session uses.
#[derive(Clone)]
struct ConversationsClient {
    auth: Arc<OpenAiAuth>,
    http: reqwest::Client,
}

impl ConversationsClient {
    fn new(auth: OpenAiAuth) -> Result<Self> {
        auth.validate()?;
        let http = reqwest::Client::builder()
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            auth: Arc::new(auth),
            http,
        })
    }

    async fn create(&self) -> Result<ProviderConversationId> {
        let created = self
            .send(Method::POST, &[], &[], Some(&json!({"items": []})))
            .await?;
        let id = created
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| behavior_error("OpenAI conversation.id must be a string"))?;
        Ok(ProviderConversationId::new(id))
    }

    /// Lists a conversation's items oldest first.
    ///
    /// Unbounded, every page in ascending order. Bounded by `limit`, pages in descending order
    /// until `limit` items are in hand, then reverses them: the reference applies the bound
    /// locally and leaves the provider's page size alone.
    async fn list_items(
        &self,
        conversation: &ProviderConversationId,
        limit: Option<usize>,
    ) -> Result<Vec<Value>> {
        let order = if limit.is_some() { "desc" } else { "asc" };
        let mut items = Vec::new();
        let mut after: Option<String> = None;
        'pages: loop {
            let mut query = vec![("order", order.to_owned())];
            if let Some(after) = &after {
                query.push(("after", after.clone()));
            }
            let page = self
                .send(Method::GET, &[conversation.as_str(), "items"], &query, None)
                .await?;
            let data = page.get("data").and_then(Value::as_array).ok_or_else(|| {
                behavior_error("OpenAI conversation item list.data must be an array")
            })?;
            for item in data {
                items.push(item.clone());
                if limit.is_some_and(|limit| items.len() >= limit) {
                    break 'pages;
                }
            }
            // The reference client's cursor page: stop on an explicit `has_more: false`, an empty
            // page, or a page without a last id to continue after.
            let has_more = page.get("has_more").and_then(Value::as_bool);
            let last_id = page
                .get("last_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            match last_id {
                Some(last_id) if has_more != Some(false) && !data.is_empty() => {
                    after = Some(last_id.to_owned());
                }
                _ => break,
            }
        }
        if limit.is_some() {
            items.reverse();
        }
        Ok(items)
    }

    async fn create_items(
        &self,
        conversation: &ProviderConversationId,
        items: Vec<Value>,
    ) -> Result<()> {
        self.send(
            Method::POST,
            &[conversation.as_str(), "items"],
            &[],
            Some(&json!({"items": items})),
        )
        .await
        .map(drop)
    }

    async fn delete_item(
        &self,
        conversation: &ProviderConversationId,
        item_id: &str,
    ) -> Result<()> {
        self.send(
            Method::DELETE,
            &[conversation.as_str(), "items", item_id],
            &[],
            None,
        )
        .await
        .map(drop)
    }

    async fn delete(&self, conversation: &ProviderConversationId) -> Result<()> {
        self.send(Method::DELETE, &[conversation.as_str()], &[], None)
            .await
            .map(drop)
    }

    /// Sends one request under `/conversations` and returns its JSON body, `null` when empty.
    async fn send(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut request = self.http.request(method, self.url(segments)?).header(
            "user-agent",
            concat!("rusty-agent/", env!("CARGO_PKG_VERSION")),
        );
        if let Some(api_key) = self.auth.api_key() {
            request = request.bearer_auth(api_key);
        }
        request = apply_transport_headers(request, &self.auth, &BTreeMap::new())?;
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await.map_err(transport_error)?;
        let facts = ResponseFacts::read(&response);
        let status = facts.status();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) if !status.is_success() => {
                return Err(response_failure(&facts, &Value::Null)
                    .with_source(error)
                    .into_error());
            }
            Err(error) => return Err(transport_error(error)),
        };
        let payload = if bytes.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(payload) => payload,
                Err(error) if !status.is_success() => {
                    return Err(response_failure(&facts, &Value::Null)
                        .with_source(error)
                        .into_error());
                }
                Err(error) => {
                    return Err(
                        behavior_error("OpenAI returned a non-JSON response").with_source(error)
                    );
                }
            }
        };
        if !status.is_success() {
            return Err(response_failure(&facts, &payload).into_error());
        }
        Ok(payload)
    }

    /// `{base_url}/conversations/{segments...}`, each segment escaped as one path segment.
    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = Url::parse(self.auth.base_url()).map_err(|error| {
            Error::config("OpenAI base URL is not a valid URL").with_source(error)
        })?;
        url.path_segments_mut()
            .map_err(|()| Error::config("OpenAI base URL cannot carry a path"))?
            .push("conversations")
            .extend(segments);
        Ok(url)
    }
}
