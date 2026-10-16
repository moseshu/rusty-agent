//! Shared provider setup and local HTTP fixtures for the framework tutorials.

// Each tutorial compiles this source independently and uses a different subset of the helpers.
#![allow(dead_code)]

use std::{sync::Arc, time::Duration};

use rusty_agent::{
    model::ModelProvider,
    openai::{auth::OpenAiAuth, responses::OpenAiResponsesProvider},
    prelude::*,
    provider::quirks::ProviderQuirks,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

/// One expected HTTP request and the output item returned by its local fixture.
pub(crate) type Step = Box<dyn FnOnce(&Value) -> Result<Value> + Send>;

/// An explicitly configured runner and its optional offline server.
pub(crate) struct Example {
    /// Reused across all calls in a tutorial.
    pub(crate) runner: AgentRunner,
    /// Whether this invocation selected the local HTTP fixture.
    pub(crate) check: bool,
    models: Arc<ProviderRegistry>,
    server: Option<JoinHandle<Result<()>>>,
}

impl Example {
    /// Verifies that every expected request ran, then closes the shared provider.
    pub(crate) async fn finish(mut self) -> Result<()> {
        if let Some(server) = self.server.take() {
            server.await.map_err(|error| {
                Error::caller("offline server task failed").with_source(error)
            })??;
            println!("Offline HTTP check passed.");
        }
        self.models.close().await
    }
}

impl Drop for Example {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

/// Uses the shipped Responses provider in both live and offline modes.
pub(crate) async fn setup(steps: Vec<Step>) -> Result<Example> {
    let check = std::env::args().any(|argument| argument == "--check");
    let (auth, model, server) = if check {
        let listener = io(TcpListener::bind("127.0.0.1:0").await)?;
        let address = io(listener.local_addr())?;
        let server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(20), serve(listener, steps))
                .await
                .map_err(|_| Error::caller("offline HTTP check timed out: missing requests"))?
                .inspect_err(|error| eprintln!("Offline HTTP fixture failed: {error}"))
        });
        (
            OpenAiAuth::new("offline").with_base_url(format!("http://{address}/v1/")),
            "offline-model".to_owned(),
            Some(server),
        )
    } else {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| Error::config("set OPENAI_API_KEY for a live run, or pass --check"))?;
        let model = std::env::var("RA_MODEL")
            .map_err(|_| Error::config("set RA_MODEL to a model available at your endpoint"))?;
        let mut auth = OpenAiAuth::new(api_key);
        if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
            auth = auth.with_base_url(base_url);
        }
        (auth, model, None)
    };
    // tutorial:provider:start
    let provider = ProviderKey::new("openai");
    let models = Arc::new(
        ProviderRegistry::builder(provider.clone())
            .register(ProviderRegistration::new(
                provider,
                ApiProtocol::OpenAiResponses,
                move |quirks: ProviderQuirks| {
                    Ok(Arc::new(
                        OpenAiResponsesProvider::new(auth.clone(), model.clone())?
                            .with_quirks(quirks),
                    ) as Arc<dyn ModelProvider>)
                },
            ))
            .build()?,
    );
    let runner = AgentRunner::new(models.clone());
    // tutorial:provider:end
    Ok(Example {
        runner,
        check,
        models,
        server,
    })
}

/// Fails a check without relying on debug assertions.
pub(crate) fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::caller(message))
    }
}

/// A Responses message fixture. The server supplies unique item IDs.
pub(crate) fn answer(text: &str) -> Value {
    json!({"type":"message", "role":"assistant", "status":"completed",
        "content":[{"type":"output_text", "text":text, "annotations":[]}]})
}

/// A Responses function call fixture.
pub(crate) fn call(name: &str, arguments: &Value) -> Value {
    json!({"type":"function_call", "call_id":"call-1", "name":name,
        "arguments":arguments.to_string(), "status":"completed"})
}

/// Text from the wire request's message items, in chronological order.
pub(crate) fn messages(request: &Value) -> Vec<String> {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["content"].as_array())
        .map(|content| {
            content
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect()
        })
        .collect()
}

/// Checks that a tool result reached the following model request.
pub(crate) fn has_tool_output(request: &Value, expected: &str) -> bool {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| {
            item["type"] == "function_call_output"
                && match &item["output"] {
                    Value::String(text) => text.contains(expected),
                    Value::Array(blocks) => blocks.iter().any(|block| {
                        block["type"] == "input_text"
                            && block["text"]
                                .as_str()
                                .is_some_and(|text| text.contains(expected))
                    }),
                    _ => false,
                }
        })
}

fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|error| Error::caller("offline HTTP fixture I/O failed").with_source(error))
}

// This deliberately small HTTP fixture accepts only the shipped client's JSON POST shape.
// It is test data for the tutorials, not an HTTP server intended for application use.
async fn request(socket: &mut TcpStream) -> Result<Value> {
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        require(
            bytes.len() < 1_048_576,
            "offline request exceeded fixture limit",
        )?;
        let mut chunk = [0; 4096];
        let count = io(socket.read(&mut chunk).await)?;
        require(count > 0, "offline HTTP request ended early")?;
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            require(
                headers.starts_with("post /v1/responses http/1.1\r\n"),
                "wrong provider route",
            )?;
            require(
                headers.contains("\r\nauthorization: bearer offline"),
                "wrong offline credential",
            )?;
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.parse::<usize>().ok())
                .ok_or_else(|| Error::caller("offline request needs Content-Length"))?;
            require(
                length < 1_048_576,
                "offline request body exceeded fixture limit",
            )?;
            break (end + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let count = io(socket.read(&mut chunk).await)?;
        require(count > 0, "offline HTTP body ended early")?;
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length])
        .map_err(|error| Error::caller("invalid fixture request JSON").with_source(error))
}

async fn serve(listener: TcpListener, steps: Vec<Step>) -> Result<()> {
    for (index, step) in steps.into_iter().enumerate() {
        let (mut socket, _) = io(listener.accept().await)?;
        let request = request(&mut socket).await?;
        require(request["model"] == "offline-model", "wrong provider model")?;
        require(request["stream"] == true, "runner did not request SSE")?;
        let mut item = step(&request)?;
        item["id"] = json!(format!("item-{index}"));
        let response_id = format!("response-{index}");
        let mut events = vec![json!({"type":"response.created",
            "response":{"id":response_id, "status":"in_progress"}})];
        if let Some(text) = item["content"][0]["text"].as_str() {
            events.push(
                json!({"type":"response.output_item.added", "output_index":0,
                "item":{"type":"message", "id":item["id"], "role":"assistant", "content":[]}}),
            );
            for delta in text.split_inclusive(' ') {
                events.push(
                    json!({"type":"response.output_text.delta", "item_id":item["id"],
                    "output_index":0, "content_index":0, "delta":delta}),
                );
            }
        }
        events.push(json!({"type":"response.completed", "response":{
            "id":response_id, "status":"completed", "output":[item],
            "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}
        }}));
        let mut body = String::new();
        for event in events {
            body.push_str("data: ");
            body.push_str(&event.to_string());
            body.push_str("\n\n");
        }
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        io(socket.write_all(headers.as_bytes()).await)?;
        io(socket.write_all(body.as_bytes()).await)?;
        io(socket.shutdown().await)?;
    }
    Ok(())
}
