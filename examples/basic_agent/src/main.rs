//! One configured provider, direct text input, sessions, context, and settled streaming text.

use std::sync::Arc;

use async_trait::async_trait;
use rusty_agent::{
    model::{Model, ModelProvider, ModelRequest},
    openai::{auth::OpenAiAuth, responses::OpenAiResponsesProvider},
    prelude::*,
    provider::quirks::ProviderQuirks,
};

/// Return a greeting for a named person.
#[tool]
fn greet(name: String) -> String {
    format!("Hello, {name}!")
}

struct AppContext {
    language: String,
}

// Offline validation still uses ProviderRegistry; callers never implement a resolver.
struct OfflineProvider;

#[async_trait]
impl ModelProvider for OfflineProvider {
    fn get_model(&self, _: Option<&str>) -> Result<Arc<dyn Model>> {
        Ok(Arc::new(OfflineModel))
    }
}

struct OfflineModel;

#[async_trait]
impl Model for OfflineModel {
    async fn get_response(&self, _: ModelRequest) -> Result<rusty_agent::item::ModelResponse> {
        use rusty_agent::item::{ItemId, ModelResponse, OutputPhase};
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new("offline-answer"),
            RunItemKind::Message(Message::assistant("Hello, Rust!", OutputPhase::Final)),
        )]))
    }
}

fn models(check: bool) -> Result<ProviderRegistry> {
    let provider = ProviderKey::new("openai");
    let registration = if check {
        ProviderRegistration::new(
            provider.clone(),
            ApiProtocol::OpenAiResponses,
            |_: ProviderQuirks| Ok(Arc::new(OfflineProvider) as Arc<dyn ModelProvider>),
        )
    } else {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| Error::config("set OPENAI_API_KEY or pass --check"))?;
        let model = std::env::var("RA_MODEL")
            .map_err(|_| Error::config("set RA_MODEL to a model available at your endpoint"))?;
        let mut auth = OpenAiAuth::new(api_key);
        if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
            auth = auth.with_base_url(base_url);
        }
        ProviderRegistration::new(
            provider.clone(),
            ApiProtocol::OpenAiResponses,
            move |quirks: ProviderQuirks| {
                Ok(Arc::new(
                    OpenAiResponsesProvider::new(auth.clone(), model.clone())?.with_quirks(quirks),
                ) as Arc<dyn ModelProvider>)
            },
        )
    };
    ProviderRegistry::builder(provider)
        .register(registration)
        .build()
}

fn main() -> Result<()> {
    let check = std::env::args().any(|argument| argument == "--check");
    let sync = std::env::args().any(|argument| argument == "--sync");
    let streamed = std::env::args().any(|argument| argument == "--stream");
    let items = std::env::args().any(|argument| argument == "--items");
    let runner = AgentRunner::new(Arc::new(models(check)?));
    let agent = AgentSpec::builder()
        .id(AgentId::new("greeter"))
        .name("Greeter")
        .instructions("Greet Rust briefly. Use the greeting tool if helpful.")
        .tool(Arc::new(greet::tool()?))
        .build()?;

    if sync {
        let first = runner.run_sync(agent.clone(), "Greet Rust.")?;
        println!("{}", first.final_text());
        // Repeated calls reuse the provider and the thread's sync executor.
        println!(
            "{}",
            runner.run_sync(agent, "Greet Rust again.")?.final_text()
        );
        return Ok(());
    }

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::caller("cannot start the example runtime").with_source(error))?
        .block_on(async move {
            // The minimal ordinary call needs only the configured runner, agent, and text.
            let result = runner.run(agent.clone(), "Greet Rust.").await?;
            println!("{}", result.final_text());
            let session = Arc::new(InMemorySession::new(SessionId::generate()));
            let contextual_agent = agent
                .to_builder()
                .dynamic_instructions_async_fn(|context| {
                    Box::pin(async move {
                        tokio::task::yield_now().await;
                        let app = context
                            .app_context::<AppContext>()
                            .ok_or_else(|| Error::caller("AppContext is required"))?;
                        Ok(ResolvedPrompt::new(
                            format!("Greet Rust briefly in {}.", app.language),
                            PromptSource::Dynamic("app_context".to_owned()),
                        ))
                    })
                })
                .build()?;
            let request = runner
                .request(contextual_agent, "Greet Rust again.")
                .with_context(AppContext {
                    language: "English".to_owned(),
                })
                .with_session(session.clone());
            let result = if items {
                let mut stream = request.run_streamed();
                while let Some(item) = stream.next_item().await {
                    println!("{item:?}");
                }
                let result = stream.finish().await?;
                println!("{}", result.final_text());
                result
            } else if streamed {
                let mut stream = request.run_streamed();
                while let Some(text) = stream.next_text().await {
                    println!("{text}");
                }
                stream.finish().await?
            } else {
                let result = request.run().await?;
                println!("{}", result.final_text());
                result
            };
            if check
                && (result.final_text() != "Hello, Rust!"
                    || session.get_items(None).await?.len() != 2)
            {
                return Err(Error::caller("offline run or session validation failed"));
            }
            Ok(())
        })
}
