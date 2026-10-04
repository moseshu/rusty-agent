//! Structured compaction tracing is isolated from tests that execute without a subscriber.

use ra_core::item::{ItemId, Message, RunItem, RunItemKind};
use ra_model::openai::{
    auth::OpenAiAuth,
    compaction::{
        OpenAiResponsesCompactionArgs as Args, OpenAiResponsesCompactionSession as CompactSession,
    },
};
use ra_session::InMemorySession;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[derive(Clone)]
struct TraceWriter(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TraceWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn forced_compaction_emits_structured_trace_without_history_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses/compact"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": [{"type":"compaction", "encrypted_content":"opaque"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let backend = Arc::new(InMemorySession::new_with_items(
        "base",
        vec![RunItem::new(
            ItemId::new("u"),
            RunItemKind::Message(Message::user("private history")),
        )],
    ));
    let writer = TraceWriter(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(writer.clone())
        .finish();
    CompactSession::new(
        "compact",
        backend,
        OpenAiAuth::new("test-key").with_base_url(format!("{}/v1", server.uri())),
    )
    .unwrap()
    .run_compaction(Some(Args {
        force: true,
        ..Default::default()
    }))
    .with_subscriber(subscriber)
    .await
    .unwrap();
    let trace = String::from_utf8(writer.0.lock().unwrap().clone()).unwrap();
    assert!(trace.contains("compaction.force=true"), "{trace}");
    assert!(trace.contains("compaction.replaced=true"), "{trace}");
    assert!(!trace.contains("private history"));
}
