//! A triage agent transferring a conversation to a refund specialist through a shipped provider.
//!
//! `--check` validates declarations offline. A live run requires `OPENAI_API_KEY` and `RA_MODEL`.
//! `--filter` forwards only the opening input through `input_items`, so the Session still stores
//! the whole turn; a filter that rewrites `new_items` would change the stored turn too.

use std::sync::{Arc, Mutex};

use ra_core::{
    agent::{AgentId, AgentSpec, HandoffInputData},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    item::{Message, ModelInputItem},
    model::{ApiProtocol, ModelProvider, ProviderKey},
    state::RunId,
};
use ra_model::{
    openai::{auth::OpenAiAuth, responses::OpenAiResponsesProvider},
    provider::{ProviderRegistration, ProviderRegistry, quirks::ProviderQuirks},
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    runner::{RunConfig, RunRequest, Runner},
};
use schemars::JsonSchema;
use serde::Deserialize;

/// Bookkeeping payload for the transfer; the specialist's main input remains the conversation.
#[derive(Deserialize, JsonSchema)]
struct RefundTransfer {
    /// Why the request should be handled by the refund specialist.
    reason: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let refunds = AgentSpec::builder()
        .id(AgentId::new("refunds"))
        .name("Refund Specialist")
        .handoff_description("Helps the user understand refund requests.")
        .instructions("Answer the user's refund question using the conversation history. Do not claim to have issued a refund.")
        .build()?;
    let transfer_log = Arc::new(Mutex::new(Vec::<String>::new()));
    let handoff =
        refunds
            .as_handoff()?
            .with_input_type(|context: &RunContext, input: RefundTransfer| {
                let result = context
                    .app_context::<Mutex<Vec<String>>>()
                    .ok_or_else(|| Error::caller("the transfer log is missing"))
                    .and_then(|log| {
                        log.lock()
                            .map_err(|_| Error::caller("the transfer log is poisoned"))?
                            .push(input.reason);
                        Ok(())
                    });
                std::future::ready(result)
            })?;
    let handoff = if std::env::args().any(|arg| arg == "--filter") {
        handoff.with_input_filter_fn(|_: &RunContext, input: HandoffInputData| {
            std::future::ready(Ok(input
                .with_pre_handoff_items(Vec::new())
                .with_input_items(Vec::new())))
        })
    } else {
        handoff
    };
    let triage = AgentSpec::builder()
        .id(AgentId::new("triage"))
        .name("Triage")
        .instructions("For refund questions, transfer the conversation to the refund specialist with a reason.")
        .handoff(handoff)
        .build()?;
    let agents = AgentRegistry::builder()
        .register(Arc::clone(&triage))
        .register(refunds)
        .build()?;
    if std::env::args().any(|arg| arg == "--check") {
        println!(
            "{}",
            serde_json::to_string_pretty(triage.handoffs()[0].schema()).map_err(|error| {
                Error::caller("the handoff declaration could not be rendered").with_source(error)
            })?
        );
        return Ok(());
    }
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| Error::config("set OPENAI_API_KEY for a live run"))?;
    let model = std::env::var("RA_MODEL")
        .map_err(|_| Error::config("set RA_MODEL to a model available at your endpoint"))?;
    let mut auth = OpenAiAuth::new(api_key);
    if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
        auth = auth.with_base_url(base_url);
    }
    let provider = ProviderKey::new("openai");
    let models = ProviderRegistry::builder(provider.clone())
        .register(ProviderRegistration::new(
            provider,
            ApiProtocol::OpenAiResponses,
            move |quirks: ProviderQuirks| {
                Ok(Arc::new(
                    OpenAiResponsesProvider::new(auth.clone(), model.clone())?.with_quirks(quirks),
                ) as Arc<dyn ModelProvider>)
            },
        ))
        .build()?;
    let input = std::env::var("RA_REQUEST")
        .unwrap_or_else(|_| "I need help requesting a refund for invoice 123.".to_owned());
    let result = Runner::run(
        RunRequest::new(
            AgentBinding::direct(triage),
            Arc::new(models),
            RunId::generate(),
            CancelScope::root(),
            vec![ModelInputItem::Message(Message::user(input))],
        )
        .with_app_context(transfer_log)
        .with_session(Arc::new(ra_session::InMemorySession::new(
            "refund-conversation",
        )))
        .with_config(RunConfig::new().with_agent_registry(agents)),
    )
    .await?;
    println!("{}", result.final_text());
    Ok(())
}
