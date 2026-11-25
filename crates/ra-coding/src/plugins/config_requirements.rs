// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The `configRequirements/read` answer: Codex's `map_requirements_to_api`.
//!
//! This product excludes the allowed login methods, the CLI credential store and the `ChatGPT`
//! backend URL from the response. The backend URL can be present in the composed configuration,
//! so this RPC also excludes it when checking whether the requirement set is empty.

use ra_config::{
    AllowDenyRequirementToml, BrowserUseAccessApprovalLifetimeToml, BrowserUseOriginPolicyToml,
    BrowserUseRequirementsToml, ComputerUseRequirementsToml, ConfigRequirementsToml,
    HookHandlerConfig, ManagedHooksRequirementsToml, MatcherGroup, NetworkDomainPermissionToml,
    NetworkRequirementsToml, NetworkUnixSocketPermissionToml, SandboxModeRequirement,
    WindowsSandboxImplementationToml,
};
use ra_protocol::server::protocol::{
    ConfigRequirements, ConfigRequirementsReadResponse, config_requirements as api,
};

/// Codex's `config_requirements_read` answer for the composed `requirements`.
pub(super) fn requirements_read_response(
    requirements: Option<ConfigRequirementsToml>,
) -> ConfigRequirementsReadResponse {
    ConfigRequirementsReadResponse {
        supports_independent_speed_modes: Some(true),
        requirements: requirements.and_then(|mut requirements| {
            requirements.chatgpt_base_url = None;
            (!requirements.is_empty()).then(|| requirements_to_api(requirements))
        }),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "One field per Codex requirement, as its `map_requirements_to_api` has."
)]
fn requirements_to_api(requirements: ConfigRequirementsToml) -> ConfigRequirements {
    ConfigRequirements {
        model_provider: requirements.model_provider,
        model_providers: requirements.model_providers.map(|providers| {
            providers
                .into_iter()
                .map(|(id, provider)| (id, serde_json::json!(provider)))
                .collect()
        }),
        additional_developer_instructions: requirements.additional_developer_instructions,
        allowed_approval_policies: requirements.allowed_approval_policies,
        allowed_approvals_reviewers: requirements.allowed_approvals_reviewers,
        allowed_sandbox_modes: requirements.allowed_sandbox_modes.map(|modes| {
            modes
                .into_iter()
                .filter_map(sandbox_mode_requirement_to_api)
                .collect()
        }),
        allowed_windows_sandbox_implementations: requirements.windows.and_then(|windows| {
            windows
                .allowed_sandbox_implementations
                .map(|implementations| {
                    implementations
                        .into_iter()
                        .map(|implementation| match implementation {
                            WindowsSandboxImplementationToml::Elevated => {
                                api::WindowsSandboxImplementation::Elevated
                            }
                            WindowsSandboxImplementationToml::Unelevated => {
                                api::WindowsSandboxImplementation::Unelevated
                            }
                        })
                        .collect()
                })
        }),
        allowed_permission_profiles: requirements.allowed_permission_profiles,
        default_permissions: requirements.default_permissions,
        allowed_web_search_modes: requirements.allowed_web_search_modes.map(|modes| {
            let mut normalized = modes
                .into_iter()
                .map(Into::into)
                .collect::<Vec<ra_config::WebSearchMode>>();
            if !normalized.contains(&ra_config::WebSearchMode::Disabled) {
                normalized.push(ra_config::WebSearchMode::Disabled);
            }
            normalized
        }),
        allow_managed_hooks_only: requirements.allow_managed_hooks_only,
        allow_browser_and_computer_use: requirements.allow_browser_and_computer_use,
        allow_appshots: requirements.allow_appshots,
        allow_remote_control: requirements.allow_remote_control,
        computer_use: requirements
            .computer_use
            .map(computer_use_requirements_to_api),
        browser_use: requirements
            .browser_use
            .map(browser_use_requirements_to_api),
        in_app_browser: requirements.in_app_browser.map(|in_app_browser| {
            api::InAppBrowserRequirements {
                allow_external_browser_settings_import: in_app_browser
                    .allow_external_browser_settings_import,
            }
        }),
        feature_requirements: requirements
            .feature_requirements
            .map(|requirements| requirements.entries),
        hooks: requirements.hooks.map(hooks_requirements_to_api),
        enforce_residency: requirements
            .enforce_residency
            .map(|residency| match residency {
                ra_config::ResidencyRequirement::Us => api::ResidencyRequirement::Us,
            }),
        network: requirements.network.map(network_requirements_to_api),
        application: requirements
            .application
            .map(|application| api::ApplicationRequirements {
                network: application
                    .network
                    .map(|network| api::ApplicationNetworkRequirements {
                        enabled: network.enabled,
                        domains: network
                            .domains
                            .into_iter()
                            .map(|(domain, permission)| {
                                (domain, network_domain_permission_to_api(permission))
                            })
                            .collect(),
                    }),
            }),
        auto_review: requirements
            .auto_review
            .map(|auto_review| api::AutoReviewRequirements {
                required_on_models: auto_review.required_on_models,
                ignore_rules: auto_review.ignore_rules,
            }),
        models: requirements.models.map(|models| api::ModelsRequirements {
            new_thread: models
                .new_thread
                .map(|new_thread| api::NewThreadModelDefaults {
                    model: new_thread.model,
                    model_reasoning_effort: new_thread.model_reasoning_effort,
                    service_tier: new_thread.service_tier,
                }),
        }),
        sqlite_home: requirements.sqlite_home.map(Into::into),
        log_dir: requirements.log_dir.map(Into::into),
        model_catalog_json: requirements.model_catalog_json.map(Into::into),
        check_for_update_on_startup: requirements.check_for_update_on_startup,
        allow_login_shell: requirements.allow_login_shell,
        feedback: requirements
            .feedback
            .map(|feedback| api::FeedbackRequirements {
                enabled: feedback.enabled,
            }),
    }
}

fn computer_use_requirements_to_api(
    computer_use: ComputerUseRequirementsToml,
) -> api::ComputerUseRequirements {
    api::ComputerUseRequirements {
        allow_locked_computer_use: computer_use.allow_locked_computer_use,
        allow_persistent_approval: computer_use.allow_persistent_approval,
        default_app_access: computer_use
            .default_app_access
            .map(allow_deny_requirement_to_api),
        macos: computer_use
            .macos
            .map(|macos| api::ComputerUseMacosRequirements {
                bundle_ids: macos.bundle_ids.map(|bundle_ids| {
                    bundle_ids
                        .into_iter()
                        .map(|(bundle_id, requirement)| {
                            (bundle_id, allow_deny_requirement_to_api(requirement))
                        })
                        .collect()
                }),
            }),
        windows: computer_use
            .windows
            .map(|windows| api::ComputerUseWindowsRequirements {
                aumids: windows.aumids.map(|aumids| {
                    aumids
                        .into_iter()
                        .map(|(aumid, requirement)| {
                            (aumid, allow_deny_requirement_to_api(requirement))
                        })
                        .collect()
                }),
                exes: windows.exes.map(|exes| {
                    exes.into_iter()
                        .map(|exe| api::ComputerUseWindowsExeRequirement {
                            publisher_name: exe.publisher_name,
                            product_name: exe.product_name,
                            binary_name: exe.binary_name,
                            access: allow_deny_requirement_to_api(exe.access),
                        })
                        .collect()
                }),
            }),
    }
}

fn browser_use_requirements_to_api(
    browser_use: BrowserUseRequirementsToml,
) -> api::BrowserUseRequirements {
    api::BrowserUseRequirements {
        extension: browser_use
            .extension
            .map(|extension| api::BrowserUseExtensionRequirements {
                request_headers: extension.request_headers.map(|headers| {
                    headers
                        .into_iter()
                        .map(|header| api::RequestHeader {
                            name: header.name,
                            value: header.value,
                        })
                        .collect()
                }),
            }),
        allow_webmcp: browser_use.allow_webmcp,
        allow_history_access: browser_use.allow_history_access,
        disable_auto_review: browser_use.disable_auto_review,
        allow_global_persistent_approval: browser_use.allow_global_persistent_approval,
        default_origin_policy: browser_use
            .default_origin_policy
            .as_ref()
            .map(browser_use_origin_policy_to_api),
        origins: browser_use.origins.map(|origins| {
            origins
                .into_iter()
                .map(|(pattern, policy)| (pattern, browser_use_origin_policy_to_api(&policy)))
                .collect()
        }),
    }
}

fn browser_use_origin_policy_to_api(
    policy: &BrowserUseOriginPolicyToml,
) -> api::BrowserUseOriginPolicy {
    api::BrowserUseOriginPolicy {
        access: policy.access.map(allow_deny_requirement_to_api),
        downloads: policy.downloads.map(allow_deny_requirement_to_api),
        uploads: policy.uploads.map(allow_deny_requirement_to_api),
        full_cdp_access: policy.full_cdp_access.map(allow_deny_requirement_to_api),
        auto_review: policy.auto_review.map(allow_deny_requirement_to_api),
        persistent_approval: policy.persistent_approval,
        access_approval_lifetime: policy
            .access_approval_lifetime
            .map(|lifetime| match lifetime {
                BrowserUseAccessApprovalLifetimeToml::Turn => {
                    api::BrowserUseAccessApprovalLifetime::Turn
                }
                BrowserUseAccessApprovalLifetimeToml::Thread => {
                    api::BrowserUseAccessApprovalLifetime::Thread
                }
            }),
    }
}

fn allow_deny_requirement_to_api(
    requirement: AllowDenyRequirementToml,
) -> api::AllowDenyRequirement {
    match requirement {
        AllowDenyRequirementToml::Allow => api::AllowDenyRequirement::Allow,
        AllowDenyRequirementToml::Deny => api::AllowDenyRequirement::Deny,
    }
}

fn hooks_requirements_to_api(hooks: ManagedHooksRequirementsToml) -> api::ManagedHooksRequirements {
    let ManagedHooksRequirementsToml {
        managed_dir,
        windows_managed_dir,
        hooks,
    } = hooks;
    let ra_config::HookEventsToml {
        pre_tool_use,
        permission_request,
        post_tool_use,
        pre_compact,
        post_compact,
        session_start,
        session_end,
        user_prompt_submit,
        subagent_start,
        subagent_stop,
        stop,
        interrupt,
    } = hooks;
    api::ManagedHooksRequirements {
        managed_dir,
        windows_managed_dir,
        pre_tool_use: hook_matcher_groups_to_api(pre_tool_use),
        permission_request: hook_matcher_groups_to_api(permission_request),
        post_tool_use: hook_matcher_groups_to_api(post_tool_use),
        pre_compact: hook_matcher_groups_to_api(pre_compact),
        post_compact: hook_matcher_groups_to_api(post_compact),
        session_start: hook_matcher_groups_to_api(session_start),
        session_end: hook_matcher_groups_to_api(session_end),
        user_prompt_submit: hook_matcher_groups_to_api(user_prompt_submit),
        subagent_start: hook_matcher_groups_to_api(subagent_start),
        subagent_stop: hook_matcher_groups_to_api(subagent_stop),
        stop: hook_matcher_groups_to_api(stop),
        interrupt: hook_matcher_groups_to_api(interrupt),
    }
}

fn hook_matcher_groups_to_api(groups: Vec<MatcherGroup>) -> Vec<api::ConfiguredHookMatcherGroup> {
    groups
        .into_iter()
        .map(|group| api::ConfiguredHookMatcherGroup {
            matcher: group.matcher,
            hooks: group.hooks.into_iter().map(hook_handler_to_api).collect(),
        })
        .collect()
}

fn hook_handler_to_api(handler: HookHandlerConfig) -> api::ConfiguredHookHandler {
    match handler {
        HookHandlerConfig::Command {
            command,
            command_windows,
            timeout_sec,
            r#async,
            status_message,
            additional_context_limit,
        } => api::ConfiguredHookHandler::Command {
            command,
            command_windows,
            timeout_sec,
            r#async,
            status_message,
            additional_context_limit,
        },
        HookHandlerConfig::McpTool {
            server,
            tool,
            input,
            timeout_sec,
            status_message,
        } => api::ConfiguredHookHandler::McpTool {
            server,
            tool,
            input,
            timeout_sec,
            status_message,
        },
        HookHandlerConfig::Prompt {} => api::ConfiguredHookHandler::Prompt {},
        HookHandlerConfig::Agent {} => api::ConfiguredHookHandler::Agent {},
    }
}

fn sandbox_mode_requirement_to_api(mode: SandboxModeRequirement) -> Option<ra_config::SandboxMode> {
    match mode {
        SandboxModeRequirement::ReadOnly => Some(ra_config::SandboxMode::ReadOnly),
        SandboxModeRequirement::WorkspaceWrite => Some(ra_config::SandboxMode::WorkspaceWrite),
        SandboxModeRequirement::DangerFullAccess => Some(ra_config::SandboxMode::DangerFullAccess),
        SandboxModeRequirement::ExternalSandbox => None,
    }
}

fn network_requirements_to_api(network: NetworkRequirementsToml) -> api::NetworkRequirements {
    let allowed_domains = network
        .domains
        .as_ref()
        .and_then(ra_config::NetworkDomainPermissionsToml::allowed_domains);
    let denied_domains = network
        .domains
        .as_ref()
        .and_then(ra_config::NetworkDomainPermissionsToml::denied_domains);
    let allow_unix_sockets = network
        .unix_sockets
        .as_ref()
        .map(ra_config::NetworkUnixSocketPermissionsToml::allow_unix_sockets)
        .filter(|entries| !entries.is_empty());

    api::NetworkRequirements {
        enabled: network.enabled,
        http_port: network.http_port,
        socks_port: network.socks_port,
        allow_upstream_proxy: network.allow_upstream_proxy,
        dangerously_allow_non_loopback_proxy: network.dangerously_allow_non_loopback_proxy,
        dangerously_allow_all_unix_sockets: network.dangerously_allow_all_unix_sockets,
        domains: network.domains.map(|domains| {
            domains
                .entries
                .into_iter()
                .map(|(pattern, permission)| {
                    (pattern, network_domain_permission_to_api(permission))
                })
                .collect()
        }),
        managed_allowed_domains_only: network.managed_allowed_domains_only,
        allowed_domains,
        denied_domains,
        unix_sockets: network.unix_sockets.map(|unix_sockets| {
            unix_sockets
                .entries
                .into_iter()
                .map(|(path, permission)| {
                    (
                        path,
                        match permission {
                            NetworkUnixSocketPermissionToml::Allow => {
                                api::NetworkUnixSocketPermission::Allow
                            }
                            NetworkUnixSocketPermissionToml::Deny => {
                                api::NetworkUnixSocketPermission::Deny
                            }
                        },
                    )
                })
                .collect()
        }),
        allow_unix_sockets,
        allow_local_binding: network.allow_local_binding,
    }
}

fn network_domain_permission_to_api(
    permission: NetworkDomainPermissionToml,
) -> api::NetworkDomainPermission {
    match permission {
        NetworkDomainPermissionToml::Allow => api::NetworkDomainPermission::Allow,
        NetworkDomainPermissionToml::Deny => api::NetworkDomainPermission::Deny,
    }
}
