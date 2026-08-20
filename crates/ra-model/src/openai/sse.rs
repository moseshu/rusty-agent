//! Shared SSE frame parsing.
//!
//! Both `OpenAI` protocols stream `text/event-stream`, so the byte-level work — reassembling
//! frames across TCP reads, joining multi-line `data:` fields, and stopping at the terminator — is
//! written once here and consumed by each protocol's own event decoder.
//!
//! The reader is deliberately dumb about content. It hands back one JSON value per frame and knows
//! nothing about chunk shapes; deciding what a frame means is the protocol adapter's job.

use std::collections::VecDeque;

use futures::{Stream, StreamExt, stream, stream::BoxStream};
use ra_core::error::Result;
use serde_json::Value;

use super::error::{behavior_error, transport_error};

/// The terminator both `OpenAI` protocols send before closing the connection.
const DONE_MARKER: &str = "[DONE]";

/// Decoder state carried across network reads.
struct SseReader {
    bytes: BoxStream<'static, Result<Vec<u8>>>,
    /// Bytes received but not yet terminated by a newline.
    partial: Vec<u8>,
    /// `data:` field values collected for the frame currently being assembled.
    data: Vec<String>,
    /// Frames already decoded from the last read, waiting to be yielded.
    ready: VecDeque<Result<Value>>,
    /// Set once the stream is finished, so trailing bytes after `[DONE]` are ignored.
    done: bool,
}

/// Splits an HTTP response body into one JSON value per SSE frame.
///
/// A frame ends at a blank line, and a stream may also simply end: an endpoint that closes without
/// sending `[DONE]` is not an error here, because the protocol decoder above has its own view of
/// whether the events it received add up to a complete response. Treating a missing terminator as
/// a transport failure would turn a complete answer into a failed call.
pub(crate) fn frames(response: reqwest::Response) -> impl Stream<Item = Result<Value>> {
    let reader = SseReader {
        bytes: response
            .bytes_stream()
            .map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(transport_error))
            .boxed(),
        partial: Vec::new(),
        data: Vec::new(),
        ready: VecDeque::new(),
        done: false,
    };
    stream::unfold(reader, |mut reader| async move {
        loop {
            if let Some(frame) = reader.ready.pop_front() {
                return Some((frame, reader));
            }
            if reader.done {
                return None;
            }
            match reader.bytes.next().await {
                Some(Ok(chunk)) => reader.push(&chunk),
                Some(Err(error)) => {
                    reader.done = true;
                    return Some((Err(error), reader));
                }
                None => {
                    // A body that ends mid-frame still owes whatever it already sent.
                    reader.flush_partial();
                    reader.done = true;
                }
            }
        }
    })
}

impl SseReader {
    /// Consumes one network read, decoding every complete line it contains.
    fn push(&mut self, chunk: &[u8]) {
        self.partial.extend_from_slice(chunk);
        while let Some(position) = self.partial.iter().position(|byte| *byte == b'\n') {
            let line = self.partial.drain(..=position).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line);
            self.push_line(line.trim_end_matches(['\r', '\n']));
        }
    }

    /// Emits whatever a truncated final line already amounts to.
    fn flush_partial(&mut self) {
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            let line = String::from_utf8_lossy(&line);
            self.push_line(line.trim_end_matches(['\r', '\n']));
        }
        self.finish_frame();
    }

    fn push_line(&mut self, line: &str) {
        if line.is_empty() {
            self.finish_frame();
            return;
        }
        // A comment keeps the connection warm and carries nothing; `event:` and `id:` name a frame
        // whose payload still arrives on `data:`, which is the only field either protocol reads.
        let Some(value) = line.strip_prefix("data:") else {
            return;
        };
        self.data
            .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
    }

    /// Turns the accumulated `data:` lines into one JSON value.
    fn finish_frame(&mut self) {
        if self.data.is_empty() {
            return;
        }
        let payload = std::mem::take(&mut self.data).join("\n");
        if payload.trim() == DONE_MARKER {
            self.done = true;
            return;
        }
        self.ready
            .push_back(serde_json::from_str(&payload).map_err(|error| {
                behavior_error("OpenAI stream sent a frame that is not JSON").with_source(error)
            }));
    }
}
