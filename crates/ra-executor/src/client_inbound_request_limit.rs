// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    unreachable_pub,
    reason = "Private implementation items are exposed only by the feature-gated test facade."
)]
#![cfg_attr(
    feature = "test-api",
    allow(missing_docs, reason = "Hidden source-test facade access.")
)]
use ra_executor_protocol::EXEC_CLOSED_METHOD;
use ra_executor_protocol::EXEC_EXITED_METHOD;
use ra_executor_protocol::EXEC_OUTPUT_DELTA_METHOD;
use ra_executor_protocol::HTTP_REQUEST_BODY_DELTA_METHOD;
use ra_executor_protocol::JSONRPCMessage;
use ra_executor_protocol::MAX_HTTP_BODY_DELTA_BYTES;

// A transport may materialize one larger frame before its JSON-RPC kind is known.
pub const MAX_CLIENT_INBOUND_REQUEST_LEN: usize = 8 * 1024;
// Streamed HTTP bodies carry up to 1 MiB before base64 and JSON-RPC framing.
pub const MAX_CLIENT_INBOUND_NOTIFICATION_LEN: usize = 2 * MAX_HTTP_BODY_DELTA_BYTES;

#[must_use]
pub fn client_inbound_message_exceeded_limit(
    message: Result<&JSONRPCMessage, &serde_json::Error>,
    encoded_len: usize,
    max_request_len: usize,
) -> Option<usize> {
    let max_len = match message {
        Ok(JSONRPCMessage::Notification(notification))
            if matches!(
                notification.method.as_str(),
                EXEC_OUTPUT_DELTA_METHOD
                    | EXEC_EXITED_METHOD
                    | EXEC_CLOSED_METHOD
                    | HTTP_REQUEST_BODY_DELTA_METHOD
            ) =>
        {
            MAX_CLIENT_INBOUND_NOTIFICATION_LEN
        }
        Ok(JSONRPCMessage::Request(_) | JSONRPCMessage::Notification(_)) | Err(_) => {
            max_request_len
        }
        Ok(JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_)) => return None,
    };
    (encoded_len > max_len).then_some(max_len)
}
