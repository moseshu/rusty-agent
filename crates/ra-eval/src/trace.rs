// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Host-installed local trace capture. Payloads stay in explicit rollout records, never spans.
//!
//! Install one clone as a subscriber layer and another as the run's rollout recorder. The host
//! owns subscriber filtering and calls `flush`/`shutdown` after the run and its spans have ended.
//! Capture is in memory; synchronous callbacks perform no file I/O. An optional existing recorder
//! receives the same trace envelopes for durable session storage. Export runs outside callbacks.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

thread_local! {
    // Only spans enabled for this layer enter this stack; filtered spans retain their ancestor.
    static ENTERED: RefCell<Vec<EnteredSpan>> = const { RefCell::new(Vec::new()) };
}

use async_trait::async_trait;
use ra_core::{
    error::Result,
    item::RunItemKind,
    session::rollout::{
        PersistContext, RolloutItem, RolloutRecorder, RolloutTraceEvent, RolloutTraceMode,
    },
};
use serde_json::{Value, json};
use tracing::{
    Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

pub use ra_core::session::rollout::RolloutTraceRecord;
mod io;
pub use io::{TraceError, read_jsonl, read_rollout, write_jsonl};

#[derive(Clone)]
struct Identity {
    trace: String,
    span: String,
    parent: Option<String>,
}

struct TrackedSpan {
    identity: Identity,
    captured: bool,
}

#[derive(Clone, Copy)]
struct EnteredSpan {
    capture: usize,
    span: u64,
}

struct State {
    records: Vec<RolloutTraceRecord>,
    spans: HashMap<u64, TrackedSpan>,
    closed: bool,
    serialization_failed: bool,
    pending: VecDeque<RolloutItem>,
    dispatching: bool,
}

/// A host-owned capture handle and subscriber layer; cloning shares its ordered record stream.
///
/// Default capture retains topology and metadata only. Use [`Self::with_payloads`] before installing
/// the layer and recorder to retain inputs, responses, tool items and host event payloads. These
/// payloads may contain sensitive user content. Transport headers/query/body are always omitted
/// from model request snapshots. Install at most one `LocalTrace` layer per subscriber, including
/// layers backed by different captures. Each capture belongs to at most one subscriber; recorder
/// clones can be shared across its tasks. Separate subscribers need separate captures.
/// Recorder lifecycle methods return only storage failures. Payload serialization failures remain
/// visible through [`Self::capture_complete`] and the affected records' `capture_error` field.
#[derive(Clone)]
pub struct LocalTrace {
    state: Arc<Mutex<State>>,
    mode: RolloutTraceMode,
    unscoped_trace: String,
    recorder: Option<Arc<dyn RolloutRecorder>>,
    idle: Arc<tokio::sync::Notify>,
}

impl Default for LocalTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalTrace {
    /// Creates a metadata-only local capture with no persistence backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                records: Vec::new(),
                spans: HashMap::new(),
                closed: false,
                serialization_failed: false,
                pending: VecDeque::new(),
                dispatching: false,
            })),
            mode: RolloutTraceMode::Metadata,
            unscoped_trace: new_trace_id(),
            recorder: None,
            idle: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Explicitly enables content capture. Configure before cloning and installing the handle.
    #[must_use]
    pub fn with_payloads(mut self) -> Self {
        self.mode = RolloutTraceMode::Payloads;
        self
    }

    /// Forwards authoritative rollout items and trace envelopes to an existing session recorder.
    /// Configure before cloning and installing the handle. Its lifecycle stays host-owned.
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<dyn RolloutRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Copies the records captured so far, including spans still in progress.
    #[must_use]
    pub fn snapshot(&self) -> Vec<RolloutTraceRecord> {
        self.lock().records.clone()
    }

    /// Whether no payload serialization failure has been observed so far.
    ///
    /// This status is shared by clones and child recorders. Once false, it stays false for the
    /// capture's lifetime, including after flush or shutdown. Inspect `capture_error` in the
    /// snapshot for affected records. This does not report storage failures or whether producers
    /// and spans have finished; stop them before reading the final snapshot and status.
    #[must_use]
    pub fn capture_complete(&self) -> bool {
        !self.lock().serialization_failed
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn capture_key(&self) -> usize {
        Arc::as_ptr(&self.state) as usize
    }

    fn append(&self, state: &mut State, identity: Option<&Identity>, event: RolloutTraceEvent) {
        if state.closed {
            return;
        }
        state.serialization_failed |= event.fields().get("capture_error").is_some();
        let record = RolloutTraceRecord::new(
            self.unscoped_trace.clone(),
            state.records.len() as u64 + 1,
            u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
            identity.map_or_else(|| self.unscoped_trace.clone(), |id| id.trace.clone()),
            identity.map(|id| id.span.clone()),
            identity.and_then(|id| id.parent.clone()),
            event,
        );
        if self.recorder.is_some() {
            state.pending.push_back(RolloutItem::Trace(record.clone()));
        }
        state.records.push(record);
    }

    fn observation(&self, event: RolloutTraceEvent) {
        let current = tracing::dispatcher::get_default(|dispatch| {
            dispatch
                .downcast_ref::<Self>()
                .filter(|capture| Arc::ptr_eq(&capture.state, &self.state))
                .and_then(|_| {
                    ENTERED
                        .try_with(|entered| {
                            entered
                                .borrow()
                                .iter()
                                .rev()
                                .find(|entry| entry.capture == self.capture_key())
                                .map(|entry| entry.span)
                        })
                        .ok()
                        .flatten()
                })
        });
        let mut state = self.lock();
        let identity =
            current.and_then(|id| state.spans.get(&id).map(|span| span.identity.clone()));
        self.append(&mut state, identity.as_ref(), event);
        drop(state);
        self.drain();
    }

    // Recorder callbacks may themselves emit spans. Queue before calling out, then drain without
    // the state lock so recursive observations retain order without deadlocking the subscriber.
    fn drain(&self) {
        let Some(recorder) = &self.recorder else {
            return;
        };
        let mut state = self.lock();
        if state.dispatching {
            return;
        }
        state.dispatching = true;
        loop {
            let Some(item) = state.pending.pop_front() else {
                state.dispatching = false;
                self.idle.notify_waiters();
                break;
            };
            drop(state);
            recorder.record(item);
            state = self.lock();
        }
    }

    fn forward(&self, item: RolloutItem) {
        if self.recorder.is_some() {
            let mut state = self.lock();
            if !state.closed {
                state.pending.push_back(item);
            }
            drop(state);
            self.drain();
        }
    }

    fn content_event<T: serde::Serialize>(
        &self,
        kind: &str,
        mut fields: Value,
        value: &T,
    ) -> RolloutTraceEvent {
        let payload = if self.mode == RolloutTraceMode::Payloads {
            if let Ok(value) = serde_json::to_value(value) {
                Some(value)
            } else {
                fields["capture_error"] = json!("serialization");
                None
            }
        } else {
            None
        };
        RolloutTraceEvent::new(kind, fields, payload)
    }

    async fn drain_pending(&self) {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            self.drain();
            if !self.lock().dispatching {
                break;
            }
            idle.await;
        }
    }
}

impl LocalTrace {
    fn observe_item(&self, item: &RolloutItem) {
        // A diagnostic copy must obey the same host-event retention policy as session history.
        if matches!(item, RolloutItem::Event(_))
            && !ra_session::rollout::is_persisted_rollout_item(item)
        {
            return;
        }
        let event = match item {
            RolloutItem::RunStarted(start) => self.content_event(
                "run_started",
                json!({"run.id": start.run_id(), "parent_run_id": start.parent_run_id(), "agent.id": start.agent_id(), "input_count": start.input().len()}),
                start,
            ),
            RolloutItem::RunEnded(end) => self.content_event(
                "run_ended",
                json!({"run.id": end.run_id(), "status": end.end(), "finish.reason": end.finish_reason()}),
                end,
            ),
            RolloutItem::Item(item) => {
                let mut fields = json!({"item.id": item.id(), "item.kind": item.kind().label()});
                match item.kind() {
                    RunItemKind::ToolCall(call) => {
                        fields["tool.call_id"] = json!(call.call_id());
                        fields["tool.name"] = json!(call.name());
                    }
                    RunItemKind::ToolCallOutput(output) => {
                        fields["tool.call_id"] = json!(output.call_id());
                    }
                    _ => {}
                }
                self.content_event("item", fields, item)
            }
            RolloutItem::ModelUsage(usage) => self.content_event(
                "model_usage",
                json!({"run.id": usage.run_id(), "rollout.turn_index": usage.turn_index(), "usage": usage_metadata(usage.usage())}),
                usage,
            ),
            RolloutItem::Event(event) => self.content_event(
                "host_event",
                json!({"run.id": event.run_id(), "agent.id": event.agent_id(), "event.seq": event.seq()}),
                event,
            ),
            RolloutItem::TurnContext(context) => self.content_event(
                "turn_context",
                json!({"run.id": context.run_id(), "rollout.turn_index": context.turn_index(), "model.name": context.model()}),
                context,
            ),
            _ => {
                return;
            }
        };
        self.observation(event);
    }
}

fn new_trace_id() -> String {
    format!("trace_{}", uuid::Uuid::new_v4().simple())
}

// Descendants share raw evidence, but keep their history and storage lifecycle independent.
struct ChildTrace {
    capture: LocalTrace,
    recorder: Option<Arc<dyn RolloutRecorder>>,
    closed: AtomicBool,
}

#[async_trait]
impl RolloutRecorder for ChildTrace {
    fn child_trace(
        &self,
        recorder: Option<Arc<dyn RolloutRecorder>>,
    ) -> Option<Arc<dyn RolloutRecorder>> {
        self.capture.child_trace(recorder)
    }

    fn trace_mode(&self) -> RolloutTraceMode {
        self.capture.trace_mode()
    }

    fn record_trace(&self, event: RolloutTraceEvent) {
        self.capture.record_trace(event);
    }

    fn record(&self, item: RolloutItem) {
        self.capture.observe_item(&item);
        if let Some(recorder) = &self.recorder {
            recorder.record(item);
        }
    }

    async fn flush(&self) -> Result<()> {
        self.capture.drain_pending().await;
        if let Some(recorder) = &self.recorder {
            recorder.flush().await?;
        }
        Ok(())
    }

    async fn persist(&self, context: PersistContext) -> Result<()> {
        self.capture.drain_pending().await;
        if let Some(recorder) = &self.recorder {
            recorder.persist(context).await?;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        self.capture.drain_pending().await;
        if let Some(recorder) = &self.recorder {
            recorder.shutdown().await?;
        }
        self.closed.store(true, Ordering::Release);
        Ok(())
    }

    async fn discard(&self) -> Result<()> {
        if let Some(recorder) = &self.recorder {
            recorder.discard().await?;
        }
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

#[async_trait]
impl RolloutRecorder for LocalTrace {
    fn child_trace(
        &self,
        recorder: Option<Arc<dyn RolloutRecorder>>,
    ) -> Option<Arc<dyn RolloutRecorder>> {
        Some(Arc::new(ChildTrace {
            capture: self.clone(),
            recorder,
            closed: AtomicBool::new(false),
        }))
    }
    fn trace_mode(&self) -> RolloutTraceMode {
        self.mode
    }

    fn record_trace(&self, event: RolloutTraceEvent) {
        self.observation(if self.mode == RolloutTraceMode::Payloads {
            event
        } else {
            event.without_payload()
        });
    }

    fn record(&self, item: RolloutItem) {
        self.observe_item(&item);
        self.forward(item);
    }

    async fn flush(&self) -> Result<()> {
        self.drain_pending().await;
        if let Some(recorder) = &self.recorder {
            recorder.flush().await?;
        }
        Ok(())
    }

    async fn persist(&self, context: PersistContext) -> Result<()> {
        self.drain_pending().await;
        if let Some(recorder) = &self.recorder {
            recorder.persist(context).await?;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        if self.lock().closed {
            return Ok(());
        }
        self.drain_pending().await;
        if let Some(recorder) = &self.recorder {
            recorder.shutdown().await?;
        }
        self.lock().closed = true;
        Ok(())
    }

    async fn discard(&self) -> Result<()> {
        if let Some(recorder) = &self.recorder {
            recorder.discard().await?;
        }
        self.lock().closed = true;
        Ok(())
    }
}

// Usage preserves arbitrary provider extensions for replay. Only known counters are metadata.
fn usage_metadata(usage: &ra_core::usage::Usage) -> Value {
    json!({
        "requests": usage.requests(),
        "input_tokens": usage.input_tokens(),
        "output_tokens": usage.output_tokens(),
        "total_tokens": usage.total_tokens(),
        "cached_input_tokens": usage.cached_input_tokens(),
        "cache_write_tokens": usage.cache_write_tokens(),
        "reasoning_tokens": usage.reasoning_tokens(),
    })
}

#[derive(Default)]
struct Fields(BTreeMap<String, Value>);
impl Fields {
    fn insert(&mut self, field: &Field, value: Value) {
        if ra_core::trace::field::ALL.contains(&field.name()) {
            self.0.insert(field.name().into(), value);
        }
    }
}
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field, json!(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, json!(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, json!(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, json!(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, json!(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, json!(value));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for LocalTrace {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        let mut state = self.lock();
        let mut parent = context.span(id).and_then(|span| span.parent());
        let mut parent_identity = None;
        while let Some(span) = parent {
            if let Some(tracked) = state.spans.get(&span.id().into_u64()) {
                parent_identity = Some(tracked.identity.clone());
                break;
            }
            parent = span.parent();
        }
        if !fields.0.contains_key(ra_core::trace::field::SPAN_KIND) {
            if let Some(identity) = parent_identity {
                state.spans.insert(
                    id.into_u64(),
                    TrackedSpan {
                        identity,
                        captured: false,
                    },
                );
                drop(state);
                self.track_entered_span(id, &context);
            }
            return;
        }
        let identity = Identity {
            trace: parent_identity
                .as_ref()
                .map_or_else(new_trace_id, |id| id.trace.clone()),
            span: format!("span_{}", &uuid::Uuid::new_v4().simple().to_string()[..24]),
            parent: parent_identity.map(|id| id.span),
        };
        self.append(
            &mut state,
            Some(&identity),
            RolloutTraceEvent::new("span_started", json!(fields.0), None),
        );
        state.spans.insert(
            id.into_u64(),
            TrackedSpan {
                identity,
                captured: true,
            },
        );
        drop(state);
        self.track_entered_span(id, &context);
        self.drain();
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        values.record(&mut fields);
        if fields.0.is_empty() {
            return;
        }
        let mut state = self.lock();
        if let Some(identity) = state
            .spans
            .get(&id.into_u64())
            .filter(|span| span.captured)
            .map(|span| span.identity.clone())
        {
            self.append(
                &mut state,
                Some(&identity),
                RolloutTraceEvent::new("span_updated", json!(fields.0), None),
            );
        }
        drop(state);
        self.drain();
    }

    fn on_enter(&self, id: &Id, context: Context<'_, S>) {
        if let Some(entry) = context
            .span(id)
            .and_then(|span| span.extensions().get::<EnteredSpan>().copied())
            .filter(|entry| entry.capture == self.capture_key())
        {
            let _ = ENTERED.try_with(|entered| {
                entered.borrow_mut().push(entry);
            });
        }
    }

    fn on_exit(&self, id: &Id, _: Context<'_, S>) {
        let _ = ENTERED.try_with(|entered| {
            let mut entered = entered.borrow_mut();
            if let Some(index) = entered.iter().rposition(|entry| {
                entry.capture == self.capture_key() && entry.span == id.into_u64()
            }) {
                entered.remove(index);
            }
        });
    }

    fn on_close(&self, id: Id, _: Context<'_, S>) {
        let mut state = self.lock();
        if let Some(tracked) = state.spans.remove(&id.into_u64())
            && tracked.captured
        {
            self.append(
                &mut state,
                Some(&tracked.identity),
                RolloutTraceEvent::new("span_ended", json!({}), None),
            );
        }
        drop(state);
        self.drain();
    }
}

impl LocalTrace {
    fn track_entered_span<S: Subscriber + for<'a> LookupSpan<'a>>(
        &self,
        id: &Id,
        context: &Context<'_, S>,
    ) {
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(EnteredSpan {
                capture: self.capture_key(),
                span: id.into_u64(),
            });
        }
    }
}
