//! A `tracing` layer that keeps every closed span's fields, in closing order.
//!
//! The reference's sandbox span tests read spans back from an in-memory trace processor; this is the
//! same thing for `tracing`. [`SpanLog::install`] gives the current thread a log of its own — every
//! test that uses it runs on a single-threaded runtime, so its spans open on its thread.
//!
//! # One global subscriber, routed per thread
//!
//! The layer is installed once, as the process's global subscriber, and each span goes to the log
//! of the thread that opened it. A per-thread `set_default` subscriber per test is not enough:
//! `tracing` keeps each callsite's interest and the maximum enabled level process-wide, and
//! rebuilds them as other tests' subscribers come and go. Under parallel tests that let a span open
//! while its callsite was judged uninteresting, so it was never recorded — on Linux in about half
//! the runs of a whole binary, never on the macOS machine this was written on. A global subscriber
//! that is always interested keeps those caches from ever ruling a callsite out.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;

type Closed = Arc<Mutex<Vec<ClosedSpan>>>;

thread_local! {
    /// Where spans opened on this thread are recorded, while a test on it holds a log.
    static CURRENT: RefCell<Option<Closed>> = const { RefCell::new(None) };
}

/// Installs the routing layer as the global subscriber, once per process.
fn install_global() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Router))
            .expect("no other global subscriber in a test binary that uses the span log");
    });
}

/// Stops recording this thread's spans when dropped.
pub struct SpanLogGuard(());

impl Drop for SpanLogGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| current.borrow_mut().take());
    }
}

/// One closed span: its name and every field that was given a value.
#[derive(Debug, Clone, Default)]
pub struct ClosedSpan {
    pub name: String,
    pub fields: BTreeMap<String, String>,
}

impl ClosedSpan {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }

    pub fn kind(&self) -> &str {
        self.field("span.kind").unwrap_or_default()
    }
}

#[derive(Clone, Default)]
pub struct SpanLog {
    closed: Closed,
}

impl SpanLog {
    /// Records spans opened on this thread until the guard is dropped.
    pub fn install() -> (Self, SpanLogGuard) {
        install_global();
        let log = Self::default();
        CURRENT.with(|current| *current.borrow_mut() = Some(Arc::clone(&log.closed)));
        (log, SpanLogGuard(()))
    }

    /// Every closed span so far, in the order they closed.
    pub fn closed(&self) -> Vec<ClosedSpan> {
        self.closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The closed spans of sandbox operations, in the order they closed.
    pub fn sandbox_spans(&self) -> Vec<ClosedSpan> {
        self.closed()
            .into_iter()
            .filter(|span| span.kind().starts_with("sandbox."))
            .collect()
    }
}

struct FieldVisitor<'a>(&'a mut BTreeMap<String, String>);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

/// The global layer: sends each span to the log of the thread that opened it.
struct Router;

/// A span still open, and the log it goes to when it closes.
struct Open {
    span: ClosedSpan,
    log: Closed,
}

impl<S> Layer<S> for Router
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(log) = CURRENT.with(|current| current.borrow().clone()) else {
            return;
        };
        let mut span = ClosedSpan {
            name: attrs.metadata().name().to_owned(),
            fields: BTreeMap::new(),
        };
        attrs.record(&mut FieldVisitor(&mut span.fields));
        if let Some(entry) = ctx.span(id) {
            entry.extensions_mut().insert(Open { span, log });
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(entry) = ctx.span(id)
            && let Some(open) = entry.extensions_mut().get_mut::<Open>()
        {
            values.record(&mut FieldVisitor(&mut open.span.fields));
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if let Some(entry) = ctx.span(&id)
            && let Some(open) = entry.extensions_mut().remove::<Open>()
        {
            open.log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(open.span);
        }
    }
}
