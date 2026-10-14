//! A typed final result through a shipped provider, with deterministic offline validation.

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    item::{ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind},
    model::{
        ApiProtocol, Model, ModelProvider, ModelRequest, ModelResolver, ModelSelector,
        ModelSettings, ProviderKey, ResolvedModel,
    },
    state::RunId,
};
use ra_model::{
    openai::{auth::OpenAiAuth, responses::OpenAiResponsesProvider},
    provider::{ProviderRegistration, ProviderRegistry, quirks::ProviderQuirks},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunRequest, RunResult, Runner},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A result the application can consume directly.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct Review {
    summary: String,
    approved: bool,
}

struct OfflineModel(&'static str);
#[async_trait]
impl Model for OfflineModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        if request.output_schema().is_none() {
            return Err(Error::caller("typed schema was not sent"));
        }
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new("answer"),
            RunItemKind::Message(Message::assistant(self.0, OutputPhase::Final)),
        )]))
    }
}
struct OfflineResolver(Arc<dyn Model>);
impl ModelResolver for OfflineResolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("offline"),
                Some("scripted".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            self.0.clone(),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn live_models() -> Result<ProviderRegistry> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| Error::config("set OPENAI_API_KEY or pass --check"))?;
    let model = std::env::var("RA_MODEL")
        .map_err(|_| Error::config("set RA_MODEL to a model available at your endpoint"))?;
    let mut auth = OpenAiAuth::new(api_key);
    if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
        auth = auth.with_base_url(base_url);
    }
    let provider = ProviderKey::new("openai");
    ProviderRegistry::builder(provider.clone())
        .register(ProviderRegistration::new(
            provider,
            ApiProtocol::OpenAiResponses,
            move |quirks: ProviderQuirks| {
                Ok(Arc::new(
                    OpenAiResponsesProvider::new(auth.clone(), model.clone())?.with_quirks(quirks),
                ) as Arc<dyn ModelProvider>)
            },
        ))
        .build()
}

async fn run(models: Arc<dyn ModelResolver>, streamed: bool) -> Result<RunResult> {
    let agent = AgentSpec::builder()
        .id(AgentId::new("reviewer"))
        .name("Reviewer")
        .instructions("Summarize the request and decide whether to approve it.")
        .output_type::<Review>()?
        .build()?;
    let request = RunRequest::new(
        AgentBinding::direct(agent),
        models,
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user(
            "Review adding a deterministic offline check to the example.",
        ))],
    );
    if streamed {
        let mut stream = Runner::run_streamed(request);
        while stream.next_event().await.is_some() {}
        stream.finish().await
    } else {
        Runner::run(request).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let check = std::env::args().any(|argument| argument == "--check");
    let streamed = std::env::args().any(|argument| argument == "--stream");
    let models: Arc<dyn ModelResolver> = if check {
        Arc::new(OfflineResolver(Arc::new(OfflineModel(
            r#"{"summary":"The offline check is deterministic.","approved":true}"#,
        ))))
    } else {
        Arc::new(live_models()?)
    };
    let result = run(models, streamed).await?;
    let review = result.final_output_as::<Review>()?;
    println!("Approved: {}\nSummary: {}", review.approved, review.summary);
    if check {
        for invalid in [
            "not valid JSON",
            r#"{"summary":"wrong type","approved":"yes"}"#,
        ] {
            let error = run(
                Arc::new(OfflineResolver(Arc::new(OfflineModel(invalid)))),
                streamed,
            )
            .await
            .err()
            .ok_or_else(|| Error::caller("invalid output unexpectedly succeeded"))?;
            if error.code() != "output.invalid_final_output" {
                return Err(error);
            }
            println!("Expected validation failure: {}", error.code());
        }
    }
    Ok(())
}
