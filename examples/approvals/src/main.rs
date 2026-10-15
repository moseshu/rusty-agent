//! Serialize an interrupted run, decide once, and resume with its session and identity.

#[path = "../../support.rs"]
mod support;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rusty_agent::{item::MessageRole, prelude::*};
use serde_json::json;
use support::{answer, call, has_tool_output, require};

/// Simulate a local action that requires a host decision before invocation.
#[tool(needs_approval = true)]
fn record_note(context: &ToolContext<'_>, text: String) -> Result<String> {
    context
        .run()
        .app_context::<AtomicUsize>()
        .ok_or_else(|| Error::caller("application counter is missing"))?
        .fetch_add(1, Ordering::SeqCst);
    Ok(format!("Recorded: {text}"))
}

const INSTRUCTIONS: &str =
    "Call record_note once with Atlas, then report its result. Do not retry a rejected call.";

/// Checks that recovery neither replayed the model's tool call nor resubmitted the user input.
fn verify_history(items: &[RunItem]) -> Result<()> {
    let tool_calls = items
        .iter()
        .filter(|item| matches!(item.kind(), RunItemKind::ToolCall(_)))
        .count();
    require(tool_calls == 1, "model call was replayed on recovery")?;
    let user_inputs = items
        .iter()
        .filter(|item| match item.kind() {
            RunItemKind::Message(message) => message.role() == MessageRole::User,
            _ => false,
        })
        .count();
    require(user_inputs == 1, "user input duplicated on recovery")
}

#[tokio::main]
async fn main() -> Result<()> {
    let reject = std::env::args().any(|argument| argument == "--reject");
    let streamed = std::env::args().any(|argument| argument == "--stream");
    let example = support::setup(vec![
        Box::new(|_| Ok(call("record_note", &json!({"text":"Atlas"})))),
        Box::new(move |request| {
            if reject {
                require(
                    has_tool_output(request, "Declined by the tutorial host"),
                    "rejection did not reach the provider",
                )?;
                Ok(answer("The note was declined."))
            } else {
                require(
                    has_tool_output(request, "Recorded: Atlas"),
                    "approved output did not reach the provider",
                )?;
                Ok(answer("The note was recorded."))
            }
        }),
    ])
    .await?;
    let agent = AgentSpec::builder()
        .id(AgentId::new("notes"))
        .name("Notes")
        .instructions(INSTRUCTIONS)
        .tool(Arc::new(record_note::tool()?))
        .build()?;
    let session = Arc::new(InMemorySession::new(SessionId::generate()));
    let counter = Arc::new(AtomicUsize::new(0));
    let request = example
        .runner
        .request(agent.clone(), "Record a note: Atlas.")
        .with_session(session.clone())
        .with_app_context(counter.clone());
    let interrupted = if streamed {
        request.run_streamed().finish().await?
    } else {
        request.run().await?
    };
    require(
        matches!(interrupted.outcome(), RunOutcome::Interrupted { .. }),
        "expected an approval interruption",
    )?;
    require(
        counter.load(Ordering::SeqCst) == 0,
        "tool ran before approval",
    )?;
    // tutorial:approval:start
    let snapshot = serde_json::to_vec(interrupted.state())
        .map_err(|error| Error::caller("cannot serialize checkpoint").with_source(error))?;
    let mut state: RunState = serde_json::from_slice(&snapshot)
        .map_err(|error| Error::caller("cannot restore checkpoint").with_source(error))?;
    for item in interrupted.outcome().interruptions() {
        if let RunItemKind::ToolApproval(approval) = item.kind() {
            println!(
                "Pending approval: {}({})",
                approval.tool_name(),
                approval.arguments()
            );
        }
        // This tutorial's host policy approves the simulated action unless --reject is selected.
        if reject {
            state.reject_with_message(item, false, "Declined by the tutorial host")?;
        } else {
            state.approve(item, false)?;
        }
    }
    // Application context is a live object and is explicitly reattached after deserialization.
    let request = example
        .runner
        .request(agent, state)
        .with_session(session.clone())
        .with_app_context(counter.clone());
    let resumed = if streamed {
        request.run_streamed().finish().await?
    } else {
        request.run().await?
    };
    // tutorial:approval:end
    require(
        resumed.state().run_id() == interrupted.state().run_id(),
        "recovery changed run identity",
    )?;
    if example.check {
        require(
            matches!(resumed.outcome(), RunOutcome::Completed { .. }),
            "recovery did not complete",
        )?;
        require(
            counter.load(Ordering::SeqCst) == usize::from(!reject),
            "action was lost or replayed",
        )?;
        verify_history(&session.get_items(None).await?)?;
    }
    println!("{}", resumed.final_text());
    example.finish().await
}
