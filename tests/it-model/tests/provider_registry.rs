//! R1-3a contracts for provider registration, `provider/model` routing, and lifecycle.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use ra_core::{
    error::{Error, Result},
    item::ModelResponse,
    model::{
        ApiProtocol, Model, ModelProvider, ModelRequest, ModelSettings, ModelStream, ProviderKey,
    },
};
use ra_model::provider::{
    ModelRegistration, ProviderRegistration, ProviderRegistry, UnknownPrefixPolicy,
};
use serde_json::{Map, Value, json};

struct FakeModel;

#[async_trait]
impl Model for FakeModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("fake model is not called by registry tests"))
    }

    fn stream_response(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::empty().boxed()
    }
}

struct FakeProvider {
    requested_models: Mutex<Vec<Option<String>>>,
    close_count: AtomicUsize,
    fail_close: bool,
}

impl FakeProvider {
    fn new(fail_close: bool) -> Self {
        Self {
            requested_models: Mutex::new(Vec::new()),
            close_count: AtomicUsize::new(0),
            fail_close,
        }
    }

    fn requested_models(&self) -> Vec<Option<String>> {
        self.requested_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ModelProvider for FakeProvider {
    fn get_model(&self, model_name: Option<&str>) -> Result<Arc<dyn Model>> {
        self.requested_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(model_name.map(str::to_owned));
        Ok(Arc::new(FakeModel))
    }

    async fn close(&self) -> Result<()> {
        self.close_count.fetch_add(1, Ordering::SeqCst);
        if self.fail_close {
            Err(Error::caller("deliberate close failure"))
        } else {
            Ok(())
        }
    }
}

fn registration(
    key: &str,
    protocol: ApiProtocol,
    provider: Arc<FakeProvider>,
) -> ProviderRegistration {
    ProviderRegistration::new(ProviderKey::new(key), protocol, move || {
        Ok(Arc::clone(&provider) as Arc<dyn ModelProvider>)
    })
}

#[test]
fn 显式注册优先_未知前缀完整交给兼容_provider() {
    let native = Arc::new(FakeProvider::new(false));
    let compat = Arc::new(FakeProvider::new(false));
    let explicit = Arc::new(FakeProvider::new(false));
    let registry = ProviderRegistry::builder(ProviderKey::new("native"))
        .unknown_prefix_policy(UnknownPrefixPolicy::ForwardTo(ProviderKey::new("compat")))
        .register(registration(
            "native",
            ApiProtocol::OpenAiResponses,
            native,
        ))
        .register(registration(
            "compat",
            ApiProtocol::OpenAiChatCompletions,
            Arc::clone(&compat),
        ))
        .register(
            registration(
                "custom",
                ApiProtocol::AnthropicMessages,
                Arc::clone(&explicit),
            )
            .with_alias("foreign"),
        )
        .build()
        .expect("registry should build");

    let explicit_selection = registry
        .resolve_model(Some("foreign/team/model"))
        .expect("explicit alias should win");
    assert_eq!(explicit_selection.selector().provider().as_str(), "custom");
    assert_eq!(explicit_selection.selector().model(), Some("team/model"));
    assert_eq!(
        explicit_selection.selector().protocol(),
        ApiProtocol::AnthropicMessages
    );
    assert_eq!(
        explicit.requested_models(),
        vec![Some("team/model".to_owned())]
    );

    let forwarded = registry
        .resolve_model(Some("unregistered/team/model"))
        .expect("unknown prefix should be forwarded");
    assert_eq!(forwarded.selector().provider().as_str(), "compat");
    assert_eq!(
        forwarded.selector().model(),
        Some("unregistered/team/model")
    );
    assert_eq!(
        compat.requested_models(),
        vec![Some("unregistered/team/model".to_owned())]
    );
}

#[test]
fn 厂商名没有内建分支_是否特殊只由注册项决定() {
    let compat = Arc::new(FakeProvider::new(false));
    let registry = ProviderRegistry::builder(ProviderKey::new("compat"))
        .unknown_prefix_policy(UnknownPrefixPolicy::ForwardTo(ProviderKey::new("compat")))
        .register(registration(
            "compat",
            ApiProtocol::OpenAiChatCompletions,
            Arc::clone(&compat),
        ))
        .build()
        .expect("registry should build");

    for vendor in ["openai/gpt-x", "gemini/pro", "anthropic/sonnet", "grok/beta"] {
        let selected = registry
            .select_model(Some(vendor))
            .expect("unregistered vendor should follow the configured policy");
        assert_eq!(selected.provider().as_str(), "compat");
        assert_eq!(selected.model(), Some(vendor));
    }
}

#[test]
fn 裸模型和_none_都走显式默认_provider() {
    let default = Arc::new(FakeProvider::new(false));
    let registry = ProviderRegistry::builder(ProviderKey::new("default"))
        .register(registration(
            "default",
            ApiProtocol::OpenAiResponses,
            Arc::clone(&default),
        ))
        .build()
        .expect("registry should build");

    let bare = registry
        .resolve_model(Some("model-without-prefix"))
        .expect("bare model should resolve");
    assert_eq!(bare.selector().provider().as_str(), "default");
    assert_eq!(bare.selector().model(), Some("model-without-prefix"));

    let provider_default = registry
        .resolve_model(None)
        .expect("provider default should resolve");
    assert_eq!(provider_default.selector().model(), None);
    assert_eq!(
        default.requested_models(),
        vec![Some("model-without-prefix".to_owned()), None]
    );
}

#[test]
fn 模型别名携带模型层默认_provider_层补出必填_max_tokens() {
    let provider = Arc::new(FakeProvider::new(false));
    let extra_body = Map::from_iter([("routing".to_owned(), json!({"region": "us"}))]);
    let registration = registration(
        "anthropic",
        ApiProtocol::AnthropicMessages,
        Arc::clone(&provider),
    )
    .with_alias("claude")
    .with_defaults(ModelSettings::new().with_max_tokens(4_096))
    .with_static_extra_body(extra_body.into_iter().collect())
    .with_model(
        ModelRegistration::new("claude-sonnet-current")
            .with_alias("sonnet")
            .with_defaults(ModelSettings::new().with_max_tokens(8_192)),
    );
    let registry = ProviderRegistry::builder(ProviderKey::new("anthropic"))
        .register(registration)
        .build()
        .expect("registry should build");

    let resolved = registry
        .resolve_model(Some("claude/sonnet"))
        .expect("model alias should resolve");
    assert_eq!(resolved.selector().provider().as_str(), "anthropic");
    assert_eq!(resolved.selector().model(), Some("claude-sonnet-current"));
    assert_eq!(
        provider.requested_models(),
        vec![Some("claude-sonnet-current".to_owned())]
    );

    // The model registration says this model can emit 8192, so 8192 it is. 4096 is only the
    // provider fallback for "models that did not say", and the coarsest layer must not beat the
    // most specific one.
    let settings = resolved.resolve_settings(&ModelSettings::new(), &ModelSettings::new());
    assert_eq!(settings.max_tokens(), Some(8_192));
    assert_eq!(
        settings.extra_body().get("routing"),
        Some(&json!({"region": "us"}))
    );

    let capped = resolved.resolve_settings(
        &ModelSettings::new(),
        &ModelSettings::new().with_max_tokens(16_384),
    );
    assert_eq!(capped.max_tokens(), Some(8_192));
}

#[test]
fn factory_按注册键懒加载且只创建一次() {
    let provider = Arc::new(FakeProvider::new(false));
    let creates = Arc::new(AtomicUsize::new(0));
    let factory_provider = Arc::clone(&provider);
    let factory_creates = Arc::clone(&creates);
    let registered = ProviderRegistration::new(
        ProviderKey::new("lazy"),
        ApiProtocol::OpenAiResponses,
        move || {
            factory_creates.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::clone(&factory_provider) as Arc<dyn ModelProvider>)
        },
    )
    .with_alias("alias");
    let registry = ProviderRegistry::builder(ProviderKey::new("lazy"))
        .register(registered)
        .build()
        .expect("registry should build without invoking factory");

    assert_eq!(creates.load(Ordering::SeqCst), 0);
    registry
        .resolve_model(Some("one"))
        .expect("first model should resolve");
    registry
        .resolve_model(Some("alias/two"))
        .expect("second model should reuse provider");
    assert_eq!(creates.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.requested_models(),
        vec![Some("one".to_owned()), Some("two".to_owned())]
    );
}

#[tokio::test]
async fn registry_本身满足_model_provider_契约() {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<ProviderRegistry>();

    let child = Arc::new(FakeProvider::new(false));
    let registry = ProviderRegistry::builder(ProviderKey::new("provider"))
        .register(
            registration(
                "provider",
                ApiProtocol::OpenAiResponses,
                Arc::clone(&child),
            )
            .with_alias("p"),
        )
        .build()
        .expect("registry should build");
    let provider: Arc<dyn ModelProvider> = Arc::new(registry);

    provider
        .get_model(Some("p/model"))
        .expect("registry should resolve through the ModelProvider trait");
    provider.close().await.expect("trait close should succeed");

    assert_eq!(child.requested_models(), vec![Some("model".to_owned())]);
    assert_eq!(child.close_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn close_去重并排空所有_provider_关闭后不可重开() {
    let shared = Arc::new(FakeProvider::new(true));
    let healthy = Arc::new(FakeProvider::new(false));
    let registry = ProviderRegistry::builder(ProviderKey::new("a"))
        .register(registration(
            "a",
            ApiProtocol::OpenAiResponses,
            Arc::clone(&shared),
        ))
        .register(registration(
            "b",
            ApiProtocol::OpenAiChatCompletions,
            Arc::clone(&shared),
        ))
        .register(registration(
            "c",
            ApiProtocol::AnthropicMessages,
            Arc::clone(&healthy),
        ))
        .build()
        .expect("registry should build");

    for selector in ["a/one", "b/two", "c/three"] {
        registry
            .resolve_model(Some(selector))
            .expect("provider should resolve before close");
    }

    let close_error = registry.close().await.expect_err("one provider should fail");
    assert_eq!(close_error.code(), "caller");
    assert_eq!(shared.close_count.load(Ordering::SeqCst), 1);
    assert_eq!(healthy.close_count.load(Ordering::SeqCst), 1);

    registry.close().await.expect("repeated close is idempotent");
    assert_eq!(shared.close_count.load(Ordering::SeqCst), 1);
    let after_close = registry
        .resolve_model(Some("a/four"))
        .expect_err("closed registry must not recreate providers");
    assert_eq!(after_close.code(), "caller");
}

#[test]
fn fail_fast_拒绝未知或畸形前缀() {
    let provider = Arc::new(FakeProvider::new(false));
    let registry = ProviderRegistry::builder(ProviderKey::new("known"))
        .register(registration(
            "known",
            ApiProtocol::OpenAiResponses,
            provider,
        ))
        .build()
        .expect("registry should build");

    for selector in ["unknown/model", "/model", "known/", ""] {
        let error = registry
            .select_model(Some(selector))
            .expect_err("selector should fail fast");
        assert_eq!(error.code(), "config");
    }
}

#[test]
fn build_拒绝身份冲突_悬空_fallback_和错误_extra_body_桶() {
    let one = Arc::new(FakeProvider::new(false));
    let two = Arc::new(FakeProvider::new(false));
    let collision = ProviderRegistry::builder(ProviderKey::new("one"))
        .register(
            registration("one", ApiProtocol::OpenAiResponses, one).with_alias("shared"),
        )
        .register(
            registration("two", ApiProtocol::OpenAiResponses, two).with_alias("shared"),
        )
        .build()
        .expect_err("prefix collision should fail");
    assert_eq!(collision.code(), "config");

    let dangling = ProviderRegistry::builder(ProviderKey::new("only"))
        .unknown_prefix_policy(UnknownPrefixPolicy::ForwardTo(ProviderKey::new("missing")))
        .register(registration(
            "only",
            ApiProtocol::OpenAiResponses,
            Arc::new(FakeProvider::new(false)),
        ))
        .build()
        .expect_err("fallback target must be registered");
    assert_eq!(dangling.code(), "config");

    let wrong_bucket = ModelSettings::new().with_extra_body_value(
        ProviderKey::new("somewhere-else"),
        "secret",
        Value::Bool(true),
    );
    let misplaced = ProviderRegistry::builder(ProviderKey::new("actual"))
        .register(
            registration(
                "actual",
                ApiProtocol::OpenAiResponses,
                Arc::new(FakeProvider::new(false)),
            )
            .with_defaults(wrong_bucket),
        )
        .build()
        .expect_err("registration must own only its canonical bucket");
    assert_eq!(misplaced.code(), "config");
}
