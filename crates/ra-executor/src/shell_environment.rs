// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Scoped environment credentials do not pass into child executors.
use std::collections::HashMap;
#[allow(
    dead_code,
    reason = "Process-session attribution is deferred with the full execution backend."
)]
pub(crate) const CODEX_SESSION_ID_ENV_VAR: &str = "CODEX_SESSION_ID";
#[allow(
    dead_code,
    reason = "Process-session attribution is deferred with the full execution backend."
)]
pub(crate) const CODEX_THREAD_ID_ENV_VAR: &str = "CODEX_THREAD_ID";
#[allow(
    dead_code,
    reason = "Process-session attribution is deferred with the full execution backend."
)]
pub(crate) const CODEX_TOOL_CALL_ID_ENV_VAR: &str = "CODEX_TOOL_CALL_ID";
pub(crate) const CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN";
pub(crate) const OPENAI_FEDERATION_RULE_ID_ENV_VAR: &str = "OPENAI_FEDERATION_RULE_ID";
pub(crate) const OPENAI_IDENTITY_TOKEN_FILE_ENV_VAR: &str = "OPENAI_IDENTITY_TOKEN_FILE";
pub(crate) const OPENAI_WORKLOAD_IDENTITY_CONTEXT_ENV_VAR: &str =
    "OPENAI_WORKLOAD_IDENTITY_CONTEXT";

/// Environment variables that model-reachable child processes must not inherit.
pub(crate) const NON_INHERITABLE_ENV_VARS: &[&str] = &[
    CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR,
    "NODE_REPL_AUTH_TOKEN",
    "CODEX_GUARDIAN_DECISIONS_API_KEY",
    OPENAI_FEDERATION_RULE_ID_ENV_VAR,
    OPENAI_IDENTITY_TOKEN_FILE_ENV_VAR,
    OPENAI_WORKLOAD_IDENTITY_CONTEXT_ENV_VAR,
];

pub(crate) fn is_non_inheritable_env_var(name: &str) -> bool {
    NON_INHERITABLE_ENV_VARS
        .iter()
        .any(|restricted| restricted.eq_ignore_ascii_case(name))
}

#[allow(
    dead_code,
    reason = "Process-session attribution is deferred with the full execution backend."
)]
pub(crate) fn set_tool_call_id_env_var(
    env: &mut HashMap<String, String>,
    tool_call_id: Option<&str>,
) {
    #[cfg(windows)]
    env.retain(|name, _| !name.eq_ignore_ascii_case(CODEX_TOOL_CALL_ID_ENV_VAR));
    #[cfg(not(windows))]
    env.remove(CODEX_TOOL_CALL_ID_ENV_VAR);
    if let Some(tool_call_id) = tool_call_id.filter(|tool_call_id| !tool_call_id.contains('\0')) {
        env.insert(
            CODEX_TOOL_CALL_ID_ENV_VAR.to_string(),
            tool_call_id.to_string(),
        );
    }
}

/// Configures a child command to omit non-inheritable variables from the
/// process environment and explicit command overrides.
///
/// This prevents accidental propagation of Codex launch context; it is not a
/// filesystem security boundary for the referenced identity-token file.
pub(crate) fn scrub_non_inheritable_env_vars(command: &mut std::process::Command) {
    let configured_names = command
        .get_envs()
        .map(|(name, _)| name.to_os_string())
        .collect::<Vec<_>>();

    for name in NON_INHERITABLE_ENV_VARS {
        command.env_remove(name);
    }
    for name in std::env::vars_os()
        .map(|(name, _)| name)
        .chain(configured_names)
    {
        if name.to_str().is_some_and(is_non_inheritable_env_var) {
            command.env_remove(name);
        }
    }
}
