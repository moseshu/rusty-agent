# Pinned Config test disposition

Status: implemented, awaiting re-review. No commit is authorized by this record.

Reference: `openai/codex@815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052`,
`codex-rs/core/src/config/{config_tests.rs,config_loader_tests.rs}`.

The inventory counts named test functions, not parameterized invocations. It includes direct
`ConfigBuilder` / `Config::load_*` calls and calls through `load_workspace_permission_profile`,
`load_current_time_reminder_config`, and `load_with_enterprise_requirement`.
This explicit scan yields **213 + 40 = 253 names**. Re-review found four more `config_tests.rs`
cases that build a Config only to feed `plugins_manager_for_config`; they are listed below as
10f, for **257 names** in all. The table below is the authoritative per-name accounting and retains
the indirectly constructed cases rather than silently discarding one to match an aggregate. The original 41 named counterparts
are distinguished from newly ported cases. Seven local builder tests are additional acceptance
cases and are not counted as upstream ports.

All tests whose assertions only consume the current Config snapshot are ported now. Deferred
cases name the concrete missing consumer. MCP OAuth is not treated as OpenAI account identity.

## Disposition totals

| Disposition | Named cases |
| --- | ---: |
| ported existing | 41 |
| port now | 199 |
| slice 2 | 3 |
| slice 3 | 2 |
| 10f | 8 |
| excluded account | 4 |

“Port now” preserves the upstream assertions subject only to these explicit adaptations:

- Product discovery paths and carveout assertions use `.rusty-agent`; `.codex` protection is also
  retained and independently asserted for canonical and legacy policies.
- Tests run in the external integration workspace. Absolute-path fixtures and source diagnostics
  use the existing local owners; test API forwarding gives access to private configuration helpers.
  Version-dependent assertions use the production crate's version, not the integration crate's 0.0.0.
- Unknown special-path warning assertions retain the existing product branding. The empty-filesystem
  warning still has its upstream Codex spelling; its expected diagnostic is unchanged.
- `for_config_writes_selected_user_config_file` uses the already ported
  `ConfigEditsBuilder::for_config_layer_stack`, since the shared owner cannot depend on product Config.
- Bootstrap HTTP cases keep all client-factory assertions; only the account auth-route assertion is
  excluded. The mixed managed-auth/backend-URL case is excluded for account fields; the retained URL
  is separately covered by `local_required_plugin_backend_url_is_projected_with_provenance`.
- Catalog tests retain one complete record from the pinned bundled catalog; the upstream cases
  themselves truncate the catalog to one model before asserting the exact Config value.
- Parameterized test_case cases use rstest with the same inputs and assertions.

## Per-test handling

| Source | Test | Disposition | Local target / remaining dependency |
| --- | --- | --- | --- |
| `config_tests.rs` | `load_config_normalizes_relative_cwd_override` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_applies_optional_mcp_startup_grace` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_resolves_thread_unload_delay` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_rejects_thread_unload_delay_overflow` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `test_toml_parsing` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `goal_max_token_budget_requires_positive_integer` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_resolves_experimental_request_user_input_enabled` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_resolves_non_prefixed_mcp_tool_servers` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_resolves_update_plan_enabled` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_resolves_code_mode_config` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_resolves_tool_registry_config` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_resolves_token_budget_config` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_rejects_overlong_auto_compact_fallback_prompt` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_rejects_invalid_token_budget_reminder_template` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_rejects_non_positive_token_budget_reminder_threshold` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_rejects_non_positive_auto_compact_fallback_buffer` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_rejects_missing_auto_compact_fallback_buffer` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_resolves_rollout_budget` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_rejects_enabled_rollout_budget_without_limit` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `load_config_resolves_current_time_reminder` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `managed_residency_warns_about_provider_header_overrides_without_mutating_config` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_applies_amazon_bedrock_aws_profile_override` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_applies_amazon_bedrock_transport_overrides` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `runtime_config_defaults_model_availability_nux` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `runtime_config_uses_tui_raw_output_mode` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `tui_auto_recap_defaults_and_cli_overrides` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_proxy_policy_does_not_start_managed_network_proxy_without_feature` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_proxy_policy_starts_managed_network_proxy` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `network_proxy_feature_is_no_op_without_sandbox_network` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `network_proxy_feature_matrix_preserves_sandbox_network_semantics` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `network_proxy_cli_overrides_merge_toggle_with_proxy_config` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `respect_system_proxy_feature_resolves_enabled` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `system_proxy_fallback_config_matches_bootstrap` | port now | `tests/it-coding/tests/product_config_upstream.rs`; HTTP bootstrap Config values ported; only account auth-route assertion excluded |
| `config_tests.rs` | `system_proxy_fallback_honors_feature_requirements` | port now | `tests/it-coding/tests/product_config_upstream.rs`; HTTP bootstrap Config values ported; only account auth-route assertion excluded |
| `config_tests.rs` | `respect_system_proxy_cli_override_enables_feature` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `experimental_network_requirements_enable_proxy_without_feature` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `network_proxy_feature_uses_profile_network_proxy_settings` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `disabled_network_proxy_feature_does_not_start_profile_proxy_policy` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_network_disabled_by_default_does_not_start_proxy` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `default_permissions_profile_populates_runtime_sandbox_policy` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `default_permissions_extended_profile_preserves_parent_metadata` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permission_profile_override_populates_runtime_permissions` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `persisted_permissions_initialize_proxy_with_expandable_managed_allowlist` | slice 2 | Builds proxy ConfigState and NetworkProxy, including execution policy rules |
| `config_tests.rs` | `persisted_permission_profile_id_wins_over_configured_default` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `missing_persisted_permission_profile_id_uses_configured_default` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `invalid_persisted_permission_profile_uses_configured_default` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `persisted_profile_cycle_uses_configured_default` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `missing_explicit_default_permissions_remains_an_error` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `permission_profile_override_preserves_managed_unrestricted_filesystem` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `managed_unrestricted_permission_profile_still_enables_network_requirements` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permission_profile_override_keeps_memories_root_out_of_legacy_projection` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permission_profile_override_preserves_configured_network_policy_without_starting_proxy` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `workspace_root_glob_none_compiles_to_filesystem_pattern_entry` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_require_default_permissions` | ported existing | `tests/it-coding/tests/product_config_defaults.rs`; Existing local test has this upstream name |
| `config_tests.rs` | `default_permissions_can_select_builtin_profile_without_permissions_table` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `default_permissions_read_only_keeps_add_dir_read_only` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `workspace_profile_applies_rules_to_runtime_and_profile_workspace_roots` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `explicit_builtin_workspace_profile_ignores_legacy_workspace_write_settings` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `default_permissions_profile_can_extend_builtin_workspace` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `default_permissions_profile_can_extend_builtin_read_only` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `empty_config_defaults_to_builtin_profile_for_trusted_project` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `empty_config_defaults_to_builtin_profile_for_untrusted_project` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `implicit_builtin_workspace_profile_preserves_sandbox_workspace_write_settings` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `implicit_builtin_workspace_profile_preserves_add_dir_metadata_carveouts` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `empty_config_defaults_to_builtin_read_only_without_trust_decision` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `default_permissions_can_select_builtin_full_access_profile` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `legacy_danger_no_sandbox_is_rejected` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `user_defined_permission_profile_names_cannot_use_builtin_prefix` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `unknown_builtin_permission_profile_name_is_rejected` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_allow_direct_write_roots_outside_workspace_root` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_reject_nested_entries_for_non_workspace_roots` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_reject_workspace_root_parent_traversal` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_allow_network_enablement` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `runtime_config_resolves_disable_paste_burst` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `disable_paste_burst_preserves_layering_and_new_key_precedence` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `runtime_config_resolves_terminal_resize_reflow_defaults_and_overrides` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `forced_chatgpt_workspace_id_empty_values_disable_runtime_restriction` | excluded account | OpenAI account identity / login machinery excluded by AGENTS.md |
| `config_tests.rs` | `legacy_remote_thread_store_endpoint_is_rejected` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `runtime_config_resolves_session_picker_view_default_and_override` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `runtime_config_resolves_resume_cwd_default_and_override` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `legacy_sandbox_mode_builds_profiles_with_compatible_projection` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `rebuild_with_session_layers_refreshes_requirements` | slice 2 | Session refresh and requirements reload lifecycle |
| `config_tests.rs` | `add_dir_override_extends_workspace_writable_roots` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `sqlite_home_defaults_to_codex_home_for_workspace_write` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `workspace_write_includes_configured_writable_root_once_without_memories_root` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `memory_tool_makes_memories_root_readable_without_creating_or_widening_writes` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_defaults_to_file_cli_auth_store_mode` | excluded account | OpenAI account identity / login machinery excluded by AGENTS.md |
| `config_tests.rs` | `config_resolves_explicit_keyring_auth_store_mode` | excluded account | OpenAI account identity / login machinery excluded by AGENTS.md |
| `config_tests.rs` | `config_applies_managed_auth_store_and_chatgpt_base_url` | excluded account | OpenAI account identity / login machinery excluded by AGENTS.md |
| `config_tests.rs` | `project_cannot_be_the_only_xaa_opt_in_source` | port now | `tests/it-coding/tests/product_config_upstream.rs`; MCP enterprise-auth source authorization is not OpenAI account identity |
| `config_tests.rs` | `config_resolves_default_oauth_store_mode` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `feedback_enabled_defaults_to_true` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `project_profiles_are_ignored` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `feature_table_overrides_legacy_flags` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `legacy_toggles_map_to_features` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `legacy_unified_exec_disable_flags_do_not_disable_command_execution` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `responses_websocket_features_do_not_change_wire_api` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_honors_explicit_file_oauth_store_mode` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `managed_config_overrides_oauth_store_mode` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `to_mcp_config_preserves_apps_feature_from_config` | 10f | Plugin-derived MCP admission and protocol/connection config; tracked with plugin MCP contribution chain |
| `config_tests.rs` | `to_mcp_config_flows_mcp_tool_prefix_from_feature` | 10f | Plugin-derived MCP admission and protocol/connection config; tracked with plugin MCP contribution chain |
| `config_tests.rs` | `to_mcp_config_flows_independent_mcp_2026_features` | 10f | Plugin-derived MCP admission and protocol/connection config; tracked with plugin MCP contribution chain |
| `config_tests.rs` | `to_mcp_config_preserves_auth_elicitation_feature_from_config` | 10f | Plugin-derived MCP admission and protocol/connection config; tracked with plugin MCP contribution chain |
| `config_tests.rs` | `to_mcp_config_empty_mcp_requirements_disable_plugin_mcps` | 10f | Plugin-derived MCP servers filtered by requirements through `plugins_manager_for_config`; plugin MCP contribution chain |
| `config_tests.rs` | `to_mcp_config_omits_plugin_id_when_user_server_shadows_plugin_mcp` | 10f | User server shadowing a plugin MCP through `plugins_manager_for_config`; plugin MCP contribution chain |
| `config_tests.rs` | `rebuild_with_session_layers_refreshes_plugin_derived_mcp_config` | 10f | Session-layer rebuild refreshing plugin-derived MCP config; plugin MCP contribution chain |
| `config_tests.rs` | `selected_plugin_wins_after_discovered_plugin_requirements` | 10f | Selected versus discovered plugin requirements through `plugins_manager_for_config`; plugin MCP contribution chain |
| `config_tests.rs` | `for_config_writes_selected_user_config_file` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `cli_override_sets_compact_prompt` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `loads_compact_prompt_from_file` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_uses_requirements_guardian_policy_config` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_uses_auto_review_guardian_policy_config_and_template` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `requirements_guardian_policy_beats_auto_review` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_ignores_empty_auto_review_guardian_policy_config` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_ignores_empty_requirements_guardian_policy_config` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_rejects_missing_agent_role_config_file` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `agent_role_relative_config_file_resolves_against_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `agent_role_relative_config_file_resolves_from_config_layer` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `agent_role_file_metadata_overrides_config_toml_metadata` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `agent_role_file_without_developer_instructions_is_dropped_with_warning` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `legacy_agent_role_config_file_allows_missing_developer_instructions` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `agent_role_without_description_after_merge_is_dropped_with_warning` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `discovered_agent_role_file_without_name_is_dropped_with_warning` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `agent_role_file_name_takes_precedence_over_config_key` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `loads_legacy_split_agent_roles_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `discovers_multiple_standalone_agent_role_files` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `mixed_legacy_and_standalone_agent_role_sources_merge_with_precedence` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `higher_precedence_agent_role_can_inherit_description_from_lower_layer` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_resolves_agent_controls` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_normalizes_agent_role_nickname_candidates` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_rejects_empty_agent_role_nickname_candidates` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_rejects_duplicate_agent_role_nickname_candidates` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_rejects_unsafe_agent_role_nickname_candidates` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `model_catalog_json_loads_from_path` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `model_catalog_json_rejects_empty_catalog` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `legacy_profile_selection_is_rejected` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `metrics_exporter_defaults_to_statsig_when_missing` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `trace_exporter_defaults_to_none_when_log_exporter_is_set` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_applies_otel_trace_metadata` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `load_config_drops_invalid_otel_trace_metadata_entries` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `explicit_null_service_tier_override_maps_to_default_service_tier` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `default_service_tier_override_uses_default_request_value` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `legacy_fast_service_tier_override_uses_priority_request_value` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_toml_priority_service_tier_uses_priority_request_value` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_toml_service_tier_accepts_arbitrary_string` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_toml_legacy_fast_service_tier_uses_priority_request_value` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `fast_default_opt_out_notice_config_is_respected` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `test_requirements_web_search_mode_allowlist_does_not_warn_when_unset` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `test_load_config_rejects_legacy_ollama_chat_provider_with_helpful_error` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_loads_mcp_oauth_callback_port_from_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_loads_allow_login_shell_from_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_loads_apps_mcp_product_sku_from_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_loads_cloud_skills_with_legacy_noop` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_loads_orchestrator_settings_from_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `config_loads_mcp_oauth_callback_url_from_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `untrusted_parent_repo_with_incomplete_child_git_keeps_unless_trusted_approval_policy` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `test_untrusted_project_gets_unless_trusted_approval_policy` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `oversized_managed_developer_instructions_are_rejected_during_config_load` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `requirements_disallowing_default_sandbox_falls_back_to_required_default` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `explicit_sandbox_mode_falls_back_when_disallowed_by_requirements` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `local_mxc_preference_preserves_configured_backend` | slice 2 | MXC validation plus resolve_runtime_refresh / RuntimeConfigRefresh::Mcp lifecycle |
| `config_tests.rs` | `windows_sandbox_mode_falls_back_when_disallowed_by_requirements` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `danger_full_access_with_never_is_rejected_when_requirements_force_read_only` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `named_full_access_profile_with_never_is_rejected_when_requirements_force_read_only` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permission_profile_override_falls_back_when_disallowed_by_requirements` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `active_profile_is_cleared_when_requirements_force_fallback` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `bypass_hook_trust_adds_startup_warning` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permission_profile_override_preserves_split_write_roots` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `requirements_web_search_mode_overrides_danger_full_access_default` | slice 3 | Calls provider-capability-dependent per-turn web-search selection |
| `config_tests.rs` | `requirements_disallowing_default_approval_falls_back_to_required_default` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `explicit_untrusted_approval_policy_is_rejected` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `feature_requirements_normalize_effective_feature_values` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `feature_requirements_can_still_disable_unified_exec` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `feature_requirements_auto_review_disables_guardian_approval` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `browser_feature_requirements_are_valid` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `in_app_chat_feature_requirements_are_valid` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `in_app_dictation_feature_requirements_are_valid` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `in_app_local_automation_feature_requirements_are_valid` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `explicit_feature_config_is_normalized_by_requirements` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `approvals_reviewer_defaults_to_manual_only_without_guardian_feature` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `prompt_instruction_blocks_can_be_disabled_from_config` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `approvals_reviewer_stays_manual_only_when_guardian_feature_is_enabled` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `approvals_reviewer_can_be_set_in_config_without_guardian_approval` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `requirements_disallowing_default_approvals_reviewer_falls_back_to_required_default` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `root_approvals_reviewer_falls_back_when_disallowed_by_requirements` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `profile_approvals_reviewer_falls_back_when_disallowed_by_requirements` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `approvals_reviewer_preserves_valid_user_choice_when_allowed_by_requirements` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `smart_approvals_alias_is_ignored` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `multi_agent_v2_config_from_feature_table` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `multi_agent_v2_default_session_thread_cap_counts_root` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `multi_agent_v2_allows_disabled_wait_agent_without_sleep_tool` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `multi_agent_v2_empty_usage_hint_overrides_are_preserved` | slice 3 | Resolves model-owned usage hints into prompt messages |
| `config_tests.rs` | `multi_agent_v2_uses_agents_max_concurrent_threads_per_session` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `catalog_v2_allows_agents_thread_limit_when_feature_disabled` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `multi_agent_v2_rejects_invalid_wait_timeouts` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `multi_agent_v2_rejects_invalid_tool_namespace` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `multi_agent_v2_session_thread_cap_one_disallows_subagents` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `feature_requirements_normalize_runtime_feature_mutations` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `feature_requirements_warn_on_collab_legacy_alias` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `feature_requirements_warn_and_ignore_unknown_feature` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `tool_suggest_discoverables_load_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `tool_suggest_disabled_tools_load_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `tool_suggest_disabled_tools_merge_across_config_layers` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `experimental_realtime_start_instructions_load_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `experimental_realtime_ws_base_url_loads_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `experimental_realtime_ws_backend_prompt_loads_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `experimental_realtime_ws_startup_context_loads_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `experimental_realtime_ws_model_loads_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `realtime_config_partial_table_uses_realtime_defaults` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `realtime_loads_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `realtime_audio_loads_from_config_toml` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_allow_unknown_special_paths` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_allow_unknown_special_paths_with_nested_entries` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_allow_missing_filesystem_with_warning` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `permissions_profiles_allow_empty_filesystem_with_warning` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `exact_requirements_apply_to_runtime_config` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_tests.rs` | `absent_allow_login_shell_does_not_report_an_override` | port now | `tests/it-coding/tests/product_config_upstream.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `cli_overrides_resolve_relative_paths_against_cwd` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `returns_config_error_for_schema_error_in_user_config` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `strict_config_rejects_unknown_user_config_key` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `non_strict_config_rejects_mixed_shell_environment_policy_before_higher_layer_merge` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `shell_environment_policy_unknown_fields_follow_strict_config` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `non_strict_config_merges_shell_filter_case_variants_across_layers` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `non_strict_config_rejects_malformed_shell_policy_before_representation_conversion` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `non_strict_config_allows_replaced_shell_policy_fields_outside_filter_representation` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `malformed_higher_shell_filter_reports_its_layer_when_lower_fields_are_replaced` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `strict_config_rejects_unknown_cli_override_key` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `strict_config_rejects_unknown_cli_override_key_with_relative_path_override` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `strict_config_rejects_unknown_feature_cli_override_key` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `strict_config_rejects_unknown_feature_user_config_key` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `strict_config_accepts_removed_shared_compression_key_without_changing_compression` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `managed_preferences_expand_home_directory_in_workspace_write_roots` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `system_requirements_control_in_app_updates` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `resolve_permission_profile_from_effective_configuration` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `system_requirements_define_managed_permission_profiles` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `credential_masking_respects_config_precedence` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `credential_broker_ambiguity_guard_survives_permission_profile_selection` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `resolve_permission_profile_inherits_across_configured_and_managed_profiles` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `resolve_permission_profile_rejects_duplicate_configured_and_managed_profiles` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `system_allowed_permission_profiles_select_managed_default_without_local_default` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `system_allowed_permission_profiles_require_managed_default` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `system_allowed_permission_profiles_standard_pair_defaults_to_workspace` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `system_managed_default_must_be_allowed` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `system_managed_default_requires_allowed_permission_profiles` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `system_allowed_permission_profiles_fall_back_from_disallowed_danger_full_access` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `system_allowed_permission_profiles_fall_back_from_disallowed_workspace` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `permission_profile_catalog_marks_profiles_disallowed_by_requirements` | ported existing | `tests/it-coding/tests/config_loader_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `system_requirements_preserve_allowed_configured_permission_default` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `system_requirements_warn_for_disallowed_explicit_permission_override` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `load_config_layers_can_ignore_managed_requirements` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `load_config_rejects_nul_in_required_deny_read_glob` | ported existing | `tests/it-coding/tests/product_config_permissions.rs`; Existing local test has this upstream name |
| `config_loader_tests.rs` | `project_paths_resolve_relative_to_dot_codex_and_override_in_order` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `cli_override_model_instructions_file_sets_base_instructions` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `inline_instructions_set_base_instructions` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `cli_override_can_update_project_local_mcp_server_when_project_is_trusted` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `cli_override_for_disabled_project_local_mcp_server_returns_invalid_transport` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |
| `config_loader_tests.rs` | `async_compaction_config` | port now | `tests/it-coding/tests/product_config_upstream_loader.rs`; Only effective Config, its builder inputs, or configuration-derived values |

## Additional contract regressions

- `product_config_state::local_dev_builds_force_file_mcp_oauth_store_modes` ports the upstream
  resolver case, including Keyring/Auto → File for 0.0.0 and preservation for release versions.
- `inherited_permission_state_retains_identity_and_constraints_without_shared_mutation` exercises
  restored state getter/setter and snapshot access. It verifies named profile identity, profile roots,
  constraint rejection and clone independence, while approval policy remains owned by the recipient.
- `product_metadata::workspace_profiles_and_legacy_policies_protect_both_product_config_names`
  checks workspace protection before and after metadata creation, existing metadata in additional
  roots, config/rules/hooks descendants, ordinary writable files and metadata-write diagnostics.

Plugin-MCP cases are deferred to 10f, the plugin MCP contribution chain, not to slice 2; this
does not reorder the product roadmap. No Config-only test case
is deferred merely because shell/thread execution is not yet connected.
