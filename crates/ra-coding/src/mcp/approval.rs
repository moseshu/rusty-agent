// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product MCP approval before execution, over the framework's recoverable in-call interruption.

use crate::thread_settings::CodingThreadSettings;
use async_trait::async_trait;
use ra_config::{AppToolApproval, AskForApproval};
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    item::ToolApproval,
    tool::{
        Tool, ToolApprovalPolicy, ToolApprovalRequest, ToolContext, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_mcp::{McpServerSource, ResolvedMcpServer, tool::McpTool};
use ra_protocol::server::{
    PendingApproval, ReviewDecision,
    protocol::{
        McpServerElicitationRequest, McpServerElicitationRequestParams, ServerRequestPayload,
        ToolRequestUserInputOption, ToolRequestUserInputParams, ToolRequestUserInputQuestion,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, PoisonError},
};

#[derive(Clone)]
pub(super) struct ProductMcpPolicy {
    default_mode: AppToolApproval,
    overrides: BTreeMap<String, AppToolApproval>,
    pub(super) plugin_id: Option<String>,
    allow_persistent: bool,
    pub(super) elicitation: bool,
    pub(super) persist_path: Option<PathBuf>,
}
impl ProductMcpPolicy {
    pub(super) fn new(server: &ResolvedMcpServer) -> Self {
        Self {
            default_mode: server
                .config()
                .default_tools_approval_mode
                .unwrap_or_default(),
            overrides: server
                .config()
                .tools
                .iter()
                .filter_map(|(name, tool)| tool.approval_mode.map(|mode| (name.clone(), mode)))
                .collect(),
            plugin_id: match server.source() {
                McpServerSource::Plugin(plugin) | McpServerSource::SelectedPlugin(plugin) => {
                    Some(plugin.plugin_id().to_owned())
                }
                _ => None,
            },
            allow_persistent: !matches!(server.source(), McpServerSource::SelectedPlugin(_)),
            elicitation: true,
            persist_path: None,
        }
    }
}

/// Non-account source cache identity; arguments deliberately do not scope a session grant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct McpApprovalKey {
    server: String,
    plugin_id: Option<String>,
    connector_id: Option<String>,
    link_id: Option<String>,
    tool_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct McpApprovalState {
    product_mcp_approval: bool,
    run_id: String,
    call_id: String,
    arguments: Value,
    key: McpApprovalKey,
    mode: AppToolApproval,
    elicitation: bool,
    allow_persistent: bool,
    persist_path: Option<PathBuf>,
    title: Option<String>,
    description: Option<String>,
}
impl McpApprovalState {
    pub(crate) fn from_approval(approval: &ToolApproval) -> Option<Self> {
        let state: Self = serde_json::from_value(approval.resume()?.state().clone()).ok()?;
        (state.product_mcp_approval
            && state.call_id == approval.call_id().as_str()
            && state.arguments == *approval.arguments())
        .then_some(state)
    }
    pub(crate) fn request(&self, pending: PendingApproval<'_>) -> ServerRequestPayload {
        let question_id = format!("mcp_tool_call_approval_{}", self.call_id);
        let message = format!(
            "Allow the {} MCP server to run tool \"{}\"?",
            self.key.server, self.key.tool_name
        );
        let session = self.mode == AppToolApproval::Auto;
        if self.elicitation {
            let mut meta = serde_json::Map::new();
            meta.insert("codex_approval_kind".into(), json!("mcp_tool_call"));
            if session {
                meta.insert(
                    "persist".into(),
                    if self.allow_persistent {
                        json!(["session", "always"])
                    } else {
                        json!("session")
                    },
                );
            }
            if let Some(title) = &self.title {
                meta.insert("tool_title".into(), json!(title));
            }
            if let Some(description) = &self.description {
                meta.insert("tool_description".into(), json!(description));
            }
            meta.insert("tool_params".into(), self.arguments.clone());
            if let Some(params) = self.arguments.as_object() {
                let mut display = params
                    .iter()
                    .map(
                        |(name, value)| json!({"name": name, "display_name": name, "value": value}),
                    )
                    .collect::<Vec<_>>();
                display.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
                meta.insert("tool_params_display".into(), json!(display));
            }
            return ServerRequestPayload::McpServerElicitationRequest(
                McpServerElicitationRequestParams::new(
                    pending.thread_id().into(),
                    Some(pending.turn_id().into()),
                    self.key.server.clone(),
                    McpServerElicitationRequest::Form {
                        meta: Some(Value::Object(meta)),
                        message,
                        requested_schema: json!({"type":"object", "properties":{}}),
                    },
                ),
            );
        }
        let mut options = vec![ToolRequestUserInputOption::new(
            "Allow",
            "Run the tool and continue.",
        )];
        if session {
            options.push(ToolRequestUserInputOption::new(
                "Allow for this session",
                "Run the tool and remember this choice for this session.",
            ));
        }
        options.push(ToolRequestUserInputOption::new(
            "Cancel",
            "Cancel this tool call.",
        ));
        ServerRequestPayload::ToolRequestUserInput(ToolRequestUserInputParams::new(
            pending.thread_id(),
            pending.turn_id(),
            &self.call_id,
            [
                ToolRequestUserInputQuestion::new(question_id, "Approve app tool call?", message)
                    .with_options(options),
            ],
        ))
    }
    pub(crate) async fn apply(&self, settings: &CodingThreadSettings, decision: &ReviewDecision) {
        if self.mode != AppToolApproval::Auto {
            return;
        }
        if !matches!(
            decision,
            ReviewDecision::ApprovedForSession | ReviewDecision::ApprovedMcpPolicyAmendment
        ) {
            return;
        }
        if matches!(decision, ReviewDecision::ApprovedMcpPolicyAmendment) && self.allow_persistent {
            if let Some(path) = &self.persist_path {
                let mut segments = self.key.plugin_id.as_ref().map_or_else(
                    || vec!["mcp_servers".into()],
                    |plugin| vec!["plugins".into(), plugin.clone(), "mcp_servers".into()],
                );
                segments.extend([
                    self.key.server.clone(),
                    "tools".into(),
                    self.key.tool_name.clone(),
                    "approval_mode".into(),
                ]);
                if let Err(error) = ra_config::ConfigEditsBuilder::for_config_path(path)
                    .with_edits([ra_config::ConfigEdit::SetPath {
                        segments,
                        value: toml_edit::value("approve"),
                    }])
                    .apply()
                    .await
                {
                    tracing::error!(%error, "failed to persist MCP tool approval; remembering it for this session");
                }
            } else {
                tracing::error!(
                    "MCP approval has no writable config source; remembering it for this session"
                );
            }
        }
        settings
            .mcp_approvals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.key.clone());
    }
}

pub(super) struct ProductMcpTool {
    inner: Arc<McpTool>,
    policy: Arc<ProductMcpPolicy>,
}
impl ProductMcpTool {
    pub(super) fn new(inner: Arc<McpTool>, policy: Arc<ProductMcpPolicy>) -> Self {
        Self { inner, policy }
    }
    fn state(&self, context: &ToolContext<'_>) -> McpApprovalState {
        let info = self.inner.info();
        let mode = self
            .policy
            .overrides
            .get(info.tool().name.as_ref())
            .copied()
            .unwrap_or(self.policy.default_mode);
        McpApprovalState {
            product_mcp_approval: true,
            run_id: context.run().run_id().as_str().to_owned(),
            call_id: context.call_id().as_str().to_owned(),
            arguments: context.arguments().clone(),
            key: McpApprovalKey {
                server: info.server_name().into(),
                plugin_id: self.policy.plugin_id.clone(),
                connector_id: info.connector_id().map(str::to_owned),
                link_id: None,
                tool_name: info.tool().name.to_string(),
            },
            mode,
            elicitation: self.policy.elicitation,
            allow_persistent: self.policy.allow_persistent,
            persist_path: self.policy.persist_path.clone(),
            title: info.tool().title.clone(),
            description: info.tool().description.as_ref().map(ToString::to_string),
        }
    }
}

/// Source context for automatically resolving a product MCP permission prompt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct McpPermissionPromptAutoApproveContext {
    /// The configured tool override, if any.
    pub tool_approval_mode: Option<AppToolApproval>,
}

/// Returns whether the selected effective profile automatically approves an MCP prompt.
#[must_use]
pub fn mcp_permission_prompt_is_auto_approved(
    approval_policy: AskForApproval,
    permission_profile: &ra_filesystem::models::PermissionProfile,
    context: McpPermissionPromptAutoApproveContext,
) -> bool {
    use ra_filesystem::models::PermissionProfile;
    if context.tool_approval_mode == Some(AppToolApproval::Approve) {
        return true;
    }
    if approval_policy != AskForApproval::Never {
        return false;
    }
    match permission_profile {
        PermissionProfile::Disabled | PermissionProfile::External { .. } => true,
        PermissionProfile::Managed { file_system, .. } => {
            file_system.to_sandbox_policy().has_full_disk_write_access()
        }
    }
}

fn auto_approved(settings: &CodingThreadSettings, mode: AppToolApproval) -> bool {
    use ra_config::SandboxPolicy;
    use ra_filesystem::legacy::{NetworkAccess, SandboxPolicy as Legacy};
    // The app-server and executor legacy policies have different nominal and wire types.
    // Lower the selected policy through the shared source implementation, including carveouts.
    let legacy = match settings.sandbox_policy() {
        SandboxPolicy::DangerFullAccess => Legacy::DangerFullAccess,
        SandboxPolicy::ExternalSandbox { network_access } => Legacy::ExternalSandbox {
            network_access: if *network_access == ra_config::NetworkAccess::Enabled {
                NetworkAccess::Enabled
            } else {
                NetworkAccess::Restricted
            },
        },
        SandboxPolicy::ReadOnly { network_access } => Legacy::ReadOnly {
            network_access: *network_access,
        },
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => {
            let Ok(writable_roots) = writable_roots
                .iter()
                .map(ra_paths::AbsolutePathBuf::from_absolute_path)
                .collect::<std::result::Result<Vec<_>, _>>()
            else {
                return false;
            };
            Legacy::WorkspaceWrite {
                writable_roots,
                network_access: *network_access,
                exclude_tmpdir_env_var: *exclude_tmpdir_env_var,
                exclude_slash_tmp: *exclude_slash_tmp,
            }
        }
        _ => return false,
    };
    let profile = ra_filesystem::models::PermissionProfile::from_legacy_sandbox_policy_for_cwd(
        &legacy,
        settings.cwd(),
    );
    mcp_permission_prompt_is_auto_approved(
        settings.approval_policy(),
        &profile,
        McpPermissionPromptAutoApproveContext {
            tool_approval_mode: Some(mode),
        },
    )
}

#[async_trait]
impl Tool for ProductMcpTool {
    fn origin(&self) -> &ToolOrigin {
        self.inner.origin()
    }
    fn recipient_binding(&self) -> Option<Value> {
        self.inner.recipient_binding()
    }
    fn schema(&self) -> &ToolSchema {
        self.inner.schema()
    }
    fn title(&self) -> Option<&str> {
        self.inner.title()
    }
    // Product approval is requested inside the invocation, as in the source. Framework
    // always-approve answers and action-class auto-allow cannot turn it into a persistent grant.
    fn options(&self) -> ToolOptions {
        self.inner
            .options()
            .with_approval(ToolApprovalPolicy::Never)
    }
    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        self.inner.needs_approval(context).await
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let state = self.state(&context);
        let settings = context.run().app_context::<CodingThreadSettings>();
        if settings.is_some_and(|settings| auto_approved(settings, state.mode))
            || !self.inner.needs_approval(&context).await?
        {
            return self.inner.invoke(context.arguments().clone()).await;
        }
        if state.mode == AppToolApproval::Auto
            && settings.is_some_and(|settings| {
                settings
                    .mcp_approvals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains(&state.key)
            })
        {
            return self.inner.invoke(context.arguments().clone()).await;
        }
        let refusal = |message| {
            Error::tool(
                ToolErrorKind::PermissionDenied,
                self.origin().name(),
                message,
            )
        };
        if settings.is_some_and(|settings| settings.approval_policy() == AskForApproval::Never) {
            return Err(refusal(
                "MCP tool call requires approval, but approval policy is never",
            ));
        }
        if !state.elicitation && context.run().parent_run_id().is_some() {
            return Err(refusal(
                "MCP tool approval via request_user_input requires the root thread. Ask the parent agent to handle this request. Do not retry the blocked action until the parent confirms the blocker is resolved.",
            ));
        }
        if let Some(saved) = context
            .resume_state()
            .cloned()
            .and_then(|value| serde_json::from_value::<McpApprovalState>(value).ok())
        {
            if saved.product_mcp_approval
                && saved.run_id == state.run_id
                && saved.call_id == state.call_id
                && saved.key == state.key
                && saved.arguments == state.arguments
            {
                return self.inner.invoke(context.arguments().clone()).await;
            }
            return Err(refusal("MCP approval does not match this invocation"));
        }
        let state = serde_json::to_value(state)
            .map_err(|error| Error::config("failed to capture MCP approval").with_source(error))?;
        Err(ToolApprovalRequest::new(state).into_error(self.origin().name()))
    }
}
