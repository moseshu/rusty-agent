// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    unreachable_pub,
    reason = "Discovery results are exported only by the explicit external-test facade."
)]
#![allow(
    missing_docs,
    reason = "Preserve the source hook discovery and execution contract."
)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::plugins::PluginHookSource;
use crate::plugins::is_allowlisted_bundled_cleanup_hook;
use ra_config::CONFIG_TOML_FILE;
use ra_config::ConfigLayerEntry;
use ra_config::ConfigLayerSource;
use ra_config::ConfigLayerStack;
use ra_config::HookEventsToml;
use ra_config::HookHandlerConfig;
use ra_config::HookStateToml;
use ra_config::HooksFile;
use ra_config::ManagedHooksRequirementsToml;
use ra_config::MatcherGroup;
use ra_config::RequirementSource;
use ra_config::TomlValue;
use ra_core::hook::HookEventName;
use ra_paths::AbsolutePathBuf;
use serde::Deserialize;

use super::ConfiguredHandler;
use super::ConfiguredHandlerKind;
use super::HookListEntry;
use super::HookListEntryHandler;
use super::dispatcher::hook_event_name_label;
use crate::hooks::config_rules::hook_states_from_stack;
use crate::hooks::engine::HookMatcher;
use crate::hooks::events::common::matcher_pattern_for_event;
use crate::hooks::output_spill::AdditionalContextLimit;
use crate::hooks::output_spill::DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT;
use ra_protocol::server::protocol::HookSource;
use ra_protocol::server::protocol::HookTrustStatus;

pub struct DiscoveryResult {
    pub handlers: Vec<ConfiguredHandler>,
    pub hook_entries: Vec<HookListEntry>,
    pub warnings: Vec<String>,
    pub required_load_errors: Vec<String>,
}

struct HookHandlerSource<'a> {
    path: &'a AbsolutePathBuf,
    key_source: String,
    source: HookSource,
    is_managed: bool,
    requirement: HookRequirement<'a>,
    bypass_hook_trust: bool,
    hook_states: &'a HashMap<String, HookStateToml>,
    env: HashMap<String, String>,
    plugin_id: Option<String>,
}

enum HookRequirement<'a> {
    Required(&'a mut Vec<String>),
    Optional,
}

impl HookHandlerSource<'_> {
    fn record_load_failure(&mut self, warning: String, warnings: &mut Vec<String>) {
        if let HookRequirement::Required(required_load_errors) = &mut self.requirement {
            required_load_errors.push(warning.clone());
        }
        warnings.push(warning);
    }
}

struct NormalizedHandler {
    config: HookHandlerConfig,
    kind: ConfiguredHandlerKind,
    timeout_sec: u64,
    status_message: Option<String>,
    additional_context_limit: Option<usize>,
}

#[derive(Clone, Copy)]
struct HookDiscoveryPolicy {
    allow_managed_hooks_only: bool,
    bypass_hook_trust: bool,
}

impl HookDiscoveryPolicy {
    fn allows(self, source: &HookHandlerSource<'_>) -> bool {
        !self.allow_managed_hooks_only || source.is_managed
    }
}

#[must_use]
pub fn discover_handlers(
    config_layer_stack: Option<&ConfigLayerStack>,
    plugin_hook_sources: Vec<PluginHookSource>,
    plugin_hook_load_warnings: Vec<String>,
    bypass_hook_trust: bool,
) -> DiscoveryResult {
    let mut handlers = Vec::new();
    let mut hook_entries = Vec::new();
    let mut warnings = plugin_hook_load_warnings;
    let mut required_load_errors = Vec::new();
    let mut display_order = 0_i64;
    let mut visited_json_hook_folders = HashSet::new();
    let hook_states = hook_states_from_stack(config_layer_stack);
    let policy = HookDiscoveryPolicy {
        allow_managed_hooks_only: config_layer_stack.is_some_and(|config_layer_stack| {
            config_layer_stack
                .requirements()
                .allow_managed_hooks_only
                .as_ref()
                .is_some_and(|requirement| requirement.value)
        }),
        bypass_hook_trust,
    };

    if let Some(config_layer_stack) = config_layer_stack {
        required_load_errors = append_managed_requirement_handlers(
            &mut handlers,
            &mut hook_entries,
            &mut warnings,
            &mut display_order,
            config_layer_stack,
            &hook_states,
            policy,
        );

        for layer in config_layer_stack.layers_low_to_high() {
            let (hook_source, is_managed) = hook_metadata_for_config_layer_source(&layer.name);
            let policy_path = config_toml_source_path(layer);
            let policy_source = HookHandlerSource {
                path: &policy_path,
                key_source: policy_path.display().to_string(),
                source: hook_source,
                is_managed,
                requirement: HookRequirement::Optional,
                bypass_hook_trust: false,
                hook_states: &hook_states,
                env: HashMap::new(),
                plugin_id: None,
            };
            if !policy.allows(&policy_source) {
                continue;
            }
            let json_hooks = match layer.hooks_config_folder() {
                Some(config_folder) if visited_json_hook_folders.insert(config_folder.clone()) => {
                    load_hooks_json(Some(config_folder.as_path()), &mut warnings)
                }
                _ => None,
            };
            let toml_hooks = load_toml_hooks_from_layer(layer, &mut warnings);

            if let (Some((json_source_path, json_events)), Some((toml_source_path, toml_events))) =
                (&json_hooks, &toml_hooks)
                && !json_events.is_empty()
                && !toml_events.is_empty()
            {
                warnings.push(format!(
                    "loading hooks from both {} and {}; prefer a single representation for this layer",
                    json_source_path.display(),
                    toml_source_path.display()
                ));
            }

            for (source_path, hook_events) in [json_hooks, toml_hooks].into_iter().flatten() {
                append_hook_events(
                    &mut handlers,
                    &mut hook_entries,
                    &mut warnings,
                    &mut display_order,
                    HookHandlerSource {
                        path: &source_path,
                        key_source: source_path.display().to_string(),
                        source: hook_source,
                        is_managed,
                        requirement: HookRequirement::Optional,
                        bypass_hook_trust: policy.bypass_hook_trust,
                        hook_states: &hook_states,
                        env: HashMap::new(),
                        plugin_id: None,
                    },
                    hook_events,
                    policy,
                );
            }
        }
    }

    append_plugin_hook_sources(
        &mut handlers,
        &mut hook_entries,
        &mut warnings,
        &mut display_order,
        plugin_hook_sources,
        &hook_states,
        policy,
    );

    DiscoveryResult {
        handlers,
        hook_entries,
        warnings,
        required_load_errors,
    }
}

fn append_managed_requirement_handlers(
    handlers: &mut Vec<ConfiguredHandler>,
    hook_entries: &mut Vec<HookListEntry>,
    warnings: &mut Vec<String>,
    display_order: &mut i64,
    config_layer_stack: &ConfigLayerStack,
    hook_states: &HashMap<String, HookStateToml>,
    policy: HookDiscoveryPolicy,
) -> Vec<String> {
    let Some(managed_hooks) = config_layer_stack.requirements().managed_hooks.as_ref() else {
        return Vec::new();
    };
    let mut required_load_errors = Vec::new();
    let source_path = managed_hooks_source_path(managed_hooks.get(), managed_hooks.source.as_ref());
    append_hook_events(
        handlers,
        hook_entries,
        warnings,
        display_order,
        HookHandlerSource {
            path: &source_path,
            key_source: source_path.display().to_string(),
            source: hook_source_for_requirement_source(managed_hooks.source.as_ref()),
            is_managed: true,
            requirement: HookRequirement::Required(&mut required_load_errors),
            bypass_hook_trust: false,
            hook_states,
            env: HashMap::new(),
            plugin_id: None,
        },
        managed_hooks.get().hooks.clone(),
        policy,
    );
    required_load_errors
}

fn append_plugin_hook_sources(
    handlers: &mut Vec<ConfiguredHandler>,
    hook_entries: &mut Vec<HookListEntry>,
    warnings: &mut Vec<String>,
    display_order: &mut i64,
    plugin_hook_sources: Vec<PluginHookSource>,
    hook_states: &HashMap<String, HookStateToml>,
    policy: HookDiscoveryPolicy,
) {
    for source in plugin_hook_sources {
        let PluginHookSource {
            plugin_root,
            plugin_id,
            plugin_data_root,
            source_path,
            source_relative_path,
            hooks,
        } = source;
        let mut env = HashMap::new();
        let plugin_root_value = plugin_root.display().to_string();
        let plugin_data_root_value = plugin_data_root.display().to_string();
        env.insert("PLUGIN_ROOT".to_string(), plugin_root_value.clone());
        // For OOTB compat with existing plugins that use this env var.
        env.insert("CLAUDE_PLUGIN_ROOT".to_string(), plugin_root_value);
        env.insert("PLUGIN_DATA".to_string(), plugin_data_root_value.clone());
        // For OOTB compat with existing plugins that use this env var.
        env.insert("CLAUDE_PLUGIN_DATA".to_string(), plugin_data_root_value);
        let plugin_id = plugin_id.as_key();
        append_hook_events(
            handlers,
            hook_entries,
            warnings,
            display_order,
            HookHandlerSource {
                path: &source_path,
                key_source: crate::hooks::declarations::plugin_hook_key_source(
                    plugin_id.as_str(),
                    source_relative_path.as_str(),
                ),
                source: HookSource::Plugin,
                is_managed: false,
                requirement: HookRequirement::Optional,
                bypass_hook_trust: policy.bypass_hook_trust,
                hook_states,
                env,
                plugin_id: Some(plugin_id),
            },
            hooks,
            policy,
        );
    }
}

fn managed_hooks_source_path(
    managed_hooks: &ManagedHooksRequirementsToml,
    requirement_source: Option<&RequirementSource>,
) -> AbsolutePathBuf {
    if let Some(source_path) = managed_hooks.managed_dir_for_current_platform()
        && source_path.is_absolute()
        && let Ok(source_path) = AbsolutePathBuf::from_absolute_path(source_path)
    {
        return source_path;
    }

    fallback_managed_hooks_source_path(requirement_source)
}

fn fallback_managed_hooks_source_path(
    requirement_source: Option<&RequirementSource>,
) -> AbsolutePathBuf {
    match requirement_source {
        Some(
            RequirementSource::SystemRequirementsToml { file }
            | RequirementSource::LegacyManagedConfigTomlFromFile { file },
        ) => file.clone(),
        Some(RequirementSource::MdmManagedPreferences { domain, key }) => {
            synthetic_layer_path(&format!("<mdm:{domain}:{key}>/requirements.toml"))
        }
        Some(RequirementSource::Composite { .. }) => {
            synthetic_layer_path("<requirements-composition>/requirements.toml")
        }
        Some(RequirementSource::EnterpriseManaged { id, name }) => {
            let name = escape_xml_text(name);
            let id = escape_xml_text(id);
            synthetic_layer_path(&format!(
                "<enterprise-managed:{name}:{id}>/requirements.toml"
            ))
        }
        Some(RequirementSource::LegacyManagedConfigTomlFromMdm) => {
            synthetic_layer_path("<legacy-managed-config.toml-mdm>/managed_config.toml")
        }
        Some(RequirementSource::Unknown) | None => {
            synthetic_layer_path("<managed-requirements>/requirements.toml")
        }
    }
}

fn load_hooks_json(
    config_folder: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Option<(AbsolutePathBuf, HookEventsToml)> {
    let source_path = config_folder?.join("hooks.json");
    if !source_path.as_path().is_file() {
        return None;
    }

    let contents = match fs::read_to_string(source_path.as_path()) {
        Ok(contents) => contents,
        Err(err) => {
            warnings.push(format!(
                "failed to read hooks config {}: {err}",
                source_path.display()
            ));
            return None;
        }
    };

    let parsed: HooksFile = match serde_json::from_str(&contents) {
        Ok(parsed) => parsed,
        Err(err) => {
            warnings.push(format!(
                "failed to parse hooks config {}: {err}",
                source_path.display()
            ));
            return None;
        }
    };

    let source_path = AbsolutePathBuf::from_absolute_path(&source_path)
        .inspect_err(|err| {
            warnings.push(format!(
                "failed to normalize hooks config path {}: {err}",
                source_path.display()
            ));
        })
        .ok()?;

    (!parsed.hooks.is_empty()).then_some((source_path, parsed.hooks))
}

fn load_toml_hooks_from_layer(
    layer: &ConfigLayerEntry,
    warnings: &mut Vec<String>,
) -> Option<(AbsolutePathBuf, HookEventsToml)> {
    let source_path = config_toml_source_path(layer);
    let hook_value = layer.config.get("hooks")?.clone();
    let parsed = match HookEventsToml::deserialize(hook_value) {
        Ok(parsed) => parsed,
        Err(err) => {
            warnings.push(format!(
                "failed to parse TOML hooks in {}: {err}",
                source_path.display()
            ));
            return None;
        }
    };

    (!parsed.is_empty()).then_some((source_path, parsed))
}

fn config_toml_source_path(layer: &ConfigLayerEntry) -> AbsolutePathBuf {
    match &layer.name {
        ConfigLayerSource::PackagedDefaults { file }
        | ConfigLayerSource::System { file }
        | ConfigLayerSource::User { file, .. }
        | ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => file.clone(),
        ConfigLayerSource::Project { dot_codex_folder } => layer
            .hooks_config_folder()
            .unwrap_or_else(|| dot_codex_folder.clone())
            .join(CONFIG_TOML_FILE),
        ConfigLayerSource::Mdm { domain, key } => {
            synthetic_layer_path(&format!("<mdm:{domain}:{key}>/{CONFIG_TOML_FILE}"))
        }
        ConfigLayerSource::EnterpriseManaged { id, name } => synthetic_layer_path(&format!(
            "<enterprise-managed:{name}:{id}>/{CONFIG_TOML_FILE}"
        )),
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            synthetic_layer_path("<legacy-managed-config.toml-mdm>/managed_config.toml")
        }
        ConfigLayerSource::SessionFlags => synthetic_layer_path("<session-flags>/config.toml"),
    }
}

fn synthetic_layer_path(path: &str) -> AbsolutePathBuf {
    #[cfg(windows)]
    {
        AbsolutePathBuf::resolve_path_against_base(path, r"C:\")
    }

    #[cfg(not(windows))]
    {
        AbsolutePathBuf::resolve_path_against_base(path, "/")
    }
}

fn escape_xml_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn append_hook_events(
    handlers: &mut Vec<ConfiguredHandler>,
    hook_entries: &mut Vec<HookListEntry>,
    warnings: &mut Vec<String>,
    display_order: &mut i64,
    mut source: HookHandlerSource<'_>,
    hook_events: HookEventsToml,
    policy: HookDiscoveryPolicy,
) {
    if !policy.allows(&source) {
        return;
    }

    for (event_name, groups) in hook_events.into_matcher_groups() {
        append_matcher_groups(
            handlers,
            hook_entries,
            warnings,
            display_order,
            &mut source,
            event_name,
            groups,
        );
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the source function whole; splitting would obscure the port."
)]
fn append_matcher_groups(
    handlers: &mut Vec<ConfiguredHandler>,
    hook_entries: &mut Vec<HookListEntry>,
    warnings: &mut Vec<String>,
    display_order: &mut i64,
    source: &mut HookHandlerSource<'_>,
    event_name: ra_core::hook::HookEventName,
    groups: Vec<MatcherGroup>,
) {
    for (group_index, group) in groups.into_iter().enumerate() {
        let matcher = matcher_pattern_for_event(event_name, group.matcher.as_deref());
        let compiled_matcher = match matcher.map(HookMatcher::new).transpose() {
            Ok(matcher) => matcher,
            Err(err) => {
                let matcher = matcher.unwrap_or_default();
                let warning = format!(
                    "invalid matcher {matcher:?} in {}: {err}",
                    source.path.display()
                );
                if group.hooks.is_empty() {
                    warnings.push(warning);
                } else {
                    source.record_load_failure(warning, warnings);
                }
                continue;
            }
        };
        for (handler_index, handler) in group.hooks.iter().cloned().enumerate() {
            let normalized = match handler {
                HookHandlerConfig::Command {
                    command,
                    command_windows,
                    timeout_sec,
                    r#async,
                    status_message,
                    additional_context_limit,
                } => {
                    let command = if cfg!(windows) {
                        command_windows.unwrap_or(command)
                    } else {
                        command
                    };
                    if command.trim().is_empty() {
                        source.record_load_failure(
                            format!("skipping empty hook command in {}", source.path.display()),
                            warnings,
                        );
                        continue;
                    }
                    let timeout_sec = normalize_command_hook(
                        event_name,
                        timeout_sec,
                        source.path.as_path(),
                        warnings,
                    );
                    let runs_async = r#async && event_name != HookEventName::SessionEnd;
                    if r#async && !runs_async {
                        warnings.push(format!(
                            "running async {} hook synchronously in {}",
                            hook_event_name_label(event_name),
                            source.path.display()
                        ));
                    }
                    let additional_context_limit = if matches!(
                        event_name,
                        ra_core::hook::HookEventName::PreToolUse
                            | ra_core::hook::HookEventName::PostToolUse
                            | ra_core::hook::HookEventName::SessionStart
                            | ra_core::hook::HookEventName::UserPromptSubmit
                            | ra_core::hook::HookEventName::SubagentStart
                    ) {
                        additional_context_limit
                    } else {
                        if additional_context_limit.is_some() {
                            warnings.push(format!(
                                "ignoring additionalContextLimit for {event_name:?} hook in {}: this event cannot emit additionalContext",
                                source.path.display()
                            ));
                        }
                        None
                    };
                    let normalized_additional_context_limit = additional_context_limit
                        .filter(|limit| *limit != DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT);
                    let config = HookHandlerConfig::Command {
                        command: command.clone(),
                        command_windows: None,
                        timeout_sec: Some(timeout_sec),
                        r#async,
                        status_message: status_message.clone(),
                        additional_context_limit: normalized_additional_context_limit,
                    };
                    let command = source.env.iter().fold(command, |command, (key, value)| {
                        command.replace(&format!("${{{key}}}"), value)
                    });
                    NormalizedHandler {
                        config,
                        kind: ConfiguredHandlerKind::Command {
                            command,
                            env: source.env.clone(),
                            r#async: runs_async,
                        },
                        timeout_sec,
                        status_message,
                        additional_context_limit,
                    }
                }
                HookHandlerConfig::McpTool {
                    server,
                    tool,
                    input,
                    timeout_sec,
                    status_message,
                } => {
                    if event_name == HookEventName::SessionEnd {
                        source.record_load_failure(
                            format!(
                                "skipping MCP tool hook in {}: {} MCP hooks are not supported",
                                source.path.display(),
                                hook_event_name_label(event_name),
                            ),
                            warnings,
                        );
                        continue;
                    }
                    if server.trim().is_empty() || tool.trim().is_empty() {
                        source.record_load_failure(
                            format!(
                                "skipping MCP tool hook in {}: server and tool must not be empty",
                                source.path.display()
                            ),
                            warnings,
                        );
                        continue;
                    }
                    let timeout_sec = normalize_command_hook(
                        event_name,
                        timeout_sec,
                        source.path.as_path(),
                        warnings,
                    );
                    let config = HookHandlerConfig::McpTool {
                        server: server.clone(),
                        tool: tool.clone(),
                        input: input.clone(),
                        timeout_sec: Some(timeout_sec),
                        status_message: status_message.clone(),
                    };
                    NormalizedHandler {
                        config,
                        kind: ConfiguredHandlerKind::McpTool {
                            server,
                            tool,
                            input,
                        },
                        timeout_sec,
                        status_message,
                        additional_context_limit: None,
                    }
                }
                HookHandlerConfig::Prompt {} => {
                    source.record_load_failure(
                        format!(
                            "skipping prompt hook in {}: prompt hooks are not supported yet",
                            source.path.display()
                        ),
                        warnings,
                    );
                    continue;
                }
                HookHandlerConfig::Agent {} => {
                    source.record_load_failure(
                        format!(
                            "skipping agent hook in {}: agent hooks are not supported yet",
                            source.path.display()
                        ),
                        warnings,
                    );
                    continue;
                }
            };

            let NormalizedHandler {
                config,
                kind,
                timeout_sec,
                status_message,
                additional_context_limit,
            } = normalized;
            if let Err(error) = TomlValue::try_from(&config) {
                source.record_load_failure(
                    format!(
                        "cannot represent hook trust identity in {}: {error}",
                        source.path.display()
                    ),
                    warnings,
                );
                *display_order += 1;
                continue;
            }
            let current_hash = hook_hash(event_name, matcher, &group, &config);
            let key =
                crate::hooks::hook_key(&source.key_source, event_name, group_index, handler_index);
            let state = source.hook_states.get(&key);
            let builtin = source.plugin_id.as_deref().is_some_and(|plugin_id| {
                is_allowlisted_bundled_cleanup_hook(
                    plugin_id,
                    event_name,
                    group.matcher.as_deref(),
                    &config,
                    /*app_connector_id*/ None,
                )
            });
            let enabled = hook_enabled(source.is_managed, builtin, state);
            let trusted_hash = hook_trusted_hash(source.is_managed, state);
            let trust_status =
                hook_trust_status(source.is_managed, builtin, &current_hash, trusted_hash);
            let handler = match &kind {
                ConfiguredHandlerKind::Command {
                    command, r#async, ..
                } => HookListEntryHandler::Command {
                    command: command.clone(),
                    r#async: *r#async,
                },
                ConfiguredHandlerKind::McpTool { server, tool, .. } => {
                    HookListEntryHandler::McpTool {
                        server: server.clone(),
                        tool: tool.clone(),
                    }
                }
            };

            hook_entries.push(HookListEntry {
                builtin,
                key,
                event_name,
                handler,
                matcher: matcher.map(ToOwned::to_owned),
                timeout_sec,
                status_message: status_message.clone(),
                additional_context_limit,
                source_path: source.path.clone(),
                source: source.source,
                plugin_id: source.plugin_id.clone(),
                display_order: *display_order,
                enabled,
                is_managed: source.is_managed,
                current_hash,
                trust_status,
            });
            if enabled
                && (source.bypass_hook_trust
                    || matches!(
                        trust_status,
                        HookTrustStatus::Managed | HookTrustStatus::Trusted
                    ))
            {
                handlers.push(ConfiguredHandler {
                    builtin,
                    event_name,
                    matcher: compiled_matcher.clone(),
                    timeout_sec,
                    status_message,
                    additional_context_limit: AdditionalContextLimit::from_config(
                        additional_context_limit,
                    ),
                    source_path: source.path.clone().into(),
                    source: source.source,
                    display_order: *display_order,
                    kind,
                });
            }
            *display_order += 1;
        }
    }
}

fn hook_metadata_for_config_layer_source(source: &ConfigLayerSource) -> (HookSource, bool) {
    match source {
        ConfigLayerSource::PackagedDefaults { .. } => (HookSource::Unknown, false),
        ConfigLayerSource::System { .. } => (HookSource::System, true),
        ConfigLayerSource::User { .. } => (HookSource::User, false),
        ConfigLayerSource::Project { .. } => (HookSource::Project, false),
        ConfigLayerSource::Mdm { .. } => (HookSource::Mdm, true),
        ConfigLayerSource::EnterpriseManaged { .. } => (HookSource::CloudManagedConfig, true),
        ConfigLayerSource::SessionFlags => (HookSource::SessionFlags, false),
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { .. } => {
            (HookSource::LegacyManagedConfigFile, true)
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            (HookSource::LegacyManagedConfigMdm, true)
        }
    }
}

fn hook_source_for_requirement_source(source: Option<&RequirementSource>) -> HookSource {
    match source {
        Some(RequirementSource::MdmManagedPreferences { .. }) => HookSource::Mdm,
        Some(RequirementSource::SystemRequirementsToml { .. }) => HookSource::System,
        Some(RequirementSource::LegacyManagedConfigTomlFromFile { .. }) => {
            HookSource::LegacyManagedConfigFile
        }
        Some(RequirementSource::LegacyManagedConfigTomlFromMdm) => {
            HookSource::LegacyManagedConfigMdm
        }
        Some(RequirementSource::Composite { sources }) => {
            // Requirements hook composition preserves contributing sources in
            // priority order, but discovery only carries one source for the
            // whole merged hooks field. Use the primary contributor as the best
            // available coarse attribution.
            hook_source_for_requirement_source(sources.first())
        }
        Some(RequirementSource::EnterpriseManaged { .. }) => HookSource::CloudRequirements,
        Some(RequirementSource::Unknown) | None => HookSource::Unknown,
    }
}

use crate::hooks::timing::normalize_command_hook;
use crate::hooks::trust::{hook_enabled, hook_hash, hook_trust_status, hook_trusted_hash};
