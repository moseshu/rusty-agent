//! Management wire behavior checked against Codex 815ef9bb declarations and config RPC tests.

use ra_protocol::server::protocol::*;
use serde_json::json;

#[test]
fn omitted_flags_use_source_defaults_and_are_not_written_back() {
    let read: ConfigReadParams = serde_json::from_value(json!({})).unwrap();
    assert!(!read.include_layers);
    assert_eq!(serde_json::to_value(read).unwrap(), json!({"cwd": null}));
    assert!(serde_json::from_value::<ConfigReadParams>(json!({"includeLayers": null})).is_err());

    let batch: ConfigBatchWriteParams = serde_json::from_value(json!({"edits": []})).unwrap();
    assert!(!batch.reload_user_config);
    assert_eq!(
        serde_json::to_value(batch).unwrap(),
        json!({"edits": [], "filePath": null, "expectedVersion": null})
    );
    let skills: SkillsListParams = serde_json::from_value(json!({})).unwrap();
    assert_eq!(serde_json::to_value(skills).unwrap(), json!({}));
    let hooks: HooksListParams = serde_json::from_value(json!({})).unwrap();
    assert_eq!(serde_json::to_value(hooks).unwrap(), json!({}));
}

#[test]
fn write_parameters_keep_null_values_required_fields_and_camel_case() {
    let value =
        json!({"keyPath": "plugins.demo.enabled", "value": null, "mergeStrategy": "upsert"});
    let params: ConfigValueWriteParams = serde_json::from_value(value.clone()).unwrap();
    assert!(params.value.is_null());
    let mut expected = value;
    expected["filePath"] = json!(null);
    expected["expectedVersion"] = json!(null);
    assert_eq!(serde_json::to_value(params).unwrap(), expected);
    assert!(serde_json::from_value::<ConfigValueWriteParams>(json!({"value": true})).is_err());
    assert!(
        serde_json::from_value::<ConfigValueWriteParams>(json!({
            "keyPath": "x", "value": true, "mergeStrategy": null
        }))
        .is_err()
    );
}

#[test]
fn config_and_analytics_preserve_unknown_keys_in_additional() {
    let input = json!({
        "model": "example-model",
        "future_option": {"enabled": true},
        "analytics": {"enabled": false, "future_analytics_option": 7}
    });
    let config: Config = serde_json::from_value(input).unwrap();
    assert_eq!(config.additional["future_option"], json!({"enabled": true}));
    assert!(!config.additional.contains_key("model"));
    assert!(!config.additional.contains_key("analytics"));
    let analytics = config.analytics.as_ref().unwrap();
    assert_eq!(analytics.additional["future_analytics_option"], json!(7));
    let wire = serde_json::to_value(config).unwrap();
    assert_eq!(wire["model"], json!("example-model"));
    assert_eq!(wire["future_option"], json!({"enabled": true}));
    assert_eq!(wire["analytics"]["future_analytics_option"], json!(7));
    assert!(wire.get("additional").is_none());
    assert!(wire["analytics"].get("additional").is_none());
    for excluded in ["forced_chatgpt_workspace_id", "forced_login_method", "apps"] {
        assert!(wire.get(excluded).is_none());
    }
}

#[test]
fn source_layers_keep_tag_names_and_nullable_profile() {
    let wire = json!({"type": "user", "file": "/tmp/config.toml"});
    let layer: ConfigLayerSource = serde_json::from_value(wire).unwrap();
    assert_eq!(
        serde_json::to_value(layer).unwrap(),
        json!({
            "type": "user", "file": "/tmp/config.toml", "profile": null
        })
    );
    let project: ConfigLayerSource = serde_json::from_value(json!({
        "type": "project", "dotCodexFolder": "/tmp/project/.codex"
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(project).unwrap()["dotCodexFolder"],
        json!("/tmp/project/.codex")
    );
}

#[test]
fn error_codes_retain_source_spelling_and_reject_unknown_values() {
    for code in [
        "configLayerReadonly",
        "configRequirementReadonly",
        "configVersionConflict",
        "configValidationError",
        "configPathNotFound",
        "configSchemaUnknownKey",
        "userLayerNotFound",
    ] {
        let parsed: ConfigWriteErrorCode = serde_json::from_value(json!(code)).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), json!(code));
    }
    assert!(serde_json::from_value::<ConfigWriteErrorCode>(json!("futureError")).is_err());
    assert!(serde_json::from_value::<MergeStrategy>(json!("futureMerge")).is_err());
    assert!(serde_json::from_value::<HookSource>(json!("future_source")).is_err());
    assert!(serde_json::from_value::<PluginSource>(json!({"type": "future"})).is_err());
}

#[test]
fn hook_handler_flatten_preserves_the_source_default_for_async() {
    let wire = json!({"handlerType": "command", "command": "echo ok"});
    let handler: HookHandlerMetadata = serde_json::from_value(wire).unwrap();
    assert_eq!(
        serde_json::to_value(handler).unwrap(),
        json!({
            "handlerType": "command", "command": "echo ok", "async": false
        })
    );
    assert!(
        serde_json::from_value::<HookHandlerMetadata>(json!({
            "handlerType": "command", "command": "echo ok", "async": null
        }))
        .is_err()
    );
}

#[test]
fn hook_lifecycle_uses_core_snake_case_and_skips_builtin() {
    let mut wire = json!({
        "id": "hook-1", "event_name": "pre_tool_use", "handler_type": "command",
        "execution_mode": "sync", "scope": "turn", "source_path": "/tmp/hooks.json",
        "display_order": 0, "status": "completed", "started_at": 1, "entries": []
    });
    let summary: HookRunSummary = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(summary.source, HookSource::Unknown);
    assert!(!summary.builtin);
    wire["source"] = json!("unknown");
    wire["status_message"] = json!(null);
    wire["completed_at"] = json!(null);
    wire["duration_ms"] = json!(null);
    assert_eq!(serde_json::to_value(summary).unwrap(), wire);
    let summary: HookRunSummary = serde_json::from_value(wire.clone()).unwrap();
    let summary = HookRunSummary {
        builtin: true,
        ..summary
    };
    assert_eq!(serde_json::to_value(summary).unwrap(), wire);
}

#[test]
fn plugin_sources_and_list_defaults_keep_non_account_contract() {
    for source in [
        json!({"type": "local", "path": "/tmp/plugin"}),
        json!({"type": "git", "url": "https://example.com/plugin.git", "path": null, "refName": null, "sha": null}),
        json!({"type": "npm", "package": "example-plugin", "version": null, "registry": null}),
    ] {
        let parsed: PluginSource = serde_json::from_value(source.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), source);
    }
    let params: PluginListParams = serde_json::from_value(json!({})).unwrap();
    assert!(!params.force_refetch);
    assert_eq!(
        serde_json::to_value(params).unwrap(),
        json!({"cwds": null, "marketplaceKinds": null})
    );
    let response: PluginListResponse = serde_json::from_value(json!({"marketplaces": []})).unwrap();
    assert!(response.marketplace_load_errors.is_empty());
    assert!(serde_json::from_value::<PluginListParams>(json!({"forceRefetch": null})).is_err());
}

#[test]
fn policy_reexports_remain_usable_in_downstream_traits_and_signatures() {
    trait LocalPolicy {
        fn is_never(&self) -> bool;
    }
    impl LocalPolicy for AskForApproval {
        fn is_never(&self) -> bool {
            matches!(self, Self::Never)
        }
    }
    fn roundtrip(policy: AskForApproval) -> AskForApproval {
        policy
    }
    assert!(roundtrip(AskForApproval::Never).is_never());
    assert_eq!(
        SandboxMode::ReadOnly.to_policy(),
        SandboxPolicy::ReadOnly {
            network_access: false
        }
    );
    let reviewer: ApprovalsReviewer = serde_json::from_value(json!("guardian_subagent")).unwrap();
    assert_eq!(
        serde_json::to_value(reviewer).unwrap(),
        json!("auto_review")
    );
}

#[test]
fn hook_metadata_flattens_handler_members_without_an_extra_object() {
    let wire = json!({
        "key": "pre-tool", "eventName": "pre_tool_use", "handlerType": "command",
        "command": "echo ok", "matcher": null, "timeoutSec": 30, "statusMessage": null,
        "additionalContextLimit": null, "sourcePath": "/tmp/hooks.json", "source": "user",
        "pluginId": null, "displayOrder": 0, "enabled": true, "isManaged": false,
        "currentHash": "abc", "trustStatus": "trusted"
    });
    let metadata: HookMetadata = serde_json::from_value(wire.clone()).unwrap();
    let mut expected = wire;
    expected["async"] = json!(false);
    assert_eq!(serde_json::to_value(metadata).unwrap(), expected);
}

#[test]
fn retained_wire_enums_reject_unknown_values_without_fallbacks() {
    macro_rules! rejects_unknown {
        ($($ty:ty),+ $(,)?) => {$(
            assert!(serde_json::from_value::<$ty>(json!("future_value")).is_err(), stringify!($ty));
        )+};
    }
    rejects_unknown!(
        AllowDenyRequirement,
        MergeStrategy,
        WriteStatus,
        ConfigWriteErrorCode,
        PluginListMarketplaceKind,
        SkillScope,
        HookHandlerType,
        HookExecutionMode,
        HookScope,
        HookSource,
        HookRunStatus,
        HookOutputEntryKind,
        HookTrustStatus,
    );
    assert!(serde_json::from_value::<ConfigLayerSource>(json!({"type": "future"})).is_err());
    assert!(
        serde_json::from_value::<HookHandlerMetadata>(json!({"handlerType": "future"})).is_err()
    );
}

#[test]
fn restored_plugin_metadata_fields_use_source_names_and_defaults() {
    let list: PluginListResponse = serde_json::from_value(json!({"marketplaces": []})).unwrap();
    assert_eq!(
        serde_json::to_value(list).unwrap()["featuredPluginIds"],
        json!([])
    );
    let detail: PluginDetail = serde_json::from_value(json!({
        "marketplaceName": "company", "summary": {
            "id": "sample@company", "name": "sample", "source": {"type": "local", "path": "/tmp/sample"},
            "installed": false, "enabled": true, "installPolicy": "AVAILABLE", "authPolicy": "ON_USE"
        }, "skills": [], "hooks": [], "mcpServers": []
    })).unwrap();
    let wire = serde_json::to_value(detail).unwrap();
    assert_eq!(wire.get("scheduledTasks"), Some(&json!(null)));
    assert_eq!(
        wire["summary"].get("installPolicySource"),
        Some(&json!(null))
    );
    assert_eq!(
        wire["summary"].get("mustShowInstallationInterstitial"),
        Some(&json!(null))
    );
    for policy in ["WORKSPACE_SETTING", "IMPLICIT_CANONICAL_APP"] {
        let parsed: PluginInstallPolicySource = serde_json::from_value(json!(policy)).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), json!(policy));
    }
    let task = json!({"key": "triage", "name": "Queue triage", "prompt": "Check queue", "schedule": {
        "type": "hourly", "intervalHours": 2, "days": ["MO", "WE", "FR"]
    }});
    let parsed: ScheduledTaskSummary = serde_json::from_value(task.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), task);
    assert!(serde_json::from_value::<ScheduledTaskSchedule>(json!({"type": "future"})).is_err());
    assert!(serde_json::from_value::<ScheduledTaskWeekday>(json!("MONDAY")).is_err());
}
