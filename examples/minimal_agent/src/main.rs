//! The smallest agent this framework can run: one tool and one model, both defined here.
//!
//! # Why this example is a gate and not a demo
//!
//! It depends on `ra-core` and `ra-runtime` and nothing else — not `ra-coding`, not `ra-tools`,
//! not even `ra-model`. Everything a run needs that is *not* kernel machinery is written below in
//! about a hundred lines: a tool, a model, and a resolver. If this stops compiling, some capability
//! a third party needs has moved behind a reference product, and the framework is no longer the
//! general thing it claims to be. That is the whole reason the example exists; see milestone M1 and
//! MVP acceptance item 6b in the development plan.
//!
//! Bringing its own [`Model`] is part of that claim rather than a testing shortcut. A provider
//! lives in `ra-model`, and depending on one here would prove that you can run *this project's*
//! adapters — not that you can bring your own. It also lets the example run in CI with no API key
//! and no network.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    state::RunId,
    tool::{Tool, ToolContext, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunOutcome, RunRequest, Runner},
};
use serde_json::json;

/// A tool this example owns, to show what a third party has to implement.
///
/// Only three methods are required — identity, schema, and the call itself. Everything else
/// (approval policy, concurrency, timeouts, failure shaping) has a default, which is the property
/// that lets the framework add a hook later without breaking tools that never heard of it.
struct CountLines {
    origin: ToolOrigin,
    schema: ToolSchema,
}

impl CountLines {
    fn new() -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new("count_lines")?,
            schema: ToolSchema::new(
                "count_lines",
                json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "Text to count lines in." }
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }),
            )?
            .with_description("Counts the lines in a block of text."),
        })
    }
}

#[async_trait]
impl Tool for CountLines {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        // Arguments arrive as parsed JSON. A tool that wants them decoded into a Rust type
        // declares a `func_schema` instead and reads `context.decoded_input()`.
        let text = context
            .arguments()
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::caller("count_lines requires a string `text` argument"))?;
        Ok(ToolOutput::text(format!("{} lines", text.lines().count())))
    }
}

/// A model this example owns, replaying a fixed two-step script.
///
/// Turn one asks for the tool; turn two answers. That is the shortest path that actually exercises
/// the loop — a single-response model would never reach tool dispatch or a second turn.
///
/// Only `get_response` is written here. The loop drives every call through `stream_response`, but
/// its default answers with this method's result as a one-event stream — which is why a model with
/// nothing to stream does not have to know that. An adapter speaking a protocol that *can* stream
/// overrides it, and earns the overlap of starting tools before generation finishes.
struct ScriptedModel {
    calls: AtomicUsize,
}

impl ScriptedModel {
    /// One scripted turn: a tool call first, an answer second.
    fn next_response(&self) -> ModelResponse {
        let turn = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if turn == 0 {
            vec![RunItem::new(
                ItemId::new("call-item-1"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("call-1"),
                    "count_lines",
                    json!({ "text": "alpha\nbeta\ngamma" }),
                )),
            )]
        } else {
            vec![RunItem::new(
                ItemId::new("message-1"),
                RunItemKind::Message(Message::assistant(
                    "The text has 3 lines.",
                    OutputPhase::Final,
                )),
            )]
        };
        ModelResponse::new(output)
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(self.next_response())
    }
}

/// Binds every model name to the one model above.
struct SingleModel(Arc<dyn Model>);

impl ModelResolver for SingleModel {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("example"),
                Some("scripted".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let tool = Arc::new(CountLines::new()?) as Arc<dyn Tool>;
    let agent = AgentSpec::builder()
        .id(AgentId::new("counter"))
        .name("Counter")
        .instructions("Count lines when asked, then answer.")
        .tools(vec![tool])
        .build()?;

    let model = Arc::new(ScriptedModel {
        calls: AtomicUsize::new(0),
    });
    let request = RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(SingleModel(model)),
        RunId::generate(),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user(
            "How many lines are in the sample text?",
        ))],
    );

    let result = Runner::run(request).await?;

    println!("outcome : {:?}", result.outcome());
    println!("turns   : {}", result.turns());
    println!("usage   : {} requests", result.usage().requests());

    // The delivered answer, without walking `new_items` and deciding for yourself which message was
    // the answer: settlement already stamped that, and `final_text` reads the stamp back.
    println!("answer  : {}", result.final_text());

    if !matches!(result.outcome(), RunOutcome::Completed { .. }) {
        return Err(Error::caller(format!(
            "the minimal agent did not complete: {:?}",
            result.outcome()
        )));
    }
    Ok(())
}
