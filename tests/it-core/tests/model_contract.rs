//! R1-3 contracts for provider-neutral model requests and object-safe model traits.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    item::{AgentId, ItemId, Message, ModelInputItem, ModelResponse},
    model::{
        ConversationContinuation, Model, ModelHandoffDefinition, ModelOutputSchema, ModelProvider,
        ModelRequest, ModelRetryAdviceRequest, ModelSettings, ModelStream, ModelStreamEvent,
        ModelToolDefinition, ModelTracing, ProviderKey, RawResponseEvent, ReplaySafety,
        RetryAdvice,
    },
};
use serde_json::json;

fn resolved_settings() -> ra_core::model::ResolvedModelSettings {
    let empty = ModelSettings::new();
    empty.resolve(&ProviderKey::new("test"), &empty, &empty, &empty)
}

fn request() -> ModelRequest {
    ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("hello"))],
        resolved_settings(),
    )
}

#[test]
fn model_request_覆盖完整协议中立参数面() {
    let tool = ModelToolDefinition::new(
        "search",
        json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    )
    .with_description("Search documents")
    .with_strict(true);
    let handoff = ModelHandoffDefinition::new(
        AgentId::new("agent-review"),
        "delegate_review",
        json!({"type": "object"}),
    )
    .with_description("Delegate review")
    .with_strict(true);
    let output_schema =
        ModelOutputSchema::new("answer", json!({"type": "object"})).with_strict(true);

    let request = request()
        .with_system_instructions("Be precise")
        .with_tools(vec![tool])
        .with_handoffs(vec![handoff])
        .with_output_schema(output_schema)
        .with_tracing(ModelTracing::EnabledWithoutData)
        .with_previous_response_id("resp-1");

    assert_eq!(request.system_instructions(), Some("Be precise"));
    assert_eq!(request.input().len(), 1);
    assert_eq!(request.model_settings().provider().as_str(), "test");
    assert_eq!(request.tools()[0].name(), "search");
    assert_eq!(request.tools()[0].description(), Some("Search documents"));
    assert!(request.tools()[0].strict());
    assert_eq!(request.handoffs()[0].target_agent().as_str(), "agent-review");
    assert_eq!(request.handoffs()[0].name(), "delegate_review");
    assert!(request.handoffs()[0].strict());
    assert_eq!(request.output_schema().expect("已设置").name(), "answer");
    assert!(request.output_schema().expect("已设置").strict());
    assert_eq!(request.tracing(), ModelTracing::EnabledWithoutData);
    assert_eq!(
        request.continuation().previous_response_id(),
        Some("resp-1")
    );
}

#[test]
fn 两种服务端续接模式在类型上互斥() {
    let request = request()
        .with_previous_response_id("resp-old")
        .with_conversation_id("conv-new");

    assert_eq!(request.continuation().previous_response_id(), None);
    assert_eq!(request.continuation().conversation_id(), Some("conv-new"));
    assert!(request.continuation().is_server_managed());

    let none = ConversationContinuation::None;
    assert!(!none.is_server_managed());
}

#[test]
fn tracing_拓扑与敏感数据是两个开关语义() {
    assert!(ModelTracing::Disabled.is_disabled());
    assert!(!ModelTracing::Disabled.include_data());
    assert!(!ModelTracing::Enabled.is_disabled());
    assert!(ModelTracing::Enabled.include_data());
    assert!(!ModelTracing::EnabledWithoutData.is_disabled());
    assert!(!ModelTracing::EnabledWithoutData.include_data());
}

#[derive(Default)]
struct FakeModel {
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl Model for FakeModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.calls
            .lock()
            .expect("测试 mutex 不应 poisoned")
            .push("response".to_owned());
        Ok(ModelResponse::new(
            request.input().iter().cloned().map(|item| match item {
                ModelInputItem::Message(message) => ra_core::item::RunItem::new(
                    ItemId::new("item-1"),
                    ra_core::item::RunItemKind::Message(message),
                )
                .with_provenance(ra_core::item::ItemProvenance::new(AgentId::new("agent"))),
                _ => unreachable!("fixture 只有消息"),
            }).collect(),
        ))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        self.calls
            .lock()
            .expect("测试 mutex 不应 poisoned")
            .push("stream".to_owned());
        stream::once(async {
            Ok(ModelStreamEvent::RawResponse(RawResponseEvent::new(
                ProviderKey::new("test"),
                "response.delta",
                json!({"delta": "hello"}),
            )))
        })
        .boxed()
    }

    fn get_retry_advice(
        &self,
        request: &ModelRetryAdviceRequest<'_>,
    ) -> Option<RetryAdvice> {
        request.is_streaming().then(|| {
            RetryAdvice::new()
                .with_suggested(true)
                .with_retry_after(Duration::from_millis(25))
                .with_replay_safety(ReplaySafety::Safe)
                .with_reason("stream not accepted")
        })
    }
}

struct FakeProvider {
    model: Arc<dyn Model>,
    names: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl ModelProvider for FakeProvider {
    fn get_model(&self, model_name: Option<&str>) -> Result<Arc<dyn Model>> {
        self.names
            .lock()
            .expect("测试 mutex 不应 poisoned")
            .push(model_name.map(str::to_owned));
        Ok(Arc::clone(&self.model))
    }
}

#[tokio::test]
async fn model_与_provider_trait_可作为动态对象使用() {
    let concrete = Arc::new(FakeModel::default());
    let model: Arc<dyn Model> = concrete.clone();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider {
        model,
        names: Mutex::new(Vec::new()),
    });

    let resolved = provider.get_model(Some("demo")).expect("应解析模型");
    let response = resolved
        .get_response(request())
        .await
        .expect("非流式调用应成功");
    assert_eq!(response.output().len(), 1);

    let event = resolved
        .stream_response(request())
        .next()
        .await
        .expect("应有流事件")
        .expect("流事件应成功");
    match event {
        ModelStreamEvent::RawResponse(raw) => {
            assert_eq!(raw.provider().as_str(), "test");
            assert_eq!(raw.event_type(), "response.delta");
        }
        _ => panic!("fixture 应产生 raw response"),
    }

    let error = Error::provider(ProviderErrorKind::Network, "disconnected");
    let continuation = ConversationContinuation::None;
    let advice_request = ModelRetryAdviceRequest::new(&error, 0, true, &continuation);
    let advice = resolved
        .get_retry_advice(&advice_request)
        .expect("fake model 应给建议");
    assert_eq!(advice.suggested(), Some(true));
    assert_eq!(advice.retry_after(), Some(Duration::from_millis(25)));
    assert_eq!(advice.replay_safety(), ReplaySafety::Safe);

    resolved.close().await.expect("默认 close 应成功");
    provider.close().await.expect("默认 provider close 应成功");
    assert_eq!(
        concrete
            .calls
            .lock()
            .expect("测试 mutex 不应 poisoned")
            .as_slice(),
        ["response", "stream"]
    );
}

#[test]
fn 默认_retry_advice_为空() {
    struct MinimalModel;

    #[async_trait]
    impl Model for MinimalModel {
        async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
            Ok(ModelResponse::new(Vec::new()))
        }

        fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
            stream::empty().boxed()
        }
    }

    let error = Error::provider(ProviderErrorKind::Timeout, "timeout");
    let continuation = ConversationContinuation::PreviousResponseId("resp-1".to_owned());
    let retry_request = ModelRetryAdviceRequest::new(&error, 1, false, &continuation);

    assert!(MinimalModel.get_retry_advice(&retry_request).is_none());
    assert_eq!(retry_request.attempt(), 1);
    assert_eq!(retry_request.error().code(), "provider.timeout");
    assert_eq!(
        retry_request.continuation().previous_response_id(),
        Some("resp-1")
    );
}

#[test]
fn raw_stream_信封保留未知字段() {
    let encoded = json!({
        "type": "raw_response",
        "data": {
            "schema_version": 2,
            "provider": "test",
            "event_type": "response.delta",
            "payload": {"delta": "hello"},
            "future_transport_hint": {"channel": 3}
        }
    });

    let event: ModelStreamEvent =
        serde_json::from_value(encoded.clone()).expect("新版事件应可降级读取");
    match &event {
        ModelStreamEvent::RawResponse(raw) => {
            assert_eq!(
                raw.unknown().get("future_transport_hint"),
                Some(&json!({"channel": 3}))
            );
        }
        _ => panic!("fixture 应是 raw response"),
    }

    assert_eq!(serde_json::to_value(event).expect("事件应可回写"), encoded);
}

#[test]
fn 模型事件通道不含_run_级事实() {
    // An adapter knows one wire call and nothing about agents or handoffs. Letting it announce
    // "the public agent changed" would make the type permit a permanently invalid state; the run
    // channel is wrapped by the runner inside ra-runtime (R1-7 / R13) rather than by widening this
    // enum.
    let labels: Vec<String> = [
        ModelStreamEvent::RawResponse(RawResponseEvent::new(
            ProviderKey::new("test"),
            "response.delta",
            json!({}),
        )),
        ModelStreamEvent::RunItem(ra_core::model::RunItemStreamEvent::new(
            "message_output_created",
            ra_core::item::RunItem::new(
                ItemId::new("item-1"),
                ra_core::item::RunItemKind::Message(Message::user("hi")),
            ),
        )),
    ]
    .iter()
    .map(|event| {
        serde_json::to_value(event).expect("事件应可序列化")["type"]
            .as_str()
            .expect("tag 应是字符串")
            .to_owned()
    })
    .collect();

    assert_eq!(labels, vec!["raw_response", "run_item"]);
}
