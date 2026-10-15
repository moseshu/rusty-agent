//! Application dependencies stay local; callbacks explicitly select model-visible content.

#[path = "../../support.rs"]
mod support;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use rusty_agent::{
    lifecycle::{AgentEndInput, AgentStartInput, ToolEndInput, ToolStartInput},
    prelude::*,
};
use serde_json::json;
use support::{answer, call, has_tool_output, messages, require};

struct AppContext {
    language: String,
    local_account: String,
    lookups: AtomicUsize,
}

/// Read an application-owned value using the local context.
#[tool]
fn lookup_project(context: &ToolContext<'_>) -> Result<String> {
    let app = context
        .run()
        .app_context::<AppContext>()
        .ok_or_else(|| Error::caller("AppContext is required"))?;
    app.lookups.fetch_add(1, Ordering::SeqCst);
    require(
        app.local_account == "local-account-42",
        "wrong application context",
    )?;
    Ok("Atlas".to_owned())
}

#[derive(Default)]
struct Events {
    starts: AtomicUsize,
    ends: AtomicUsize,
    tool_starts: AtomicUsize,
    tool_ends: AtomicUsize,
}

#[async_trait]
impl LifecycleHook for Events {
    fn name(&self) -> &'static str {
        "tutorial_events"
    }
    async fn on_agent_start(&self, _: LifecycleScope, _: &AgentStartInput<'_>) -> Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        println!("Agent started");
        Ok(())
    }
    async fn on_agent_end(&self, _: LifecycleScope, _: &AgentEndInput<'_>) -> Result<()> {
        self.ends.fetch_add(1, Ordering::SeqCst);
        println!("Agent ended");
        Ok(())
    }
    async fn on_tool_start(&self, _: LifecycleScope, _: &ToolStartInput<'_>) -> Result<()> {
        self.tool_starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn on_tool_end(&self, _: LifecycleScope, _: &ToolEndInput<'_>) -> Result<()> {
        self.tool_ends.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let check_request = |request: &serde_json::Value| {
        let instructions = "Answer briefly in English. Use lookup_project for the project name.";
        require(
            request["instructions"] == instructions,
            "dynamic instructions did not reach the system-instruction field",
        )?;
        require(
            !messages(request).iter().any(|text| text == instructions),
            "dynamic instructions were duplicated into conversation input",
        )?;
        require(
            !request.to_string().contains("local-account-42"),
            "local application state leaked into model input",
        )
    };
    let example = support::setup(vec![
        Box::new(move |request| {
            check_request(request)?;
            Ok(call("lookup_project", &json!({})))
        }),
        Box::new(move |request| {
            check_request(request)?;
            require(
                has_tool_output(request, "Atlas"),
                "selected tool value did not reach the model",
            )?;
            Ok(answer("Your project is Atlas."))
        }),
    ])
    .await?;
    let app = Arc::new(AppContext {
        language: "English".to_owned(),
        local_account: "local-account-42".to_owned(),
        lookups: AtomicUsize::new(0),
    });
    let events = Arc::new(Events::default());
    // tutorial:context:start
    let agent = AgentSpec::builder()
        .id(AgentId::new("assistant"))
        .name("Assistant")
        .dynamic_instructions_async_fn(|context| {
            Box::pin(async move {
                tokio::task::yield_now().await;
                let app = context
                    .app_context::<AppContext>()
                    .ok_or_else(|| Error::caller("AppContext is required"))?;
                Ok(ResolvedPrompt::new(
                    format!(
                        "Answer briefly in {}. Use lookup_project for the project name.",
                        app.language
                    ),
                    PromptSource::Dynamic("app_context".to_owned()),
                ))
            })
        })
        .tool(Arc::new(lookup_project::tool()?))
        .lifecycle_hook(events.clone())
        .build()?;
    let result = example
        .runner
        .request(agent, "What is my project?")
        .with_app_context(app.clone())
        .run()
        .await?;
    // tutorial:context:end
    println!("{}", result.final_text());
    if example.check {
        require(
            app.lookups.load(Ordering::SeqCst) == 1,
            "tool did not use local context exactly once",
        )?;
        for count in [
            &events.starts,
            &events.ends,
            &events.tool_starts,
            &events.tool_ends,
        ] {
            require(
                count.load(Ordering::SeqCst) == 1,
                "lifecycle callback was missing or duplicated",
            )?;
        }
    }
    example.finish().await
}
