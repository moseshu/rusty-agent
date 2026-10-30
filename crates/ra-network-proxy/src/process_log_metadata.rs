// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Process attribution captured at launch for network policy logs.
//! Executor registration identity comes from the executor, never the controller.

/// Registry-issued identity of the executor that launched a process.
#[derive(Clone)]
pub struct ExecutorLogIdentity {
    pub environment_id: String,
    pub registration_id: String,
}

/// Correlation metadata only; these values do not authorize network requests.
#[derive(Clone, Default)]
pub struct NetworkProxyProcessLogMetadata {
    pub thread_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub executor_identity: Option<ExecutorLogIdentity>,
}
