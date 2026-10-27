// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Configured policies restrict each declared MCP server without weakening it.
#![allow(
    clippy::implicit_hasher,
    reason = "Preserve the source concrete configuration map API."
)]
use ra_config::{McpServerConfig, PluginMcpServerConfig};
use std::collections::HashMap;
/// Applies configured activation, tool and approval restrictions to declared servers.
pub fn apply_configured_plugin_mcp_server_policies(
    policies: &HashMap<String, PluginMcpServerConfig>,
    servers: &mut HashMap<String, McpServerConfig>,
) {
    for (name, server) in servers {
        if let Some(policy) = policies.get(name) {
            server.enabled &= policy.enabled && !policy.has_unsupported_ema_auth;
            if policy.has_unsupported_ema_auth {
                server.auth = ra_config::McpServerAuth::EmaAuth;
            }
            let declared_approval_mode = server.default_tools_approval_mode.unwrap_or_default();

            if let Some(approval_mode) = policy.default_tools_approval_mode {
                server.default_tools_approval_mode =
                    Some(declared_approval_mode.restrict_to(approval_mode));
            }
            if let Some(enabled_tools) = &policy.enabled_tools {
                match &mut server.enabled_tools {
                    Some(declared_tools) => {
                        declared_tools.retain(|tool| enabled_tools.contains(tool));
                    }
                    None => server.enabled_tools = Some(enabled_tools.clone()),
                }
            }
            if let Some(disabled_tools) = &policy.disabled_tools {
                let declared_tools = server.disabled_tools.get_or_insert_default();
                for tool in disabled_tools {
                    if !declared_tools.contains(tool) {
                        declared_tools.push(tool.clone());
                    }
                }
            }
            for (tool_name, tool_policy) in &policy.tools {
                if tool_policy.approval_mode.is_some() || tool_policy.output_token_limit.is_some() {
                    server.tools.entry(tool_name.clone()).or_default();
                }
            }
            for (tool_name, tool_config) in &mut server.tools {
                if let Some(approval_mode) = policy
                    .tools
                    .get(tool_name)
                    .and_then(|tool_policy| tool_policy.approval_mode)
                    .or(policy.default_tools_approval_mode)
                {
                    tool_config.approval_mode = Some(
                        tool_config
                            .approval_mode
                            .unwrap_or(declared_approval_mode)
                            .restrict_to(approval_mode),
                    );
                }
                tool_config.restrict_output_token_limit(
                    policy
                        .tools
                        .get(tool_name)
                        .and_then(|tool_policy| tool_policy.output_token_limit),
                );
            }
        }
    }
}
