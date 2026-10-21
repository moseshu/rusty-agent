//! Local spans exclude fixture payloads and retain task-safe stream cleanup.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use futures::{StreamExt, stream};
use ra_core::{
    error::{Error, ProviderErrorKind},
    model::{Model, ModelRequest, ModelSettings, ModelTracing, ProviderKey, SettingsLayers},
};
use ra_eval::fixture::{ModelStep, ScriptedModel, assistant_message};
use tracing::{
    Instrument, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{
    Layer,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
};

#[derive(Clone, Default)]
struct Recorded(Arc<Mutex<Vec<String>>>);

impl Visit for Recorded {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .lock()
            .unwrap()
            .push(format!("{}={value:?}", field.name()));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Recorded {
    fn on_new_span(&self, attrs: &Attributes<'_>, _: &Id, _: Context<'_, S>) {
        self.0.lock().unwrap().push(attrs.metadata().name().into());
        attrs.record(&mut self.clone());
    }

    fn on_record(&self, _: &Id, values: &Record<'_>, _: Context<'_, S>) {
        values.record(&mut self.clone());
    }

    fn on_close(&self, id: Id, context: Context<'_, S>) {
        let span = context.span(&id).unwrap();
        self.0
            .lock()
            .unwrap()
            .push(format!("closed:{}", span.name()));
    }
}

fn request() -> ModelRequest {
    let settings = ModelSettings::new();
    ModelRequest::new(
        Vec::new(),
        SettingsLayers::new()
            .with_provider(&settings)
            .resolve(&ProviderKey::new("fixture")),
    )
    .with_system_instructions("secret fixture instructions")
    .with_tracing(ModelTracing::Enabled)
}

#[tokio::test]
async fn traces_are_opt_in_and_only_record_stable_metadata_even_when_data_tracing_is_enabled() {
    let recorded = Recorded::default();
    let subscriber = tracing_subscriber::registry().with(recorded.clone());
    let _default = tracing::subscriber::set_default(subscriber);
    let model = ScriptedModel::new([Error::provider(
        ProviderErrorKind::Network,
        "secret error body",
    )]);
    model.get_response(request()).await.unwrap_err();
    assert!(recorded.0.lock().unwrap().is_empty());
    let model = ScriptedModel::new([Error::provider(
        ProviderErrorKind::Network,
        "secret error body",
    )])
    .with_emit_traces(true);
    drop(model.stream_response(request()));
    assert!(recorded.0.lock().unwrap().is_empty());
    model.get_response(request()).await.unwrap_err();
    let fields = recorded.0.lock().unwrap().clone();
    assert!(fields.iter().any(|value| value == "generation"));
    assert!(
        fields
            .iter()
            .any(|value| value.contains("error.code") && value.contains("provider.network"))
    );
    assert!(!fields.iter().any(|value| value.contains("secret")));
    drop(fields);

    recorded.0.lock().unwrap().clear();
    let model = ScriptedModel::new([ModelStep::default()]).with_emit_traces(true);
    let mut stream = model.stream_response(request());
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    let records = recorded.0.lock().unwrap().clone();
    assert_eq!(
        records
            .iter()
            .filter(|value| *value == "closed:generation")
            .count(),
        1
    );
    assert!(stream.next().await.is_none());
    assert_eq!(*recorded.0.lock().unwrap(), records);
}

struct Cleanup {
    in_generation_span: Arc<AtomicBool>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.in_generation_span.store(
            tracing::Span::current()
                .metadata()
                .is_some_and(|metadata| metadata.name() == "generation"),
            Ordering::SeqCst,
        );
    }
}

#[tokio::test]
async fn inner_stream_drops_in_its_generation_span_without_leaking_into_another_task() {
    let subscriber = tracing_subscriber::registry().with(Recorded::default());
    let _default = tracing::subscriber::set_default(subscriber);
    let in_generation = Arc::new(AtomicBool::new(false));
    let capture = Arc::clone(&in_generation);
    let model = ScriptedModel::new([ModelStep::stream_factory(move |_| {
        let guard = Cleanup {
            in_generation_span: Arc::clone(&capture),
        };
        Box::pin(stream::unfold(guard, |guard| async {
            Some((Err(Error::caller("stream failure")), guard))
        }))
    })])
    .with_emit_traces(true);
    let mut stream = model.stream_response(request());
    let first_task = tracing::info_span!("first_task");
    stream
        .next()
        .instrument(first_task)
        .await
        .unwrap()
        .unwrap_err();
    assert!(tracing::Span::current().metadata().is_none());
    let second_task = tracing::info_span!("second_task");
    second_task.in_scope(|| drop(stream));
    assert!(in_generation.load(Ordering::SeqCst));
    assert!(tracing::Span::current().metadata().is_none());
}

#[tokio::test]
async fn generation_spans_record_ok_error_and_cancelled_outcomes() {
    let recorded = Recorded::default();
    let subscriber = tracing_subscriber::registry().with(recorded.clone());
    let _default = tracing::subscriber::set_default(subscriber);
    let outcomes = || {
        recorded
            .0
            .lock()
            .unwrap()
            .drain(..)
            .filter(|value| value.starts_with("outcome="))
            .collect::<Vec<_>>()
    };

    let model = ScriptedModel::new([
        ModelStep::default(),
        Error::provider(ProviderErrorKind::Network, "failure").into(),
        ModelStep::new(vec![assistant_message("partial", None)]),
        ModelStep::respond_async(|_| futures::future::pending()),
    ])
    .with_emit_traces(true);
    model.get_response(request()).await.unwrap();
    assert_eq!(outcomes(), [r#"outcome="ok""#]);
    model.get_response(request()).await.unwrap_err();
    assert_eq!(outcomes(), [r#"outcome="error""#]);

    let mut stream = model.stream_response(request());
    stream.next().await.unwrap().unwrap();
    drop(stream);
    assert_eq!(outcomes(), [r#"outcome="cancelled""#]);

    let mut pending = Box::pin(model.get_response(request()));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    drop(pending);
    assert_eq!(outcomes(), [r#"outcome="cancelled""#]);
    model.assert_complete().unwrap();
}
