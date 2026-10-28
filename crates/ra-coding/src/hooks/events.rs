// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Preserve the source hook discovery and execution contract."
)]
#[path = "events/common.rs"]
pub(crate) mod common;
#[path = "events/compact.rs"]
pub(crate) mod compact;
#[path = "events/interrupt.rs"]
pub(crate) mod interrupt;
#[path = "events/permission_request.rs"]
pub(crate) mod permission_request;
#[path = "events/post_tool_use.rs"]
pub(crate) mod post_tool_use;
#[path = "events/pre_tool_use.rs"]
pub(crate) mod pre_tool_use;
#[path = "events/session_end.rs"]
pub(crate) mod session_end;
#[path = "events/session_start.rs"]
pub(crate) mod session_start;
#[path = "events/stop.rs"]
pub(crate) mod stop;
#[path = "events/user_prompt_submit.rs"]
pub(crate) mod user_prompt_submit;
