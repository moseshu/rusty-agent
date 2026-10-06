//! The wire messages and the correlation of the agent's requests with their answers, as Codex's
//! app-server has them (`app-server-protocol/src/rpc.rs`, `app-server/src/error_code.rs`,
//! `app-server/src/outgoing_message.rs`).

use std::time::Duration;

use ra_core::session::SessionId;
use ra_protocol::frame::{
    JsonRpcError, JsonRpcErrorObject, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, PendingRequests, RequestId, W3cTraceContext, error_code,
};
use serde_json::{Value, json};
use tokio::time::timeout;

fn decode(line: &str) -> JsonRpcMessage {
    serde_json::from_str(line).unwrap()
}

fn encode(message: impl Into<JsonRpcMessage>) -> Value {
    serde_json::to_value(message.into()).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

/// Messages go out as JSON-RPC 2.0 without the `jsonrpc` member, and members left unset are left
/// out, as Codex writes them.
#[test]
fn messages_are_written_without_the_jsonrpc_member() {
    assert_eq!(
        encode(JsonRpcRequest::new(1, "thread/start")),
        json!({"id": 1, "method": "thread/start"})
    );
    assert_eq!(
        encode(
            JsonRpcRequest::new("req-a", "turn/start")
                .with_params(json!({"threadId": "t"}))
                .with_trace(W3cTraceContext::new().with_traceparent("00-abc-def-01"))
        ),
        json!({
            "id": "req-a",
            "method": "turn/start",
            "params": {"threadId": "t"},
            "trace": {"traceparent": "00-abc-def-01"},
        })
    );
    assert_eq!(
        encode(JsonRpcNotification::new("initialized")),
        json!({"method": "initialized"})
    );
    assert_eq!(
        encode(JsonRpcResponse::new(2, json!({}))),
        json!({"id": 2, "result": {}})
    );
    assert_eq!(
        encode(JsonRpcError::new(
            3,
            JsonRpcErrorObject::invalid_request("Not initialized")
        )),
        json!({"error": {"code": -32600, "message": "Not initialized"}, "id": 3})
    );
}

/// A message is read as the first kind whose members it has: request (`id`, `method`),
/// notification (`method`), response (`id`, `result`), error (`id`, `error`).
#[test]
fn messages_are_told_apart_by_their_members() {
    assert!(matches!(
        decode(r#"{"id":1,"method":"initialize","params":{}}"#),
        JsonRpcMessage::Request(request) if request.method() == "initialize"
            && request.id() == &RequestId::Integer(1)
    ));
    assert!(matches!(
        decode(r#"{"method":"initialized"}"#),
        JsonRpcMessage::Notification(notification) if notification.method() == "initialized"
            && notification.params().is_none()
    ));
    assert!(matches!(
        decode(r#"{"id":"s-1","result":null}"#),
        JsonRpcMessage::Response(response) if response.id() == &RequestId::from("s-1")
            && response.result().is_null()
    ));
    assert!(matches!(
        decode(r#"{"id":7,"error":{"code":-32601,"message":"no such method","data":{"m":"x"}}}"#),
        JsonRpcMessage::Error(error) if error.id() == &RequestId::Integer(7)
            && error.error().code() == error_code::METHOD_NOT_FOUND
            && error.error().data() == Some(&json!({"m": "x"}))
    ));
}

/// A message that has both `result` and `error`, which JSON-RPC forbids, is read as a response,
/// the first of the two in Codex's order.
#[test]
fn a_message_with_both_result_and_error_is_read_as_a_response() {
    assert_eq!(
        decode(r#"{"id":1,"result":2,"error":{"code":-32603,"message":"x"}}"#),
        JsonRpcResponse::new(1, json!(2)).into()
    );
}

/// A client that follows JSON-RPC to the letter sends `"jsonrpc": "2.0"`; it is accepted and
/// ignored, as Codex ignores it.
#[test]
fn the_jsonrpc_member_a_client_sends_is_ignored() {
    let message = decode(r#"{"jsonrpc":"2.0","id":1,"method":"thread/list"}"#);
    assert_eq!(message, JsonRpcRequest::new(1, "thread/list").into());
    assert_eq!(encode(message), json!({"id": 1, "method": "thread/list"}));
}

/// What a newer peer adds to the parameters reaches the method untouched.
#[test]
fn parameters_are_carried_verbatim() {
    let line =
        r#"{"id":1,"method":"turn/start","params":{"threadId":"t","fromTheFuture":{"a":[1,2]}}}"#;
    let JsonRpcMessage::Request(request) = decode(line) else {
        panic!("expected a request");
    };
    assert_eq!(
        request.into_params(),
        Some(json!({"threadId": "t", "fromTheFuture": {"a": [1, 2]}}))
    );
}

/// Something that is no message at all is refused, as Codex refuses to read it.
#[test]
fn a_line_that_is_no_message_is_refused() {
    for line in [r#"{"id":1}"#, r#"{"params":{}}"#, "[]", "not json"] {
        assert!(
            serde_json::from_str::<JsonRpcMessage>(line).is_err(),
            "{line}"
        );
    }
}

/// A request whose id is neither a string nor an integer still has the members of a notification,
/// and is read as one, as Codex's untagged decoding reads it: it gets no answer.
#[test]
fn a_request_with_an_unusable_id_is_read_as_a_notification() {
    assert_eq!(
        decode(r#"{"id":true,"method":"m"}"#),
        JsonRpcNotification::new("m").into()
    );
}

/// Every kind survives a round trip through the wire.
#[test]
fn every_kind_round_trips() {
    for message in [
        JsonRpcMessage::from(JsonRpcRequest::new(-5, "m").with_params(json!([1, "two", null]))),
        JsonRpcNotification::new("thread/started")
            .with_params(json!({"thread": {"id": "t"}}))
            .into(),
        JsonRpcResponse::new("r", json!({"ok": true})).into(),
        JsonRpcError::new(
            9,
            JsonRpcErrorObject::internal_error("boom").with_data(json!({"why": "because"})),
        )
        .into(),
    ] {
        let line = serde_json::to_string(&message).unwrap();
        assert_eq!(decode(&line), message, "{line}");
    }
}

/// Request ids print as they are written, and order as Codex's do: strings before integers.
#[test]
fn request_ids_print_and_order_as_codexs() {
    assert_eq!(RequestId::from(42).to_string(), "42");
    assert_eq!(RequestId::from("abc").to_string(), "abc");
    assert!(RequestId::from("z") < RequestId::from(0));
    assert!(RequestId::from(1) < RequestId::from(2));
}

/// The error codes and helpers are Codex's.
#[test]
fn error_codes_are_codexs() {
    assert_eq!(JsonRpcErrorObject::invalid_request("x").code(), -32600);
    assert_eq!(JsonRpcErrorObject::method_not_found("x").code(), -32601);
    assert_eq!(JsonRpcErrorObject::invalid_params("x").code(), -32602);
    assert_eq!(JsonRpcErrorObject::internal_error("x").code(), -32603);
    let overloaded = JsonRpcErrorObject::overloaded();
    assert_eq!(overloaded.code(), -32001);
    assert_eq!(overloaded.message(), "Server overloaded; retry later.");
}

// ---------------------------------------------------------------------------------------------
// Pending requests
// ---------------------------------------------------------------------------------------------

/// The agent's requests take integer ids from zero, in the order they are made, as Codex's do.
#[test]
fn requests_take_integer_ids_from_zero() {
    let pending = PendingRequests::new();
    let (first, _a) = pending.register("item/tool/call", Some(json!({"tool": "t"})), None);
    let (second, _b) = pending.register("item/fileChange/requestApproval", None, None);
    assert_eq!(first.id(), &RequestId::Integer(0));
    assert_eq!(first.method(), "item/tool/call");
    assert_eq!(first.params(), Some(&json!({"tool": "t"})));
    assert_eq!(second.id(), &RequestId::Integer(1));
    assert_eq!(second.params(), None);
    assert_eq!(pending.len(), 2);
}

/// The host's response reaches the waiter, once.
#[tokio::test]
async fn a_response_reaches_the_waiter_once() {
    let pending = PendingRequests::new();
    let (request, answer) = pending.register("item/commandExecution/requestApproval", None, None);
    assert!(pending.notify_response(request.id(), json!({"decision": "accept"})));
    assert_eq!(
        timeout(Duration::from_secs(1), answer)
            .await
            .unwrap()
            .unwrap(),
        Ok(json!({"decision": "accept"}))
    );
    assert!(!pending.notify_response(request.id(), json!({})));
    assert!(pending.is_empty());
}

/// Codex's `notify_client_error_forwards_error_to_waiter`.
#[tokio::test]
async fn an_error_reaches_the_waiter() {
    let pending = PendingRequests::new();
    let (request, answer) = pending.register("item/fileChange/requestApproval", None, None);
    let error = JsonRpcErrorObject::internal_error("refresh failed");
    assert!(pending.notify_error(request.id(), error.clone()));
    assert_eq!(
        timeout(Duration::from_secs(1), answer)
            .await
            .unwrap()
            .unwrap(),
        Err(error)
    );
}

/// An answer to an id no request is waiting under is not taken for another request's.
#[test]
fn an_answer_to_an_unknown_id_is_ignored() {
    let pending = PendingRequests::new();
    let (_request, _answer) = pending.register("m", None, None);
    assert!(!pending.notify_response(&RequestId::Integer(99), json!(null)));
    assert!(!pending.notify_error(
        &RequestId::from("0"),
        JsonRpcErrorObject::internal_error("x")
    ));
    assert_eq!(pending.len(), 1);
}

/// A waiter that went away does not keep the request waiting: the answer still settles it.
#[test]
fn an_answer_settles_a_request_whose_waiter_went_away() {
    let pending = PendingRequests::new();
    let (request, answer) = pending.register("m", None, None);
    drop(answer);
    assert!(pending.notify_response(request.id(), json!(1)));
    assert!(pending.is_empty());
}

/// A request withdrawn without an error closes its answer channel: Codex's `cancel_request`.
#[tokio::test]
async fn a_withdrawn_request_closes_its_answer_channel() {
    let pending = PendingRequests::new();
    let (request, answer) = pending.register("m", None, None);
    assert!(pending.cancel_request(request.id()));
    assert!(!pending.cancel_request(request.id()));
    assert!(
        timeout(Duration::from_secs(1), answer)
            .await
            .unwrap()
            .is_err()
    );
    assert!(!pending.notify_response(request.id(), json!(1)));
}

/// Codex's `cancel_all_requests`: every request is withdrawn, with the error if one is given.
#[tokio::test]
async fn withdrawing_every_request_settles_each() {
    let pending = PendingRequests::new();
    let (_a, first) = pending.register("a", None, None);
    let (_b, second) = pending.register("b", None, Some(SessionId::new("t")));
    let error = JsonRpcErrorObject::internal_error("connection closed");
    pending.cancel_all(Some(&error));
    assert_eq!(
        timeout(Duration::from_secs(1), first)
            .await
            .unwrap()
            .unwrap(),
        Err(error.clone())
    );
    assert_eq!(
        timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap(),
        Err(error)
    );
    assert!(pending.is_empty());

    let (_c, third) = pending.register("c", None, None);
    pending.cancel_all(None);
    assert!(
        timeout(Duration::from_secs(1), third)
            .await
            .unwrap()
            .is_err()
    );
}

/// Codex's `pending_requests_for_thread_returns_thread_requests_in_request_id_order`, with a
/// request of another thread and one of none left out.
#[test]
fn a_threads_requests_are_listed_in_id_order() {
    let pending = PendingRequests::new();
    let thread = SessionId::new("thread-1");
    let mut expected = Vec::new();
    for method in ["item/tool/call", "item/tool/requestUserInput"] {
        let (request, _answer) = pending.register(method, None, Some(thread.clone()));
        expected.push(request);
        let (_other, _answer) = pending.register("other", None, Some(SessionId::new("thread-2")));
    }
    let (_none, _answer) = pending.register("none", None, None);
    let (last, _answer) = pending.register(
        "item/fileChange/requestApproval",
        None,
        Some(thread.clone()),
    );
    expected.push(last);
    assert_eq!(pending.pending_for_thread(&thread), expected);
}

/// Codex's `cancel_requests_for_thread_cancels_all_thread_requests`; another thread's requests
/// keep waiting.
#[tokio::test]
async fn withdrawing_a_threads_requests_leaves_the_others() {
    let pending = PendingRequests::new();
    let thread = SessionId::new("thread-1");
    let (_a, tool) = pending.register("item/tool/call", None, Some(thread.clone()));
    let (_b, input) = pending.register("item/tool/requestUserInput", None, Some(thread.clone()));
    let (other, _other_answer) = pending.register("m", None, Some(SessionId::new("thread-2")));
    let error = JsonRpcErrorObject::internal_error("tracked request cancelled");

    pending.cancel_for_thread(&thread, Some(&error));

    assert_eq!(
        timeout(Duration::from_secs(1), tool)
            .await
            .unwrap()
            .unwrap(),
        Err(error.clone())
    );
    assert_eq!(
        timeout(Duration::from_secs(1), input)
            .await
            .unwrap()
            .unwrap(),
        Err(error)
    );
    assert!(pending.pending_for_thread(&thread).is_empty());
    assert_eq!(
        pending.pending_for_thread(&SessionId::new("thread-2")),
        vec![other]
    );
}

/// Many tasks registering and answering at once never share an id or lose an answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_keep_distinct_ids_and_answers() {
    let pending = std::sync::Arc::new(PendingRequests::new());
    let mut tasks = Vec::new();
    for index in 0..64_i64 {
        let pending = std::sync::Arc::clone(&pending);
        tasks.push(tokio::spawn(async move {
            let (request, answer) = pending.register("m", Some(json!(index)), None);
            let answering = std::sync::Arc::clone(&pending);
            let id = request.id().clone();
            tokio::spawn(async move { answering.notify_response(&id, json!(index)) });
            let result = timeout(Duration::from_secs(5), answer).await.unwrap();
            (request.id().clone(), result.unwrap().unwrap())
        }));
    }
    let mut ids = Vec::new();
    for (index, task) in tasks.into_iter().enumerate() {
        let (id, result) = task.await.unwrap();
        assert_eq!(result, json!(index));
        ids.push(id);
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 64);
    assert!(pending.is_empty());
}
