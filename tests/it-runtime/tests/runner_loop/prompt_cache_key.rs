//! Runner cache-key parity, including streaming and serialized approval continuations.
use super::*;
use ra_core::model::ResolvedModelSettings;
use ra_session::memory::InMemorySession;

struct CacheModel {
    responses: Mutex<Vec<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    supported: bool,
}

impl CacheModel {
    fn new(responses: Vec<ModelResponse>, supported: bool) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(vec![]),
            supported,
        })
    }
    fn keys(&self) -> Vec<Option<String>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.prompt_cache_key().map(str::to_owned))
            .collect()
    }
}

#[async_trait]
impl Model for CacheModel {
    fn supports_default_prompt_cache_key(&self, settings: &ResolvedModelSettings) -> bool {
        self.supported && !settings.extra_body().contains_key("prompt_cache_key")
    }
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Ok(self.responses.lock().unwrap().remove(0))
    }
}

fn cache_request(model: &Arc<CacheModel>, tools: Vec<Arc<dyn Tool>>, run: &str) -> RunRequest {
    RunRequest::new(
        agent(tools),
        Arc::new(SingleModelResolver {
            model: model.clone(),
        }),
        RunId::new(run),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user("hello"))],
    )
}
fn answer() -> ModelResponse {
    ModelResponse::new(vec![message("done", "done")])
}

#[tokio::test]
async fn generated_keys_are_per_run_and_reused_across_turns() {
    let model = CacheModel::new(
        vec![
            ModelResponse::new(vec![tool_call("tool", "c1", "lookup")]),
            answer(),
            answer(),
        ],
        true,
    );
    let first = Runner::run(cache_request(
        &model,
        vec![Arc::new(ScriptedTool::new("lookup"))],
        "first",
    ))
    .await
    .unwrap();
    let key = first.state().generated_prompt_cache_key().unwrap();
    let suffix = key.strip_prefix("agents-sdk:run:").unwrap();
    assert_eq!(suffix.len(), 32);
    assert_eq!(uuid::Uuid::parse_str(suffix).unwrap().get_version_num(), 4);
    assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(model.keys(), vec![Some(key.to_owned()); 2]);
    let second = Runner::run(cache_request(&model, vec![], "second"))
        .await
        .unwrap();
    assert_ne!(second.state().generated_prompt_cache_key(), Some(key));
}

#[tokio::test]
async fn session_and_group_boundaries_trim_and_hash_exact_bytes() {
    for (session, group, expected) in [
        (
            Some(" session-123 "),
            Some("ignored"),
            "agents-sdk:session:b9c84322f82434cb46e239d20daf1f37",
        ),
        (
            None,
            Some(" group-123 "),
            "agents-sdk:group:617fa97b8a99e95ea35f0f461c8f7fb4",
        ),
    ] {
        let model = CacheModel::new(vec![answer(), answer()], true);
        for run in ["first", "second"] {
            let mut request = cache_request(&model, vec![], run);
            if let Some(session) = session {
                request = request.with_session(Arc::new(InMemorySession::new(session)));
            }
            if let Some(group) = group {
                request = request.with_config(RunConfig::new().with_group_id(group));
            }
            let result = Runner::run(request).await.unwrap();
            assert_eq!(result.state().generated_prompt_cache_key(), Some(expected));
        }
        assert_eq!(model.keys(), vec![Some(expected.to_owned()); 2]);
    }
}

#[tokio::test]
async fn unsupported_models_and_explicit_extras_do_not_generate_or_persist_keys() {
    for (supported, extra) in [
        (false, None),
        (true, Some(json!("explicit"))),
        (true, Some(Value::Null)),
    ] {
        let model = CacheModel::new(vec![answer()], supported);
        let mut settings = ModelSettings::new();
        if let Some(extra) = &extra {
            settings = settings.with_extra_body_value(
                ProviderKey::new("test-provider"),
                "prompt_cache_key",
                extra.clone(),
            );
        }
        let result = Runner::run(
            cache_request(&model, vec![], "run")
                .with_config(RunConfig::new().with_model_settings(settings)),
        )
        .await
        .unwrap();
        assert_eq!(model.keys(), vec![None]);
        assert_eq!(result.state().generated_prompt_cache_key(), None);
        if let Some(extra) = extra {
            assert_eq!(
                model.requests.lock().unwrap()[0]
                    .model_settings()
                    .extra_body()["prompt_cache_key"],
                extra
            );
        }
    }
}

#[tokio::test]
async fn unrelated_extras_and_streaming_preserve_options_and_generate_a_key() {
    let model = CacheModel::new(vec![answer()], true);
    let settings = ModelSettings::new().with_extra_body(
        ProviderKey::new("test-provider"),
        [
            ("service_tier".to_owned(), json!("flex")),
            (
                "prompt_cache_options".to_owned(),
                json!({"mode":"explicit", "ttl":"30m"}),
            ),
            (
                "context_management".to_owned(),
                json!([{"type":"compaction","compact_threshold":200000}]),
            ),
        ]
        .into(),
    );
    let mut stream = Runner::run_streamed(
        cache_request(&model, vec![], "stream")
            .with_config(RunConfig::new().with_model_settings(settings.clone())),
    );
    while stream.next_event().await.is_some() {}
    let result = stream.finish().await.unwrap();
    assert!(result.state().generated_prompt_cache_key().is_some());
    let requests = model.requests.lock().unwrap();
    assert_eq!(
        requests[0].model_settings().extra_body(),
        &settings.extra_body()[&ProviderKey::new("test-provider")]
    );
}

struct RemoveInstructions;
impl ContextFilter for RemoveInstructions {
    fn name(&self) -> &str {
        "remove_instructions"
    }
    fn filter_model_input(
        &self,
        _: &ContextFilterRequest<'_>,
        input: ModelInputData,
    ) -> Result<ModelInputData> {
        Ok(input.with_instructions(None))
    }
}

#[tokio::test]
async fn filters_cannot_remove_the_routing_key_when_rewriting_or_removing_instructions() {
    for filter in [
        Arc::new(PrefixRewritingFilter) as Arc<dyn ContextFilter>,
        Arc::new(RemoveInstructions),
    ] {
        let model = CacheModel::new(vec![answer()], true);
        let result = Runner::run(
            cache_request(&model, vec![], "filter")
                .with_config(RunConfig::new().with_context_filter(filter)),
        )
        .await
        .unwrap();
        assert_eq!(
            model.keys()[0].as_deref(),
            result.state().generated_prompt_cache_key()
        );
        let requests = model.requests.lock().unwrap();
        assert!(requests[0].validate_cache_plan().is_ok());
        if requests[0].system_instructions().is_none() {
            assert!(requests[0].cache_plan().is_none());
        }
    }
}

#[tokio::test]
async fn product_override_precedes_saved_generation_without_overwriting_it() {
    let model = CacheModel::new(vec![answer()], false);
    let mut state = RunState::start(RunId::new("override"));
    state.set_generated_prompt_cache_key("previous");
    let result = Runner::run(
        cache_request(&model, vec![], "override")
            .with_state(state)
            .with_config(RunConfig::new().with_prompt_cache_key_override("root-thread")),
    )
    .await
    .unwrap();
    assert_eq!(model.keys(), vec![Some("root-thread".to_owned())]);
    assert_eq!(
        result.state().generated_prompt_cache_key(),
        Some("previous")
    );
}

#[tokio::test]
async fn serialized_approval_continuations_reuse_generated_keys() {
    let tool = Arc::new(
        ScriptedTool::new("write_file")
            .with_options(ToolOptions::new().with_approval(ToolApprovalPolicy::Always)),
    );
    let model = CacheModel::new(
        vec![
            ModelResponse::new(vec![tool_call("call", "c1", "write_file")]),
            answer(),
        ],
        true,
    );
    let first = Runner::run(cache_request(&model, vec![tool.clone()], "approval"))
        .await
        .unwrap();
    let mut state: RunState =
        serde_json::from_str(&serde_json::to_string(first.state()).unwrap()).unwrap();
    state
        .approve(&first.outcome().interruptions()[0], true)
        .unwrap();
    let second = Runner::run(cache_request(&model, vec![tool], "approval").with_state(state))
        .await
        .unwrap();
    assert_eq!(
        second.state().generated_prompt_cache_key(),
        first.state().generated_prompt_cache_key()
    );
    assert_eq!(
        model.keys(),
        vec![
            first
                .state()
                .generated_prompt_cache_key()
                .map(str::to_owned);
            2
        ]
    );
}

#[test]
fn conversation_grouping_has_exact_upstream_bytes_and_precedes_session_and_group() {
    use ra_runtime::runner::prompt_cache_key_test_api::resolve;
    let session = InMemorySession::new("session-123");
    assert_eq!(
        resolve(Some(" conv-123 "), Some(&session), Some("group-123")),
        "agents-sdk:conversation:1723376043e8a9d0090025f147440b99"
    );
    assert_eq!(
        resolve(Some(" "), Some(&session), Some("group-123")),
        "agents-sdk:session:b9c84322f82434cb46e239d20daf1f37"
    );
}
