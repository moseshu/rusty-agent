//! Responses compaction session decorator, ported from `openai-agents-python`.
//!
//! The wrapper serializes mutations through replacement and rollback. Automatic compaction
//! consumes ordered evidence from the actual successful exchange; stores offering atomic
//! snapshots may replace a model-visible suffix, while other stores require complete coverage.
//!
//! Rust adaptations: endpoint credentials are explicit; wire items are lowered from typed
//! records; usage is returned for the run's ledger to settle rather than mutating a live context.
//! Once a replacement starts, an independent task holds the mutation lock through settlement,
//! since a dropped Rust future cannot perform Python's cancellation cleanup. Provider modes and
//! the decision hook stay here rather than becoming provider-neutral core settings.

mod client;
mod items;

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    cancel::{CancelReason, CancelScope, ScopeKind},
    error::{Error, Result},
    item::{InputItemDigest, ModelInputItem, RunItem},
    model::{ConversationContinuation, ModelRequest, ProviderKey},
    session::{
        CompactionSnapshot, Session, SessionCompaction, SessionCompactionContext,
        SessionCompactionOutcome, SessionId,
    },
    usage::Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::auth::OpenAiAuth;
use client::CompactionClient;
use items::{
    decision_items, digest_item, digest_request, lift_output, lower_items, suffix_is_intact,
    user_message,
};

/// Default number of non-user, non-compaction items that triggers compaction.
pub const DEFAULT_COMPACTION_THRESHOLD: usize = 10;

pub use items::select_compaction_candidate_items;
const ALL_SESSION_ITEMS_LIMIT: usize = 2_147_483_647;

/// How a Responses compact request receives history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum OpenAiResponsesCompactionMode {
    /// Uses stored response history when available, otherwise local input.
    #[default]
    Auto,
    /// Explicitly uses server-managed response history.
    PreviousResponseId,
    /// Sends local session input.
    Input,
}

/// Arguments for a manual Responses compaction.
#[derive(Debug, Clone, Default)]
pub struct OpenAiResponsesCompactionArgs {
    /// Latest response identity; absent values retain the wrapper's previous identity.
    pub response_id: Option<String>,
    /// Overrides the wrapper's configured mode for this call.
    pub compaction_mode: Option<OpenAiResponsesCompactionMode>,
    /// Whether the latest response was stored; auto avoids unstored responses.
    pub store: Option<bool>,
    /// Bypasses the initial candidate threshold.
    pub force: bool,
}

/// The provider-specific context handed to the decision hook.
#[derive(Debug, Clone)]
pub struct OpenAiResponsesCompactionDecision {
    /// Latest response identity.
    response_id: Option<String>,
    /// Resolved mode, always input or previous-response-id.
    compaction_mode: OpenAiResponsesCompactionMode,
    /// Normalized history items eligible for the candidate threshold.
    compaction_candidate_items: Vec<Value>,
    /// Normalized history, including unsupported retained records, in chronological order.
    session_items: Vec<Value>,
}

impl OpenAiResponsesCompactionDecision {
    /// Latest successful response identity.
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }
    /// Resolved provider request mode.
    #[must_use]
    pub const fn compaction_mode(&self) -> OpenAiResponsesCompactionMode {
        self.compaction_mode
    }
    /// Non-user, non-compaction history items selected for the threshold.
    #[must_use]
    pub fn compaction_candidate_items(&self) -> &[Value] {
        &self.compaction_candidate_items
    }
    /// History considered by this decision; unsupported items retain their typed fields.
    ///
    /// Only the final selected history is strictly lowered for the compact request.
    #[must_use]
    pub fn session_items(&self) -> &[Value] {
        &self.session_items
    }
}

/// Default decision: compact when at least ten candidate items exist.
#[must_use]
pub fn default_should_trigger_compaction(context: &OpenAiResponsesCompactionDecision) -> bool {
    context.compaction_candidate_items.len() >= DEFAULT_COMPACTION_THRESHOLD
}

/// Accepts the reference's GPT, reasoning, and fine-tuned model name conventions.
#[must_use]
pub fn is_openai_model_name(model: &str) -> bool {
    let trimmed = model.trim();
    let root = trimmed
        .strip_prefix("ft:")
        .unwrap_or(trimmed)
        .split(':')
        .next()
        .unwrap_or("");
    root.starts_with("gpt-")
        || (root.starts_with('o') && root.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
}

type DecisionHook = dyn Fn(&OpenAiResponsesCompactionDecision) -> bool + Send + Sync;

/// A local Session decorated with optional Responses API compaction.
#[derive(Clone)]
pub struct OpenAiResponsesCompactionSession {
    session_id: SessionId,
    underlying: Arc<dyn Session>,
    client: CompactionClient,
    model: String,
    mode: OpenAiResponsesCompactionMode,
    should_trigger: Arc<DecisionHook>,
    max_rollback_items: Option<usize>,
    provider: ProviderKey,
    state: Arc<Mutex<CompactionState>>,
}

#[derive(Default)]
struct CompactionState {
    cached_items: Option<Vec<RunItem>>,
    response_id: Option<String>,
    deferred_response_id: Option<String>,
    last_unstored_response_id: Option<String>,
    generation: u64,
}

struct AutomaticHistory {
    items: Vec<RunItem>,
    mode: OpenAiResponsesCompactionMode,
    snapshot: Option<CompactionSnapshot>,
    suffix_start: usize,
}

impl CompactionState {
    fn invalidate(&mut self) {
        self.cached_items = None;
        self.response_id = None;
        self.deferred_response_id = None;
        self.last_unstored_response_id = None;
        self.generation = self.generation.wrapping_add(1);
    }
}

impl fmt::Debug for OpenAiResponsesCompactionSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiResponsesCompactionSession")
            .field("session_id", &self.session_id)
            .field("model", &self.model)
            .field("compaction_mode", &self.mode)
            .field("max_rollback_items", &self.max_rollback_items)
            .finish_non_exhaustive()
    }
}

impl OpenAiResponsesCompactionSession {
    /// Wraps a local session. Defaults to `gpt-4.1`, auto mode, and ten candidates.
    ///
    /// Rejects a provider-managed conversation session, whose history cannot be locally replaced.
    pub fn new(
        session_id: impl Into<SessionId>,
        underlying: Arc<dyn Session>,
        auth: OpenAiAuth,
    ) -> Result<Self> {
        if underlying.manages_server_history() {
            return Err(Error::caller(
                "OpenAiResponsesCompactionSession cannot wrap a provider-managed conversation session because it manages its own history on the server",
            ));
        }
        Ok(Self {
            session_id: session_id.into(),
            underlying,
            client: CompactionClient::new(auth)?,
            model: "gpt-4.1".into(),
            mode: OpenAiResponsesCompactionMode::Auto,
            should_trigger: Arc::new(default_should_trigger_compaction),
            max_rollback_items: None,
            provider: ProviderKey::new("openai"),
            state: Arc::new(Mutex::new(CompactionState::default())),
        })
    }

    /// Selects the compaction model, validating the reference's name conventions.
    pub fn with_model(mut self, model: impl Into<String>) -> Result<Self> {
        let model = model.into();
        if !is_openai_model_name(&model) {
            return Err(Error::caller(format!(
                "Unsupported model for OpenAI responses compaction: {model}"
            )));
        }
        self.model = model;
        Ok(self)
    }

    /// Selects how the API receives history.
    #[must_use]
    pub const fn with_compaction_mode(mut self, mode: OpenAiResponsesCompactionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Replaces the default candidate decision, including hooks which always decline.
    #[must_use]
    pub fn with_should_trigger_compaction(
        mut self,
        hook: impl Fn(&OpenAiResponsesCompactionDecision) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.should_trigger = Arc::new(hook);
        self
    }

    /// Bounds full rollback snapshots by item count; `None` keeps the unlimited policy.
    ///
    /// The budget is independent of read settings and does not constrain atomic suffix snapshots.
    pub fn with_max_rollback_items(mut self, limit: Option<usize>) -> Result<Self> {
        if limit == Some(0) {
            return Err(Error::caller("max_rollback_items must be positive or None"));
        }
        self.max_rollback_items = limit;
        Ok(self)
    }

    /// Attributes provider compaction items to this registered alias.
    #[must_use]
    pub fn with_provider(mut self, provider: ProviderKey) -> Self {
        self.provider = provider;
        self
    }

    /// The wrapped history backend.
    #[must_use]
    pub fn underlying_session(&self) -> &Arc<dyn Session> {
        &self.underlying
    }

    /// The configured model.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The configured request mode.
    #[must_use]
    pub const fn compaction_mode(&self) -> OpenAiResponsesCompactionMode {
        self.mode
    }

    /// The response whose compaction is deferred until local tool outputs reach the model.
    pub async fn deferred_compaction_response_id(&self) -> Option<String> {
        self.state.lock().await.deferred_response_id.clone()
    }

    /// Compacts stored history explicitly, even if it was not part of a run's model exchange.
    ///
    /// A manual call replaces the complete history. A retrieval default may select fewer items
    /// for the request, while rollback always snapshots the full retained history.
    pub async fn run_compaction(&self, args: Option<OpenAiResponsesCompactionArgs>) -> Result<()> {
        self.run_compaction_with_usage(args).await.into_result()
    }

    /// Manual compaction with billed usage, including when replacement subsequently fails.
    pub async fn run_compaction_with_usage(
        &self,
        args: Option<OpenAiResponsesCompactionArgs>,
    ) -> SessionCompactionOutcome {
        self.compact(args.unwrap_or_default(), None, &CancelScope::root())
            .await
    }

    fn resolve_mode(
        &self,
        state: &CompactionState,
        args: &OpenAiResponsesCompactionArgs,
        response_id: Option<&str>,
    ) -> OpenAiResponsesCompactionMode {
        let requested = args.compaction_mode.unwrap_or(self.mode);
        if requested != OpenAiResponsesCompactionMode::Auto {
            return requested;
        }
        if args.store == Some(false)
            || response_id.is_none()
            || (args.store.is_none() && response_id == state.last_unstored_response_id.as_deref())
        {
            OpenAiResponsesCompactionMode::Input
        } else {
            OpenAiResponsesCompactionMode::PreviousResponseId
        }
    }

    async fn all_items(&self) -> Result<Vec<RunItem>> {
        let limit = self
            .max_rollback_items
            .map_or(ALL_SESSION_ITEMS_LIMIT, |n| n.saturating_add(1));
        let items = self.underlying.get_items(Some(limit)).await?;
        if self.max_rollback_items.is_some_and(|n| items.len() > n) {
            return Err(Error::caller(
                "Compaction history exceeds max_rollback_items; history was retained",
            ));
        }
        Ok(items)
    }

    async fn cached_items(&self, state: &mut CompactionState) -> Result<Vec<RunItem>> {
        if let Some(items) = &state.cached_items {
            return Ok(items.clone());
        }
        let items = self.underlying.get_items(None).await?;
        state.cached_items = Some(items.clone());
        Ok(items)
    }

    fn decision(
        &self,
        response_id: Option<String>,
        mode: OpenAiResponsesCompactionMode,
        wire: Vec<Value>,
    ) -> bool {
        (self.should_trigger)(&OpenAiResponsesCompactionDecision {
            response_id,
            compaction_mode: mode,
            compaction_candidate_items: select_compaction_candidate_items(&wire),
            session_items: wire,
        })
    }

    async fn compact(
        &self,
        args: OpenAiResponsesCompactionArgs,
        automatic: Option<&SessionCompactionContext>,
        cancel: &CancelScope,
    ) -> SessionCompactionOutcome {
        let mut usage = Usage::default();
        let result = self
            .compact_inner(args, automatic, cancel, &mut usage)
            .await;
        SessionCompactionOutcome::new(usage, result)
    }

    async fn compact_inner(
        &self,
        args: OpenAiResponsesCompactionArgs,
        automatic: Option<&SessionCompactionContext>,
        cancel: &CancelScope,
        usage: &mut Usage,
    ) -> Result<()> {
        let mut state = cancel.run(Arc::clone(&self.state).lock_owned()).await?;
        if automatic.is_some_and(|context| context.generation() != Some(state.generation)) {
            tracing::warn!(
                "Skipped compaction because Session history changed after this run appended its items"
            );
            return Ok(());
        }
        if let Some(id) = args.response_id.as_ref().filter(|id| !id.is_empty()) {
            state.response_id = Some(id.clone());
        }
        if args.store == Some(false) {
            let state = &mut *state;
            state
                .last_unstored_response_id
                .clone_from(&state.response_id);
        } else if args.store == Some(true) && state.response_id == state.last_unstored_response_id {
            state.last_unstored_response_id = None;
        }
        let response_id = state.response_id.clone();
        let mode = self.resolve_mode(&state, &args, response_id.as_deref());
        if mode == OpenAiResponsesCompactionMode::PreviousResponseId && response_id.is_none() {
            return Err(Error::caller(
                "OpenAiResponsesCompactionSession.run_compaction requires a response_id when using previous_response_id compaction",
            ));
        }
        if automatic.is_none() && self.max_rollback_items.is_some() {
            cancel.run(self.all_items()).await??;
        }
        let selected = cancel.run(self.cached_items(&mut state)).await??;
        let approved_wire = cancel
            .run(decision_items(&selected, &self.provider))
            .await??;
        if !args.force && !self.decision(response_id.clone(), mode, approved_wire.clone()) {
            return Ok(());
        }
        let (selected, mode, snapshot, suffix_start) = if let Some(context) = automatic {
            let Some(selection) = self
                .select_automatic_history(
                    context,
                    &args,
                    response_id.clone(),
                    mode,
                    &approved_wire,
                    cancel,
                )
                .await?
            else {
                return Ok(());
            };
            (
                selection.items,
                selection.mode,
                selection.snapshot,
                selection.suffix_start,
            )
        } else {
            (selected, mode, None, 0)
        };
        if automatic.is_some() && snapshot.is_none() && self.max_rollback_items.is_some() {
            cancel.run(self.all_items()).await??;
        }
        let input = cancel.run(lower_items(&selected, &self.provider)).await??;
        tracing::info!(compaction.force = args.force, compaction.automatic = automatic.is_some(), compaction.mode = ?mode, compaction.input_items = selected.len(), "Responses session compaction started");
        let payload = cancel
            .run(
                self.client
                    .compact(&self.model, mode, response_id.as_deref(), input),
            )
            .await??;
        if payload.get("usage").is_some_and(|value| !value.is_null()) {
            *usage = super::responses::convert::convert_usage(payload.get("usage"));
        }
        let output = lift_output(&payload, &self.provider)?;
        self.settle_replacement(
            state,
            output,
            snapshot,
            suffix_start,
            automatic.is_some(),
            cancel,
        )
        .await
    }

    async fn settle_replacement(
        &self,
        mut state: tokio::sync::OwnedMutexGuard<CompactionState>,
        output: Vec<RunItem>,
        snapshot: Option<CompactionSnapshot>,
        suffix_start: usize,
        automatic: bool,
        cancel: &CancelScope,
    ) -> Result<()> {
        // Refresh rollback history after the provider request: expiring stores must never have
        // expired items revived by a stale pre-request snapshot.
        let previous = if snapshot.is_none() {
            Some(cancel.run(self.all_items()).await??)
        } else {
            None
        };
        let wrapper = self.clone();
        let mutation_cancel = cancel.child(ScopeKind::Run);
        let drop_guard = mutation_cancel
            .clone()
            .cancel_on_drop(CancelReason::Superseded);
        let cached_output = (!automatic && snapshot.is_none()).then(|| output.clone());
        // Once replacement begins it settles under the owned lock even if the caller drops us.
        let settled = tokio::spawn(async move {
            let result = match snapshot {
                Some(snapshot) => snapshot.replace_suffix(suffix_start, output).await,
                None => wrapper
                    .replace_with_rollback(output, previous.unwrap_or_default(), &mutation_cancel)
                    .await
                    .map(|()| true),
            };
            state.generation = state.generation.wrapping_add(1);
            state.cached_items = if matches!(result, Ok(true)) {
                cached_output
            } else {
                None
            };
            if matches!(result, Ok(true)) {
                state.deferred_response_id = None;
            }
            result
        })
        .await
        .map_err(|error| {
            Error::caller("Session compaction settlement task failed").with_source(error)
        })??;
        let _ = drop_guard.disarm();
        if !settled {
            tracing::warn!("Skipped compaction replacement because the stored suffix changed");
        }
        tracing::info!(
            compaction.replaced = settled,
            "Responses session compaction settled"
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn select_automatic_history(
        &self,
        context: &SessionCompactionContext,
        args: &OpenAiResponsesCompactionArgs,
        response_id: Option<String>,
        mut mode: OpenAiResponsesCompactionMode,
        approved_wire: &[Value],
        cancel: &CancelScope,
    ) -> Result<Option<AutomaticHistory>> {
        let approved_mode = mode;
        let limit = context.model_exchange().len().saturating_add(1);
        let snapshot = cancel
            .run(self.underlying.get_compaction_snapshot(limit))
            .await??;
        let (mut selected, complete) = if let Some(snapshot) = &snapshot {
            (snapshot.items().to_vec(), snapshot.complete())
        } else {
            let items = cancel.run(self.underlying.get_items(Some(limit))).await??;
            let complete = items.len() < limit;
            (items, complete)
        };
        let mut remaining = context.model_exchange().iter().rev();
        let mut matched_count = 0;
        for item in selected.iter().rev() {
            let Some(input) = item.to_model_input() else {
                break;
            };
            // An item that cannot be fingerprinted was never recorded as model-visible either.
            let Ok(digest) = cancel.run(self.model_item_digest(&input)).await? else {
                break;
            };
            if !remaining.any(|candidate| candidate == &digest) {
                break;
            }
            matched_count += 1;
        }
        let mut suffix_start = selected.len() - matched_count;
        if matched_count == 0 || (snapshot.is_none() && (!complete || suffix_start > 0)) {
            tracing::warn!(
                "Skipped automatic compaction because stored history could not be matched to the latest model exchange"
            );
            return Ok(None);
        }
        let partial = !complete || suffix_start > 0;
        if partial {
            if args.compaction_mode.unwrap_or(self.mode)
                == OpenAiResponsesCompactionMode::PreviousResponseId
            {
                return Ok(None);
            }
            mode = OpenAiResponsesCompactionMode::Input;
        }
        selected = selected[suffix_start..].to_vec();
        if partial {
            let Some(boundary) = selected.iter().position(user_message) else {
                return Ok(None);
            };
            suffix_start += boundary;
            selected = selected[boundary..].to_vec();
            if !suffix_is_intact(&selected) {
                return Ok(None);
            }
        }
        let wire = cancel.run(lower_items(&selected, &self.provider)).await??;
        if (wire != approved_wire || mode != approved_mode)
            && !self.decision(response_id, mode, wire)
        {
            return Ok(None);
        }
        Ok(Some(AutomaticHistory {
            items: selected,
            mode,
            snapshot,
            suffix_start,
        }))
    }

    async fn replace_with_rollback(
        &self,
        output: Vec<RunItem>,
        previous: Vec<RunItem>,
        cancel: &CancelScope,
    ) -> Result<()> {
        // A store may detach blocking writes when its future drops. Drain each mutation before
        // inspecting or restoring history, so a late clear/add cannot overwrite the rollback.
        let clear = match cancel.ensure_not_cancelled() {
            Ok(()) => self.underlying.clear().await,
            Err(error) => Err(error),
        };
        let cleared = clear.is_ok();
        let result = match clear {
            Ok(()) => {
                async {
                    cancel.ensure_not_cancelled()?;
                    if !output.is_empty() {
                        self.underlying.add_items(output).await?;
                    }
                    cancel.ensure_not_cancelled()
                }
                .await
            }
            other => other,
        };
        if let Err(error) = result {
            let restore = if cleared {
                self.restore_items(previous, true).await
            } else {
                match self.all_items().await {
                    Ok(current) if current == previous => Ok(()),
                    Ok(_) => self.restore_items(previous, false).await,
                    Err(inspect) => Err(inspect),
                }
            };
            if let Err(restore) = restore {
                tracing::warn!(
                    error.code = restore.code(),
                    "Failed to restore session history after compaction replacement failed"
                );
            }
            return Err(error);
        }
        Ok(())
    }

    async fn restore_items(&self, items: Vec<RunItem>, clear: bool) -> Result<()> {
        if clear {
            self.underlying.clear().await?;
        }
        if !items.is_empty() {
            self.underlying.add_items(items).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Session for OpenAiResponsesCompactionSession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    fn compaction(&self) -> Option<&dyn SessionCompaction> {
        Some(self)
    }
    fn ignore_ids_for_matching(&self) -> bool {
        self.underlying.ignore_ids_for_matching()
    }
    fn prepare_items_for_persistence(&self, items: Vec<RunItem>) -> Vec<RunItem> {
        self.underlying.prepare_items_for_persistence(items)
    }
    async fn item_digest_for_persistence(&self, item: &RunItem) -> Result<InputItemDigest> {
        self.underlying.item_digest_for_persistence(item).await
    }
    fn matches_reconstructed_history_item(&self, item: &RunItem) -> bool {
        self.underlying.matches_reconstructed_history_item(item)
    }
    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        self.underlying.get_items(limit).await
    }
    async fn get_items_with_generation(
        &self,
        limit: Option<usize>,
    ) -> Result<(Vec<RunItem>, Option<u64>)> {
        let state = self.state.lock().await;
        Ok((
            self.underlying.get_items(limit).await?,
            Some(state.generation),
        ))
    }
    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        self.add_items_with_generation(items, None).await.map(drop)
    }
    async fn add_items_with_generation(
        &self,
        items: Vec<RunItem>,
        expected: Option<u64>,
    ) -> Result<Option<u64>> {
        let mut state = Arc::clone(&self.state).lock_owned().await;
        let underlying = Arc::clone(&self.underlying);
        tokio::spawn(async move {
            let owned = expected == Some(state.generation);
            let result = underlying.add_items(items.clone()).await;
            state.generation = state.generation.wrapping_add(1);
            match result {
                Ok(()) => {
                    if let Some(cached) = &mut state.cached_items {
                        cached.extend(items);
                    }
                    Ok(owned.then_some(state.generation))
                }
                Err(error) => {
                    state.cached_items = None;
                    Err(error)
                }
            }
        })
        .await
        .map_err(|error| Error::caller("Session append task failed").with_source(error))?
    }
    async fn pop_item(&self) -> Result<Option<RunItem>> {
        let mut state = Arc::clone(&self.state).lock_owned().await;
        let underlying = Arc::clone(&self.underlying);
        tokio::spawn(async move {
            let result = underlying.pop_item().await;
            if !matches!(result, Ok(None)) {
                state.invalidate();
            }
            result
        })
        .await
        .map_err(|error| Error::caller("Session pop task failed").with_source(error))?
    }
    async fn clear(&self) -> Result<()> {
        let mut state = Arc::clone(&self.state).lock_owned().await;
        let underlying = Arc::clone(&self.underlying);
        tokio::spawn(async move {
            let result = underlying.clear().await;
            state.invalidate();
            if result.is_ok() {
                state.cached_items = Some(Vec::new());
            }
            result
        })
        .await
        .map_err(|error| Error::caller("Session clear task failed").with_source(error))?
    }
}

#[async_trait]
impl SessionCompaction for OpenAiResponsesCompactionSession {
    async fn model_item_digest(&self, item: &ModelInputItem) -> Result<InputItemDigest> {
        digest_item(item, self.ignore_ids_for_matching(), &self.provider).await
    }
    async fn model_request_digests(&self, request: &ModelRequest) -> Result<Vec<InputItemDigest>> {
        digest_request(request, self.ignore_ids_for_matching(), &self.provider).await
    }
    fn response_stored(&self, request: &ModelRequest) -> Option<bool> {
        Some(
            request
                .model_settings()
                .extra_body()
                .get("store")
                .and_then(Value::as_bool)
                .unwrap_or(!matches!(
                    request.continuation(),
                    ConversationContinuation::None
                )),
        )
    }
    async fn after_turn(
        &self,
        context: &SessionCompactionContext,
        has_local_tool_outputs: bool,
        cancel: &CancelScope,
    ) -> SessionCompactionOutcome {
        let Some(response_id) = context.response_id() else {
            return SessionCompactionOutcome::new(Usage::default(), Ok(()));
        };
        if has_local_tool_outputs {
            let result = async {
                let mut state = cancel.run(self.state.lock()).await?;
                if state.deferred_response_id.is_some() {
                    return Ok(());
                }
                let args = OpenAiResponsesCompactionArgs {
                    store: context.response_stored(),
                    ..Default::default()
                };
                let mode = self.resolve_mode(&state, &args, Some(response_id));
                let items = cancel.run(self.cached_items(&mut state)).await??;
                let wire = cancel.run(decision_items(&items, &self.provider)).await??;
                if self.decision(Some(response_id.to_owned()), mode, wire) {
                    state.deferred_response_id = Some(response_id.to_owned());
                }
                tracing::debug!("Deferring session compaction due to local tool outputs");
                Ok(())
            }
            .await;
            return SessionCompactionOutcome::new(Usage::default(), result);
        }
        let force = match cancel.run(self.deferred_compaction_response_id()).await {
            Ok(deferred) => deferred.is_some(),
            Err(error) => {
                return SessionCompactionOutcome::new(Usage::default(), Err(error));
            }
        };
        self.compact(
            OpenAiResponsesCompactionArgs {
                response_id: Some(response_id.to_owned()),
                store: context.response_stored(),
                force,
                compaction_mode: None,
            },
            Some(context),
            cancel,
        )
        .await
    }
}
