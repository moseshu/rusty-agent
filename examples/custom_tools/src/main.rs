//! Function tools through a shipped provider, with a deterministic offline runner check.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelProvider, ModelRequest, ModelResolver, ModelSelector,
        ModelSettings, ProviderKey, ResolvedModel,
    },
    state::RunId,
    tool::{Tool, ToolContext, ToolOutput},
};
use ra_macros::tool;
use ra_model::{
    openai::{auth::OpenAiAuth, responses::OpenAiResponsesProvider},
    provider::{ProviderRegistration, ProviderRegistry, quirks::ProviderQuirks},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunOutcome, RunRequest, Runner},
};
use serde_json::json;

/// Counts the lines in a block of text.
#[tool(name = "line_count")]
fn count_lines(#[tool(description = "Text whose lines should be counted.")] text: String) -> usize {
    text.lines().count()
}

/// Multiplies two numbers and records how often the application used the calculator.
#[tool]
async fn multiply(
    context: &ToolContext<'_>,
    #[tool(description = "The first factor.")] a: i64,
    #[tool(description = "The second factor.", default = 2)] b: i64,
) -> Result<i64> {
    context
        .run()
        .app_context::<AtomicUsize>()
        .ok_or_else(|| Error::caller("the application counter is missing"))?
        .fetch_add(1, Ordering::SeqCst);
    a.checked_mul(b)
        .ok_or_else(|| Error::caller("the product overflowed i64"))
}

struct OfflineModel(AtomicUsize);
#[async_trait::async_trait]
impl Model for OfflineModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let output = if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![RunItem::new(
                ItemId::new("multiply-call"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("multiply-1"),
                    "multiply",
                    json!({"a": 21}),
                )),
            )]
        } else {
            let has_output = request.input().iter().any(|item| {
                matches!(item, ModelInputItem::ToolCallOutput(output)
                    if ToolOutput::from_stored(output.output()).is_ok_and(|output|
                        output.is_some_and(|output| output.as_text() == Some("42"))))
            });
            if !has_output {
                return Err(Error::caller(
                    "the function result was not returned to the model",
                ));
            }
            vec![RunItem::new(
                ItemId::new("answer"),
                RunItemKind::Message(Message::assistant("42", OutputPhase::Final)),
            )]
        };
        Ok(ModelResponse::new(output))
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
        .map_err(|_| Error::config("set OPENAI_API_KEY for a live run, or pass --check"))?;
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

#[tokio::main]
async fn main() -> Result<()> {
    let check = std::env::args().any(|argument| argument == "--check");
    let counter = Arc::new(AtomicUsize::new(0));
    let line_count = count_lines::tool()?;
    if count_lines("alpha\nbeta".to_owned()) != 2 {
        return Err(Error::caller("ordinary function invocation failed"));
    }
    println!(
        "{}: {}",
        line_count.schema().name(),
        line_count.schema().canonical_json()?
    );
    let agent = AgentSpec::builder().id(AgentId::new("calculator")).name("Calculator")
        .instructions("Use multiply for multiplication and line_count to count text lines. Return the result.")
        .tool(Arc::new(line_count)).tools([Arc::new(multiply::tool()?) as Arc<dyn Tool>]).build()?;
    let models: Arc<dyn ModelResolver> = if check {
        Arc::new(OfflineResolver(Arc::new(OfflineModel(AtomicUsize::new(0)))))
    } else {
        Arc::new(live_models()?)
    };
    let input =
        std::env::var("RA_REQUEST").unwrap_or_else(|_| "What is 21 multiplied by 2?".to_owned());
    let result = Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent),
            models,
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(input))],
        )
        .with_app_context(counter.clone()),
    )
    .await?;
    println!("{}", result.final_text());
    if !matches!(result.outcome(), RunOutcome::Completed { .. }) {
        return Err(Error::caller("the function-tool run did not complete"));
    }
    if check && (counter.load(Ordering::SeqCst) != 1 || result.final_text() != "42") {
        return Err(Error::caller("offline function-tool execution failed"));
    }
    Ok(())
}
