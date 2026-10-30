// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use crate::network_policy::NetworkDecisionSource;
use crate::network_policy::NetworkPolicyDecision;
use crate::network_policy::NetworkProtocol;
use crate::reasons::REASON_DENIED;
use crate::reasons::REASON_METHOD_NOT_ALLOWED;
use crate::reasons::REASON_MITM_HOOK_DENIED;
use crate::reasons::REASON_MITM_REQUIRED;
use crate::reasons::REASON_NOT_ALLOWED;
use crate::reasons::REASON_NOT_ALLOWED_LOCAL;
use crate::reasons::REASON_PROXY_DISABLED;
use rama_http::Body;
use rama_http::Response;
use rama_http::StatusCode;
use serde::Serialize;
use tracing::error;

pub(crate) struct PolicyDecisionDetails<'a> {
    pub decision: NetworkPolicyDecision,
    pub reason: &'a str,
    pub source: NetworkDecisionSource,
    pub protocol: NetworkProtocol,
    pub host: &'a str,
    pub port: u16,
}

pub(crate) fn text_response(status: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from(body.to_string())))
}

pub(crate) fn json_response<T: Serialize>(value: &T) -> Response {
    let body = match serde_json::to_string(value) {
        Ok(body) => body,
        Err(err) => {
            error!("failed to serialize JSON response: {err}");
            "{}".to_string()
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|err| {
            error!("failed to build JSON response: {err}");
            Response::new(Body::from("{}"))
        })
}

pub(crate) fn blocked_header_value(reason: &str) -> &'static str {
    match reason {
        REASON_NOT_ALLOWED | REASON_NOT_ALLOWED_LOCAL => "blocked-by-allowlist",
        REASON_DENIED => "blocked-by-denylist",
        REASON_METHOD_NOT_ALLOWED => "blocked-by-method-policy",
        REASON_MITM_HOOK_DENIED => "blocked-by-mitm-hook",
        REASON_MITM_REQUIRED => "blocked-by-mitm-required",
        _ => "blocked-by-policy",
    }
}

pub(crate) fn blocked_message(reason: &str) -> &'static str {
    match reason {
        REASON_NOT_ALLOWED => "Domain not in allowlist.",
        REASON_NOT_ALLOWED_LOCAL => "Sandbox policy blocks local/private network addresses.",
        REASON_DENIED => "Domain denied by the sandbox policy.",
        REASON_METHOD_NOT_ALLOWED => "Method not allowed in limited mode.",
        REASON_MITM_HOOK_DENIED => "HTTPS request denied by MITM hook policy.",
        REASON_MITM_REQUIRED => "MITM required for limited HTTPS.",
        REASON_PROXY_DISABLED => "network proxy is disabled",
        _ => "Request blocked by network policy.",
    }
}

pub(crate) fn blocked_text_response(reason: &str) -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("content-type", "text/plain")
        .header("x-proxy-error", blocked_header_value(reason))
        .body(Body::from(blocked_message(reason)))
        .unwrap_or_else(|_| Response::new(Body::from("blocked")))
}
pub(crate) fn blocked_message_with_policy(
    reason: &str,
    details: &PolicyDecisionDetails<'_>,
) -> String {
    let _ = (details.reason, details.host);
    blocked_message(reason).to_string()
}

pub(crate) fn blocked_text_response_with_policy(
    reason: &str,
    details: &PolicyDecisionDetails<'_>,
) -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("content-type", "text/plain")
        .header("x-proxy-error", blocked_header_value(reason))
        .body(Body::from(blocked_message_with_policy(reason, details)))
        .unwrap_or_else(|_| Response::new(Body::from("blocked")))
}
