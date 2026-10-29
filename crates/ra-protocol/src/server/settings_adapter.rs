//! The host boundary for Codex's product-owned settings. The protocol ports their wire shape;
//! the selected product previews and applies their concrete execution mechanism.

use ra_core::{item::ToolApproval, tool::Tool};
use ra_runtime::{
    runner::RunConfig,
    thread::{AgentThread, ThreadEvent},
};
use serde_json::Value;

use super::{
    approvals::ReviewDecision,
    permission_requests::ThreadServerRequests,
    protocol::{ServerRequestPayload, ThreadPolicyOverrides},
};

/// A call a turn's run stopped on, for which the client is to be asked.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct PendingApproval<'a> {
    thread_id: &'a str,
    turn_id: &'a str,
    started_at_ms: i64,
    approval: &'a ToolApproval,
}

impl<'a> PendingApproval<'a> {
    pub(crate) const fn new(
        thread_id: &'a str,
        turn_id: &'a str,
        started_at_ms: i64,
        approval: &'a ToolApproval,
    ) -> Self {
        Self {
            thread_id,
            turn_id,
            started_at_ms,
            approval,
        }
    }

    /// The thread.
    #[must_use]
    pub const fn thread_id(&self) -> &'a str {
        self.thread_id
    }

    /// The turn whose run stopped on the call.
    #[must_use]
    pub const fn turn_id(&self) -> &'a str {
        self.turn_id
    }

    /// When the client is asked, in milliseconds since the Unix epoch.
    #[must_use]
    pub const fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }

    /// The call and the state its tool keeps with the approval.
    #[must_use]
    pub const fn approval(&self) -> &'a ToolApproval {
        self.approval
    }
}

/// Previews a policy update and describes its effective wire snapshot.
///
/// Preparation has no side effects. The returned immutable configuration is installed only when
/// input is admitted, preserving Codex's rejection and frozen-step semantics. The host explicitly
/// supplies this adapter; provider-neutral threads need no coding policy configuration.
/// The wire snapshot remains an open record because framework hosts do not own coding fields
/// such as cwd or permission-profile provenance. A coding adapter contributes those fields; the
/// runtime adds model settings. Thread creation and resumption install the appropriate adapter.
///
/// The adapter also asks the client about the calls a run stops on for approval, and applies what
/// an answer asks of the product beyond the call, such as remembering it for the session; the
/// approval itself is settled on the run.
#[async_trait::async_trait]
pub trait ThreadSettingsAdapter: Send + Sync + 'static {
    /// Validates and lowers a candidate without mutating the current configuration.
    fn prepare(
        &self,
        current: &RunConfig,
        update: &ThreadPolicyOverrides,
    ) -> Result<RunConfig, String>;

    /// The server's entry to [`Self::prepare`]. A product whose settings are resolved from its
    /// configuration on disk, as Codex reloads its configuration to select a named permission
    /// profile, overrides this to do that work; the default prepares synchronously.
    async fn prepare_async(
        &self,
        current: &RunConfig,
        update: &ThreadPolicyOverrides,
    ) -> Result<RunConfig, String> {
        self.prepare(current, update)
    }

    /// Returns the product-owned fields of the effective thread settings.
    fn describe(&self, config: &RunConfig) -> Value;

    /// Observes the listener's ordered thread events to release product-owned pending execution
    /// state after a call settles or a turn ends. No provider-neutral policy is implied.
    fn on_thread_event(&self, config: &RunConfig, event: &ThreadEvent) {
        let _ = (config, event);
    }

    /// Hands the product of a thread just loaded with `config` the way to ask its client: Codex's
    /// session reaching its app server. The product keeps it for requests of its own, such as
    /// `item/permissions/requestApproval`. The default keeps nothing.
    fn attach_server_requests(&self, _config: &RunConfig, _requests: ThreadServerRequests) {}

    /// The request asking the client to approve `pending`, made with the configuration of the
    /// turn whose run stopped on it: `item/commandExecution/requestApproval` or
    /// `item/fileChange/requestApproval`. `None` if the product asks nothing for this call, which
    /// is then refused as Codex refuses a call whose approval could not be obtained.
    fn approval_request(
        &self,
        config: &RunConfig,
        pending: PendingApproval<'_>,
    ) -> Option<ServerRequestPayload> {
        let _ = (config, pending);
        None
    }

    /// Whether [`Self::approval_request`] asks the client about the calls of `tool`, a tool whose
    /// every call needs approval
    /// ([`ToolApprovalPolicy::Always`](ra_core::tool::ToolApprovalPolicy::Always)).
    ///
    /// A thread with such a statically declared tool is refused when loaded if its effective
    /// permissions require approval and the product cannot ask for it. Capabilities contribute
    /// tools only after run binding; their approvals use the normal per-call request path. A tool
    /// that decides per call whether it needs approval is not checked here; a call that stops
    /// without a request is refused then. The default asks about no tool.
    fn requests_approval_for(&self, tool: &dyn Tool) -> bool {
        let _ = tool;
        false
    }

    /// Applies what `decision`, the client's answer about `approval`, asks of the product beyond
    /// the call, before the run is answered: what Codex's session does with an approval for the
    /// session or with a command-prefix rule. Returns a warning for the user if part of it could
    /// not be applied, which is sent as Codex sends its `warning`; the answer stands either way.
    async fn apply_review_decision(
        &self,
        config: &RunConfig,
        approval: &ToolApproval,
        decision: &ReviewDecision,
    ) -> Option<String> {
        let _ = (config, approval, decision);
        None
    }
}

pub(crate) fn settings_snapshot(
    thread: &AgentThread,
    adapter: Option<&dyn ThreadSettingsAdapter>,
) -> Value {
    let config = thread.run_config();
    let mut snapshot = adapter.map_or_else(
        || serde_json::json!({}),
        |adapter| adapter.describe(&config),
    );
    if let Some(object) = snapshot.as_object_mut() {
        let resolved = thread.resolve_model(config.model()).ok();
        object.insert(
            "model".into(),
            resolved
                .as_ref()
                .and_then(|model| model.selector().model())
                .map_or(Value::Null, Into::into),
        );
        object.insert(
            "modelProvider".into(),
            resolved.as_ref().map_or(Value::Null, |model| {
                model.selector().provider().as_str().into()
            }),
        );
        let extras = resolved.as_ref().and_then(|resolved| {
            config
                .model_settings()
                .extra_body()
                .get(resolved.selector().provider())
        });
        object.insert(
            "serviceTier".into(),
            extras
                .and_then(|body| body.get("service_tier"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        object.insert(
            "summary".into(),
            extras
                .and_then(|body| body.get("reasoning"))
                .and_then(|body| body.get("summary"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        if object.contains_key("collaborationMode") {
            object.insert("collaborationMode".into(), serde_json::json!({"mode": "default", "settings": {
                "model": object.get("model").cloned().unwrap_or(Value::Null),
                "reasoning_effort": config.model_settings().effort().map(ra_core::model::Effort::label),
                "developer_instructions": null
            }}));
        }
        object.insert(
            "effort".into(),
            config
                .model_settings()
                .effort()
                .map_or(Value::Null, |effort| effort.label().into()),
        );
    }
    snapshot
}
