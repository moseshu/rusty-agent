//! Anthropic SSE to `ModelStreamEvent` conversion.

use std::collections::{BTreeMap, VecDeque};

use futures::{StreamExt, stream, stream::BoxStream};
use ra_core::{
    error::{Error, ProviderErrorKind, Result},
    model::{
        ModelHandoffDefinition, ModelStream, ModelStreamEvent, ProviderKey, RawResponseEvent,
        ReplaySafety, RunItemStreamEvent, stamp_replay_safety,
    },
};
use serde_json::Value;

use super::convert;

/// Where a tool call's streamed argument fragments accumulate until the block is closed.
///
/// It is removed again while the message is assembled: the name is scratch space of this decoder,
/// not a field any consumer should ever see.
const INPUT_JSON_SCRATCH: &str = "__input_json";

pub(crate) fn events(
    response: reqwest::Response,
    provider: ProviderKey,
    handoffs: Vec<ModelHandoffDefinition>,
    request_id: Option<String>,
    unstarted: ReplaySafety,
) -> ModelStream<'static> {
    stream::unfold(
        Driver {
            bytes: response
                .bytes_stream()
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(transport_error))
                .boxed(),
            provider,
            handoffs,
            request_id,
            partial: Vec::new(),
            data: Vec::new(),
            ready: VecDeque::new(),
            pending: VecDeque::new(),
            message: None,
            blocks: BTreeMap::new(),
            emitted: false,
            finished: false,
            outcome: Outcome::Open,
            unstarted,
        },
        |mut driver| async move { driver.next().await.map(|event| (event, driver)) },
    )
    .boxed()
}

struct Driver {
    bytes: BoxStream<'static, Result<Vec<u8>>>,
    provider: ProviderKey,
    handoffs: Vec<ModelHandoffDefinition>,
    request_id: Option<String>,
    partial: Vec<u8>,
    data: Vec<String>,
    ready: VecDeque<Result<Value>>,
    pending: VecDeque<Result<ModelStreamEvent>>,
    message: Option<Value>,
    blocks: BTreeMap<u64, Value>,
    emitted: bool,
    finished: bool,
    outcome: Outcome,
    unstarted: ReplaySafety,
}

/// The terminal event this stream has already produced, if any.
///
/// Recorded rather than inferred because both terminal states have to stop the stream. Once the
/// input runs out, "no `message_stop` arrived" stays true on every later poll, so a driver that
/// only remembered whether it had settled handed a consumer draining the stream — which is what
/// draining is, for a stream whose end is `None` — the same failure forever.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Still reading; nothing terminal has been said.
    Open,
    /// `message_stop` arrived and the assembled turn was handed over.
    Settled,
    /// A failure was reported, and it was the last event.
    Failed,
}

impl Driver {
    async fn next(&mut self) -> Option<Result<ModelStreamEvent>> {
        if self.outcome == Outcome::Failed {
            return None;
        }
        loop {
            if let Some(event) = self.pending.pop_front() {
                self.emitted = true;
                return Some(event);
            }
            if let Some(frame) = self.ready.pop_front() {
                match frame.and_then(|frame| self.decode(frame)) {
                    Ok(events) => self.pending.extend(events.into_iter().map(Ok)),
                    // Whatever is still buffered describes a turn that will not arrive, so the
                    // first failure is the last event rather than a note in the middle.
                    Err(error) => return Some(self.fail(error)),
                }
            }
            if self.finished && self.ready.is_empty() && self.pending.is_empty() {
                if self.outcome == Outcome::Settled {
                    return None;
                }
                return Some(self.fail(Error::provider(
                    ProviderErrorKind::Network,
                    "Anthropic message stream ended without `message_stop`; the turn is incomplete",
                )));
            }
            match self.bytes.next().await {
                Some(Ok(chunk)) => self.push(&chunk),
                Some(Err(error)) => return Some(self.fail(error)),
                None => {
                    self.flush();
                    self.finished = true;
                }
            }
        }
    }

    /// Reports the failure that ends this stream, stamped with what the consumer has already seen.
    fn fail(&mut self, error: Error) -> Result<ModelStreamEvent> {
        self.finished = true;
        self.outcome = Outcome::Failed;
        let safety = if self.emitted {
            ReplaySafety::Unsafe
        } else {
            self.unstarted
        };
        Err(stamp_replay_safety(error, safety))
    }

    fn push(&mut self, chunk: &[u8]) {
        self.partial.extend_from_slice(chunk);
        while let Some(end) = self.partial.iter().position(|byte| *byte == b'\n') {
            let line = self.partial.drain(..=end).collect::<Vec<_>>();
            self.line(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']));
        }
    }
    fn flush(&mut self) {
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.line(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']));
        }
        self.frame();
    }
    fn line(&mut self, line: &str) {
        if line.is_empty() {
            self.frame();
        } else if let Some(data) = line.strip_prefix("data:") {
            self.data
                .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
        }
    }
    fn frame(&mut self) {
        if self.data.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        self.ready
            .push_back(serde_json::from_str(&data).map_err(|error| {
                convert::behavior_error("Anthropic stream contained invalid JSON")
                    .with_source(error)
            }));
    }

    fn decode(&mut self, frame: Value) -> Result<Vec<ModelStreamEvent>> {
        let event_type = frame
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| convert::behavior_error("Anthropic stream event has no type"))?
            .to_owned();
        if event_type == "error" {
            return Err(self.stream_error(&frame));
        }
        let mut events = vec![ModelStreamEvent::RawResponse(RawResponseEvent::new(
            self.provider.clone(),
            &event_type,
            frame.clone(),
        ))];
        match event_type.as_str() {
            // Both objects are written into as the stream goes on, and writing a key into a
            // `Value` that is not an object panics. The shape is therefore established once, here,
            // rather than re-checked at each of the writes that assume it.
            "message_start" => {
                self.message = Some(object(&frame, "message", "message_start")?);
            }
            "content_block_start" => {
                let index = frame.get("index").and_then(Value::as_u64).ok_or_else(|| {
                    convert::behavior_error("Anthropic content_block_start has no index")
                })?;
                self.blocks.insert(
                    index,
                    object(&frame, "content_block", "content_block_start")?,
                );
            }
            "content_block_delta" => self.apply_delta(&frame)?,
            "message_delta" => self.apply_message_delta(&frame),
            "message_stop" => {
                let payload = self.assembled()?;
                let response = convert::convert_response(
                    &payload,
                    self.request_id.clone(),
                    &self.handoffs,
                    &self.provider,
                )?;
                self.outcome = Outcome::Settled;
                self.finished = true;
                for item in response.output() {
                    events.push(ModelStreamEvent::RunItem(RunItemStreamEvent::new(
                        item.kind().label(),
                        item.clone(),
                    )));
                }
                events.push(ModelStreamEvent::Completed(Box::new(response)));
            }
            _ => {}
        }
        Ok(events)
    }

    fn apply_delta(&mut self, frame: &Value) -> Result<()> {
        let index = frame
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| convert::behavior_error("Anthropic content_block_delta has no index"))?;
        let delta = frame
            .get("delta")
            .ok_or_else(|| convert::behavior_error("Anthropic content_block_delta has no delta"))?;
        let block = self.blocks.get_mut(&index).ok_or_else(|| {
            convert::behavior_error("Anthropic content block delta arrived before its start")
        })?;
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => append(block, "text", required(delta, "text")?),
            Some("thinking_delta") => append(block, "thinking", required(delta, "thinking")?),
            Some("input_json_delta") => {
                append(block, INPUT_JSON_SCRATCH, required(delta, "partial_json")?);
            }
            Some("signature_delta") => {
                block["signature"] = Value::String(required(delta, "signature")?.to_owned());
            }
            Some(other) => {
                return Err(convert::behavior_error(format!(
                    "unsupported Anthropic content delta `{other}`"
                )));
            }
            None => {
                return Err(convert::behavior_error(
                    "Anthropic content delta has no type",
                ));
            }
        }
        Ok(())
    }
    /// Turns an `error` frame into a classified failure.
    ///
    /// The frame carries no status line, so its `error.type` is the only evidence there is. Reading
    /// every one of them as a protocol violation used to tell the retry layer to change the request
    /// when the endpoint had merely said it was overloaded — the one case where sending the same
    /// bytes again is the right move.
    fn stream_error(&self, frame: &Value) -> Error {
        let code = frame.pointer("/error/type").and_then(Value::as_str);
        let message = frame
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Anthropic stream reported an error");
        let suffix = self
            .request_id
            .as_ref()
            .map_or_else(String::new, |id| format!(" (request_id {id})"));
        let mut normalized = ra_core::model::NormalizedProviderError::new(
            convert::stream_error_kind(code, message),
            format!("Anthropic stream error{suffix}: {message}"),
        );
        if let Some(code) = code {
            normalized = normalized.with_error_code(code);
        }
        if let Some(id) = &self.request_id {
            normalized = normalized.with_request_id(id);
        }
        normalized.into_error()
    }

    fn apply_message_delta(&mut self, frame: &Value) {
        if let Some(message) = self.message.as_mut() {
            if let Some(stop_reason) = frame.pointer("/delta/stop_reason") {
                message["stop_reason"] = stop_reason.clone();
            }
            if let Some(usage) = frame.get("usage") {
                let mut merged = message
                    .get("usage")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                if let Some(delta) = usage.as_object() {
                    merged.extend(delta.clone());
                }
                message["usage"] = Value::Object(merged);
            }
        }
    }
    fn assembled(&mut self) -> Result<Value> {
        let mut message = self.message.take().ok_or_else(|| {
            convert::behavior_error("Anthropic stream ended before message_start")
        })?;
        let mut content = Vec::new();
        for (_, mut block) in std::mem::take(&mut self.blocks) {
            if let Some(partial) = block
                .as_object_mut()
                .and_then(|block| block.remove(INPUT_JSON_SCRATCH))
            {
                let input = serde_json::from_str(partial.as_str().unwrap_or_default()).map_err(
                    |error| {
                        convert::behavior_error("Anthropic streamed tool input is not valid JSON")
                            .with_source(error)
                    },
                )?;
                block["input"] = input;
            }
            content.push(block);
        }
        message["content"] = Value::Array(content);
        Ok(message)
    }
}

/// Reads a required object field, so that later writes into it cannot panic.
fn object(frame: &Value, key: &str, owner: &str) -> Result<Value> {
    frame
        .get(key)
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| {
            convert::behavior_error(format!("Anthropic {owner}.{key} must be an object"))
        })
}
fn append(block: &mut Value, field: &str, fragment: &str) {
    let text = block.get(field).and_then(Value::as_str).unwrap_or_default();
    block[field] = Value::String(format!("{text}{fragment}"));
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value.get(key).and_then(Value::as_str).ok_or_else(|| {
        convert::behavior_error(format!("Anthropic stream delta.{key} must be a string"))
    })
}
fn transport_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        ProviderErrorKind::Timeout
    } else {
        ProviderErrorKind::Network
    };
    ra_core::model::NormalizedProviderError::new(kind, "Anthropic stream transport failed")
        .with_source(error)
        .into_error()
}
