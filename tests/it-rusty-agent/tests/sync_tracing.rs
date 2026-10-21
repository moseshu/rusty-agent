//! `run_sync` polls the run on a thread of its own; the caller's tracing context goes with it.
//!
//! A test binary of its own: a thread-local default subscriber shares tracing's global callsite
//! interest cache with every other thread in the process, and a concurrent test on another thread
//! can leave a callsite cached as disabled, so the assertion would depend on scheduling.

use std::sync::{Arc, Mutex};

use agents::{
    item::{ItemId, ModelResponse, OutputPhase},
    model::{Model, ModelProvider, ModelRequest},
    prelude::*,
    provider::quirks::ProviderQuirks,
};
use async_trait::async_trait;
use tracing_subscriber::layer::SubscriberExt as _;

struct Answer;

#[async_trait]
impl Model for Answer {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new("answer"),
            RunItemKind::Message(Message::assistant("traced", OutputPhase::Final)),
        )]))
    }
}

struct Provider;

#[async_trait]
impl ModelProvider for Provider {
    fn get_model(&self, _: Option<&str>) -> Result<Arc<dyn Model>> {
        Ok(Arc::new(Answer))
    }
}

/// Records each new span's name with its parent's.
struct ParentRecorder(Arc<Mutex<Vec<(String, Option<String>)>>>);

impl<S> tracing_subscriber::Layer<S> for ParentRecorder
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        _: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = context.span(id).unwrap();
        let parent = span.parent().map(|parent| parent.name().to_owned());
        self.0
            .lock()
            .unwrap()
            .push((span.name().to_owned(), parent));
    }
}

// The run is polled on another thread, but a thread-local default subscriber and the caller's
// current span are where a host expects the run's spans: under its own span, in its subscriber.
#[test]
fn sync_keeps_the_callers_subscriber_and_span() {
    let registry = Arc::new(
        ProviderRegistry::builder(ProviderKey::new("test"))
            .register(ProviderRegistration::new(
                ProviderKey::new("test"),
                ApiProtocol::OpenAiResponses,
                |_: ProviderQuirks| Ok(Arc::new(Provider) as Arc<dyn ModelProvider>),
            ))
            .build()
            .unwrap(),
    );
    let agent = AgentSpec::builder()
        .id(AgentId::new("traced"))
        .name("Traced")
        .instructions("Answer.")
        .build()
        .unwrap();
    let runner = AgentRunner::new(registry);
    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(ParentRecorder(Arc::clone(&spans)));
    let text = tracing::subscriber::with_default(subscriber, || {
        let host = tracing::info_span!("host_call");
        let _entered = host.enter();
        runner.run_sync(agent, "hello").unwrap().final_text().to_owned()
    });
    assert_eq!(text, "traced");
    let spans = spans.lock().unwrap();
    assert!(
        spans.contains(&("agent".to_owned(), Some("host_call".to_owned()))),
        "{spans:?}"
    );
}
