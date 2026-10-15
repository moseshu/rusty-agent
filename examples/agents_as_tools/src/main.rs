//! A specialist returns a tool result; the manager retains the conversation.

#[path = "../../support.rs"]
mod support;

use std::sync::Arc;

use rusty_agent::prelude::*;
use serde_json::json;
use support::{answer, call, has_tool_output, messages, require};

/// The manager's instructions identify which agent sends the final request.
const MANAGER_INSTRUCTIONS: &str =
    "Use translate_to_french to translate. Then present the translation to the user.";

#[tokio::main]
async fn main() -> Result<()> {
    let example = support::setup(vec![
        Box::new(|_| Ok(call("translate_to_french", &json!({"input":"Hello"})))),
        Box::new(|request| {
            require(
                messages(request) == ["Hello"],
                "specialist must receive generated input, not parent history",
            )?;
            Ok(answer("Bonjour"))
        }),
        Box::new(|request| {
            require(
                has_tool_output(request, "Bonjour"),
                "specialist result did not return to the manager",
            )?;
            require(
                request["instructions"] == MANAGER_INSTRUCTIONS,
                "manager did not regain control",
            )?;
            Ok(answer("French: Bonjour"))
        }),
    ])
    .await?;
    // tutorial:agent_tool:start
    let specialist = AgentSpec::builder()
        .id(AgentId::new("french"))
        .name("French translator")
        .instructions("Translate the supplied text into French. Return only the translation.")
        .build()?;
    let manager = AgentSpec::builder()
        .id(AgentId::new("manager"))
        .name("Manager")
        .instructions(MANAGER_INSTRUCTIONS)
        .tool(Arc::new(
            specialist
                .as_tool()
                .tool_name("translate_to_french")
                .tool_description("Translate the supplied text into French.")
                .build()?,
        ))
        .build()?;
    let result = example
        .runner
        .run(manager.clone(), "Translate Hello to French.")
        .await?;
    // tutorial:agent_tool:end
    println!("{}", result.final_text());
    if example.check {
        require(
            result.last_agent().id() == manager.id(),
            "specialist took over the conversation",
        )?;
        require(
            result.final_text() == "French: Bonjour",
            "missing manager synthesis",
        )?;
    }
    example.finish().await
}
