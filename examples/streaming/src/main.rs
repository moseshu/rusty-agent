//! Provider text deltas and settled items are separate views of the same run.

#[path = "../../support.rs"]
mod support;

use std::{
    io::{self, Write},
    sync::Arc,
};

use rusty_agent::model::ModelStreamDeltaKind;
use rusty_agent::prelude::*;
use serde_json::json;
use support::{answer, call, has_tool_output, require};

/// Return the topic that the assistant should explain.
#[tool]
fn topic() -> String {
    "Rust ownership".to_owned()
}

#[tokio::main]
async fn main() -> Result<()> {
    let example = support::setup(vec![
        Box::new(|_| Ok(call("topic", &json!({})))),
        Box::new(|request| {
            require(
                has_tool_output(request, "Rust ownership"),
                "tool output did not reach the provider",
            )?;
            Ok(answer("Ownership makes resource lifetimes explicit."))
        }),
    ])
    .await?;
    let agent = AgentSpec::builder()
        .id(AgentId::new("teacher"))
        .name("Teacher")
        .instructions("Call topic first, then explain that topic in one sentence.")
        .tool(Arc::new(topic::tool()?))
        .build()?;
    let items = std::env::args().any(|argument| argument == "--items");
    let mut rendered = String::new();
    let mut settled = Vec::new();
    // tutorial:stream:start
    let mut stream = example
        .runner
        .request(agent, "Explain today's topic.")
        .with_config(RunConfig::new().with_partial_messages(true))
        .run_streamed();
    while let Some(event) = stream.next_event().await {
        match event {
            RunStreamEvent::Delta(narration) if !items => {
                if let ModelStreamDeltaKind::OutputText { delta } = narration.kind() {
                    print!("{delta}");
                    io::stdout()
                        .flush()
                        .map_err(|error| Error::caller("cannot flush output").with_source(error))?;
                    rendered.push_str(delta);
                }
            }
            RunStreamEvent::Item(item) => {
                if items {
                    println!("{item:?}");
                }
                settled.push(item);
            }
            RunStreamEvent::FinalMessage(message) if items => {
                println!("{}", message.text_content());
            }
            _ => {}
        }
    }
    let result = stream.finish().await?;
    // tutorial:stream:end
    println!();
    if example.check {
        require(
            settled
                .iter()
                .any(|item| matches!(item.kind(), RunItemKind::ToolCall(_))),
            "missing settled tool call",
        )?;
        require(
            settled
                .iter()
                .any(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(_))),
            "missing settled tool output",
        )?;
        require(
            settled
                .iter()
                .filter(|item| matches!(item.kind(), RunItemKind::Message(_)))
                .count()
                == 1,
            "message was duplicated",
        )?;
        if !items {
            require(
                rendered == result.final_text(),
                "delta text differs from settled answer",
            )?;
        }
    }
    example.finish().await
}
