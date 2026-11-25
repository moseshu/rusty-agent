// Adapted from openai/codex (Apache-2.0) @ e58bf8f5233b51e7cd1679d947e3028c1ed91b06, modified.
//! App-server feature catalog and runtime defaults.
use super::{PluginServices, protocol::internal};
use ra_config::{
    ConfigLayerStack, ManagedFeatures,
    features::{FEATURES, Stage, canonical_feature_for_key},
};
use ra_protocol::{
    frame::JsonRpcErrorObject,
    server::protocol::{
        ExperimentalFeature, ExperimentalFeatureEnablementSetParams,
        ExperimentalFeatureEnablementSetResponse, ExperimentalFeatureListParams,
        ExperimentalFeatureListResponse, ExperimentalFeatureStage,
    },
};
use std::collections::BTreeMap;

/// The runtime RPC accepts only canonical keys from this list that exist in the local registry.
pub(super) const SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT: &[&str] = &[
    "api_key_cyber_access_programs",
    "api_key_model_discovery",
    "auth_elicitation",
    "background_paginated_rollout_migration",
    "codex_apps_mcp_2026_07_28",
    "daybreak_program_propagation",
    "mcp_2026_07_28",
    "mcp_connection_sharing",
    "mcp_lazy_stdio_startup",
    "memories",
    "mentions_v2",
    "remote_control",
    "remote_plugin",
    "tool_suggest",
    "windows_sandbox_service",
];

fn filter_enablement(enablement: &mut BTreeMap<String, bool>) {
    let mut invalid_keys = Vec::new();
    enablement.retain(|key, _| {
        let valid = canonical_feature_for_key(key).is_some()
            && SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT.contains(&key.as_str());
        if !valid {
            invalid_keys.push(key.clone());
        }
        valid
    });
    if !invalid_keys.is_empty() {
        let invalid_keys = invalid_keys.join(", ");
        tracing::warn!("ignoring invalid experimental feature enablement keys: {invalid_keys}");
    }
}

pub(super) fn apply_runtime_feature_enablement(
    features: &mut ManagedFeatures,
    layers: &ConfigLayerStack,
    enablement: &BTreeMap<String, bool>,
) {
    let effective = layers.effective_config();
    let configured = effective.get("features").and_then(toml::Value::as_table);
    let required = layers.requirements_toml().feature_requirements.as_ref();
    for (key, enabled) in enablement {
        if configured.is_some_and(|features| features.contains_key(key))
            || required.is_some_and(|requirements| requirements.entries.contains_key(key))
        {
            continue;
        }
        if let Some(feature) = canonical_feature_for_key(key)
            && let Err(error) = features.set_enabled(feature, *enabled)
        {
            tracing::warn!(feature = key, %error, "failed to apply runtime feature enablement");
        }
    }
}

impl PluginServices {
    pub(super) async fn experimental_feature_list(
        &self,
        params: ExperimentalFeatureListParams,
    ) -> Result<ExperimentalFeatureListResponse, JsonRpcErrorObject> {
        let ExperimentalFeatureListParams {
            cursor,
            limit,
            thread_id,
        } = params;
        let config = if let Some(thread_id) = thread_id {
            let refresh = self.loaded_hook_refresh(&thread_id).ok_or_else(|| {
                JsonRpcErrorObject::invalid_request(format!("thread not found: {thread_id}"))
            })?;
            let (cwd, session_layers) = refresh.feature_config_context().await;
            self.config_manager
                .load_with_session_layers(cwd, &session_layers)
                .await
                .map_err(|err| internal(format!("failed to reload config: {err}")))?
        } else {
            self.config_manager
                .load_latest_config()
                .await
                .map_err(|err| feature_config_error(&err))?
        };

        let total = FEATURES.len();
        let start = cursor.as_deref().map_or(Ok(0), |cursor| {
            cursor.parse::<usize>().map_err(|_| {
                JsonRpcErrorObject::invalid_request(format!("invalid cursor: {cursor}"))
            })
        })?;
        if start > total {
            return Err(JsonRpcErrorObject::invalid_request(format!(
                "cursor {start} exceeds total feature flags {total}"
            )));
        }
        // Clamp to one so a zero limit still advances the cursor.
        let limit = limit.map_or(total, |limit| (limit as usize).max(1));
        let end = start.saturating_add(limit).min(total);
        let data = FEATURES[start..end]
            .iter()
            .map(|spec| {
                let (stage, announcement) = match spec.stage {
                    Stage::Experimental { announcement, .. } => (
                        ExperimentalFeatureStage::Beta,
                        Some(announcement.to_owned()),
                    ),
                    Stage::UnderDevelopment => (ExperimentalFeatureStage::UnderDevelopment, None),
                    Stage::Stable => (ExperimentalFeatureStage::Stable, None),
                    Stage::Deprecated => (ExperimentalFeatureStage::Deprecated, None),
                    Stage::Removed => (ExperimentalFeatureStage::Removed, None),
                };
                ExperimentalFeature {
                    name: spec.key.to_owned(),
                    stage,
                    display_name: spec.stage.experimental_menu_name().map(str::to_owned),
                    description: spec
                        .stage
                        .experimental_menu_description()
                        .map(str::to_owned),
                    announcement,
                    enabled: config.features.enabled(spec.id),
                    default_enabled: spec.default_enabled,
                }
            })
            .collect();
        Ok(ExperimentalFeatureListResponse {
            data,
            next_cursor: (end < total).then(|| end.to_string()),
        })
    }

    pub(super) async fn experimental_feature_enablement_set(
        &self,
        params: ExperimentalFeatureEnablementSetParams,
    ) -> Result<ExperimentalFeatureEnablementSetResponse, JsonRpcErrorObject> {
        let ExperimentalFeatureEnablementSetParams { mut enablement } = params;
        filter_enablement(&mut enablement);
        self.update_runtime_feature_enablement(&enablement).await?;
        Ok(ExperimentalFeatureEnablementSetResponse { enablement })
    }
}

pub(super) fn feature_config_error(err: &std::io::Error) -> JsonRpcErrorObject {
    internal(format!(
        "failed to resolve feature override precedence: {err}"
    ))
}
