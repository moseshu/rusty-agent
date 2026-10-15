//! A triage agent transferring a conversation to a refund specialist through a shipped provider.
//!
//! `--check` runs a local HTTP handoff round trip. A live run requires `OPENAI_API_KEY` and
//! `RA_MODEL`.
//! `--filter` forwards only the opening input through `input_items`, so the Session still stores
//! the whole turn; a filter that rewrites `new_items` would change the stored turn too.

use std::sync::{Arc, Mutex};

#[path = "../../support.rs"]
mod support;

use rusty_agent::{agent::HandoffInputData, prelude::*};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use support::{answer, call, messages, require};

/// Bookkeeping payload for the transfer; the specialist's main input remains the conversation.
#[derive(Deserialize, JsonSchema)]
struct RefundTransfer {
    /// Why the request should be handled by the refund specialist.
    reason: String,
}

fn offline_steps(filtered: bool) -> Vec<support::Step> {
    vec![
        Box::new(|request| {
            require(
                request["tools"].as_array().is_some_and(|tools| {
                    tools
                        .iter()
                        .any(|tool| tool["name"] == "transfer_to_refund_specialist")
                }),
                "handoff schema did not reach the provider",
            )?;
            Ok(call(
                "transfer_to_refund_specialist",
                &json!({"reason":"refund request"}),
            ))
        }),
        Box::new(move |request| {
            require(
                messages(request) == ["I need help requesting a refund for invoice 123."],
                "handoff lost opening input",
            )?;
            require(
                request["input"]
                    .as_array()
                    .is_some_and(|items| items.len() == if filtered { 1 } else { 3 }),
                "handoff history or explicit input filtering differs from the contract",
            )?;
            Ok(answer("I can help you request a refund for invoice 123."))
        }),
    ]
}

#[tokio::main]
async fn main() -> Result<()> {
    let refunds = AgentSpec::builder()
        .id(AgentId::new("refunds"))
        .name("Refund Specialist")
        .handoff_description("Helps the user understand refund requests.")
        .instructions(
            "Answer the user's refund question using the conversation history. \
             Do not claim to have issued a refund.",
        )
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
    let filtered = std::env::args().any(|arg| arg == "--filter");
    let handoff = if filtered {
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
        .instructions(
            "For refund questions, transfer the conversation to the refund specialist \
             with a reason.",
        )
        .handoff(handoff)
        .build()?;
    let agents = AgentRegistry::builder()
        .register(Arc::clone(&triage))
        .register(refunds)
        .build()?;
    let example = support::setup(offline_steps(filtered)).await?;
    let input = if example.check {
        "I need help requesting a refund for invoice 123.".to_owned()
    } else {
        std::env::var("RA_REQUEST")
            .unwrap_or_else(|_| "I need help requesting a refund for invoice 123.".to_owned())
    };
    let session = Arc::new(InMemorySession::new("refund-conversation"));
    let result = example
        .runner
        .request(triage, input)
        .with_app_context(transfer_log.clone())
        .with_session(session.clone())
        .with_config(RunConfig::new().with_agent_registry(agents))
        .run()
        .await?;
    println!("{}", result.final_text());
    if example.check {
        require(
            result.last_agent().id().as_str() == "refunds",
            "specialist did not take over",
        )?;
        require(
            transfer_log
                .lock()
                .map_err(|_| Error::caller("transfer log is poisoned"))?
                .as_slice()
                == ["refund request"],
            "typed callback did not run exactly once",
        )?;
        let items = session.get_items(None).await?;
        require(items.len() == 4, "filter changed authoritative history")?;
        require(
            items
                .iter()
                .any(|item| matches!(item.kind(), RunItemKind::HandoffCall(_))),
            "session lost handoff call",
        )?;
        require(
            items
                .iter()
                .any(|item| matches!(item.kind(), RunItemKind::HandoffOutput(_))),
            "session lost handoff result",
        )?;
    }
    example.finish().await
}
