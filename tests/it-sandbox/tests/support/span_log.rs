//! A `tracing` layer that keeps every closed span's fields, in closing order.
//!
//! The reference's sandbox span tests read spans back from an in-memory trace processor; this is the
//! same thing for `tracing`. Installed per test with [`SpanLog::install`], which only affects the
//! current thread — every test that uses it runs on a single-threaded runtime.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;

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
    closed: Arc<Mutex<Vec<ClosedSpan>>>,
}

impl SpanLog {
    /// Records spans on this thread until the guard is dropped.
    pub fn install() -> (Self, DefaultGuard) {
        let log = Self::default();
        let subscriber = tracing_subscriber::registry().with(log.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        (log, guard)
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

impl<S> Layer<S> for SpanLog
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut span = ClosedSpan {
            name: attrs.metadata().name().to_owned(),
            fields: BTreeMap::new(),
        };
        attrs.record(&mut FieldVisitor(&mut span.fields));
        if let Some(entry) = ctx.span(id) {
            entry.extensions_mut().insert(span);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(entry) = ctx.span(id)
            && let Some(span) = entry.extensions_mut().get_mut::<ClosedSpan>()
        {
            values.record(&mut FieldVisitor(&mut span.fields));
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if let Some(entry) = ctx.span(&id)
            && let Some(span) = entry.extensions_mut().remove::<ClosedSpan>()
        {
            self.closed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(span);
        }
    }
}
