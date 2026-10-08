//! The host boundary for Codex's product-owned settings. The protocol ports their wire shape;
//! the selected product previews and applies their concrete execution mechanism.

use ra_runtime::{runner::RunConfig, thread::AgentThread};
use serde_json::Value;

use super::protocol::ThreadPolicyOverrides;

/// Previews a policy update and describes its effective wire snapshot.
///
/// Preparation has no side effects. The returned immutable configuration is installed only when
/// input is admitted, preserving Codex's rejection and frozen-step semantics. The host explicitly
/// supplies this adapter; provider-neutral threads need no coding policy configuration.
/// The wire snapshot remains an open record because framework hosts do not own coding fields
/// such as cwd or permission-profile provenance. A coding adapter contributes those fields; the
/// runtime adds model settings. Thread creation and resumption install the appropriate adapter.
pub trait ThreadSettingsAdapter: Send + Sync + 'static {
    /// Validates and lowers a candidate without mutating the current configuration.
    fn prepare(
        &self,
        current: &RunConfig,
        update: &ThreadPolicyOverrides,
    ) -> Result<RunConfig, String>;

    /// Returns the product-owned fields of the effective thread settings.
    fn describe(&self, config: &RunConfig) -> Value;
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
