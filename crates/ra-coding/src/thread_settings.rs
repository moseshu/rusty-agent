//! Immutable coding policy snapshots and the app-server adapter that previews them.
//!
//! Approval and sandboxing are independent, as in Codex's tool sandboxing mechanism. The wire
//! policy is lowered to the existing executor; no coding policy vocabulary enters the core.
//!
//! # Integration and compatibility
//!
//! The host selects this adapter, calls [`CodingThreadSettingsAdapter::initialize`] on its run
//! configuration, and loads that thread with the same adapter. Generic framework hosts need no
//! coding policy. The immutable snapshot occupies the application's context slot; a coding host
//! using another application context must compose those payloads in its own selected adapter.
//! The named catalog supports the legacy filesystem policies the executor can enforce. Profile
//! inheritance, glob grants and externally managed mounts require a richer host adapter; an
//! unknown profile never falls back to ambient authority. External sandbox enforcement is an
//! explicit embedding-host responsibility. Reviewer selection is retained for the server request
//! producer; it does not install an automatic reviewer on its own.
//!
//! Commands are decided by Codex's command policy (see [`crate::exec_policy`]) with the rules the
//! host selects through [`CodingThreadSettingsAdapter::with_exec_policy`]: a command that needs
//! approval asks, a forbidden one is refused with Codex's reason when it is called, and one every
//! rule explicitly allows runs outside the sandbox. A call's sandbox override and requested prefix
//! rule are not yet arguments of the command tool, so every call uses the defaults.
//!
//! Restricted in-process patches use Codex's unavailable-patch-sandbox approval path: this writer
//! has directory capabilities, but does not run beneath the command executor's kernel sandbox.
//! Commands do enter that sandbox. The existing executor's workspace policy grants entire writable
//! roots; it does not yet port Codex's nested protected-directory rules. Those remain an execution
//! adapter parity gap rather than a new provider-neutral permission rule.

use crate::exec_policy::{
    AllowPrefixRules, ExecApprovalRequest, ExecApprovalRequirement, ExecPolicyManager,
    FileSystemSandboxKind, SandboxPermissions,
};
use ra_core::{
    error::{Error, Result},
    permission::PermissionMode,
    tool::ToolContext,
};
use ra_exec::sandbox::{
    ExecEnvironment, NetworkAccess as ExecNetworkAccess, SandboxLevel,
    SandboxPolicy as ExecSandboxPolicy,
};
use ra_protocol::server::{
    ThreadSettingsAdapter,
    protocol::{ApprovalsReviewer, AskForApproval, SandboxPolicy, ThreadPolicyOverrides},
};
use ra_runtime::{permission::PermissionEngine, runner::RunConfig};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// A policy captured by one model step and its tools.
#[derive(Clone, Debug)]
pub struct CodingThreadSettings {
    cwd: PathBuf,
    approval_policy: AskForApproval,
    approvals_reviewer: ApprovalsReviewer,
    sandbox_policy: SandboxPolicy,
    active_permission_profile: Option<String>,
    base_permission: PermissionEngine,
    exec_policy: Arc<ExecPolicyManager>,
    allow_prefix_rules: AllowPrefixRules,
}

impl CodingThreadSettings {
    /// The configured destination for approval review, captured with a model step.
    #[must_use]
    pub const fn approvals_reviewer(&self) -> ApprovalsReviewer {
        self.approvals_reviewer
    }

    /// The independent approval-prompt policy.
    #[must_use]
    pub const fn approval_policy(&self) -> AskForApproval {
        self.approval_policy
    }

    /// The filesystem and network policy selected by the host.
    #[must_use]
    pub const fn sandbox_policy(&self) -> &SandboxPolicy {
        &self.sandbox_policy
    }
}

/// Host-selected permission catalog. Unknown profiles are rejected without changing the thread.
#[derive(Clone, Debug)]
pub struct CodingThreadSettingsAdapter {
    initial: CodingThreadSettings,
    profiles: BTreeMap<String, SandboxPolicy>,
}

impl CodingThreadSettingsAdapter {
    /// Selects an explicit initial sandbox policy for this workspace.
    pub fn new(cwd: impl Into<PathBuf>, sandbox_policy: SandboxPolicy) -> Result<Self> {
        let cwd = cwd.into();
        if !cwd.is_absolute() {
            return Err(Error::config("thread cwd must be absolute"));
        }
        ThreadPolicyOverrides::new()
            .with_sandbox_policy(sandbox_policy.clone())
            .validate()
            .map_err(Error::config)?;
        let mut profiles = BTreeMap::new();
        profiles.insert(
            ":danger-full-access".into(),
            SandboxPolicy::DangerFullAccess,
        );
        profiles.insert(
            ":read-only".into(),
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        );
        profiles.insert(
            ":workspace".into(),
            SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![],
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
        );
        Ok(Self {
            initial: CodingThreadSettings {
                cwd,
                sandbox_policy,
                approval_policy: AskForApproval::default(),
                approvals_reviewer: ApprovalsReviewer::default(),
                active_permission_profile: None,
                base_permission: PermissionEngine::default(),
                exec_policy: Arc::new(ExecPolicyManager::default()),
                allow_prefix_rules: AllowPrefixRules::default(),
            },
            profiles,
        })
    }

    /// Decides commands with `exec_policy`'s rules, as Codex's session does with the rules its
    /// config layers load. Without one, no rule matches and every command is decided by Codex's
    /// heuristics for unmatched commands.
    #[must_use]
    pub fn with_exec_policy(mut self, exec_policy: Arc<ExecPolicyManager>) -> Self {
        self.initial.exec_policy = exec_policy;
        self
    }

    /// Whether allow prefix rules are honored and reusable amendments proposed; honored unless the
    /// host chooses otherwise.
    #[must_use]
    pub const fn with_allow_prefix_rules(mut self, allow_prefix_rules: AllowPrefixRules) -> Self {
        self.initial.allow_prefix_rules = allow_prefix_rules;
        self
    }

    /// Adds a configured profile, the product's equivalent of Codex's named permission catalog.
    pub fn with_profile(mut self, name: impl Into<String>, policy: SandboxPolicy) -> Result<Self> {
        ThreadPolicyOverrides::new()
            .with_sandbox_policy(policy.clone())
            .validate()
            .map_err(Error::config)?;
        self.profiles.insert(name.into(), policy);
        Ok(self)
    }

    /// Installs the explicitly selected product snapshot on a framework configuration.
    pub fn initialize(&self, config: RunConfig) -> RunConfig {
        let mut settings = self.initial.clone();
        settings.base_permission = config.permission().clone();
        install(config, settings)
    }

    fn snapshot(&self, config: &RunConfig) -> CodingThreadSettings {
        config
            .app_context()
            .and_then(|value| value.downcast_ref::<CodingThreadSettings>())
            .cloned()
            .unwrap_or_else(|| {
                let mut initial = self.initial.clone();
                initial.base_permission = config.permission().clone();
                initial
            })
    }
}

impl ThreadSettingsAdapter for CodingThreadSettingsAdapter {
    fn prepare(
        &self,
        current: &RunConfig,
        update: &ThreadPolicyOverrides,
    ) -> std::result::Result<RunConfig, String> {
        update.validate()?;
        let mut settings = self.snapshot(current);
        if let Some(policy) = update.approval_policy() {
            settings.approval_policy = policy;
        }
        if let Some(reviewer) = update.approvals_reviewer() {
            settings.approvals_reviewer = reviewer;
        }
        if let Some(policy) = update.sandbox_policy() {
            settings.sandbox_policy = policy.clone();
            settings.active_permission_profile = None;
        }
        if let Some(profile) = update.permissions() {
            settings.sandbox_policy = self
                .profiles
                .get(profile)
                .cloned()
                .ok_or_else(|| format!("unknown permissions profile: {profile}"))?;
            settings.active_permission_profile = Some(profile.to_owned());
        }
        Ok(install(current.clone(), settings))
    }

    fn describe(&self, config: &RunConfig) -> Value {
        let settings = self.snapshot(config);
        json!({"cwd": settings.cwd, "approvalPolicy": settings.approval_policy,
            "approvalsReviewer": settings.approvals_reviewer, "sandboxPolicy": settings.sandbox_policy,
            "activePermissionProfile": settings.active_permission_profile.map(|id| json!({"id": id, "extends": null})),
            "disabledPluginIds": [], "collaborationMode": null,
            "multiAgentMode": "explicitRequestOnly", "personality": null})
    }
}

fn install(config: RunConfig, settings: CodingThreadSettings) -> RunConfig {
    // Never suppresses prompts rather than granting tools, so explicit denials keep their meaning.
    let no_prompts = matches!(settings.approval_policy, AskForApproval::Never);
    let permission = settings.base_permission.clone().with_mode(if no_prompts {
        PermissionMode::DontAsk
    } else {
        settings.base_permission.mode()
    });
    config
        .with_permission_engine(permission)
        .with_app_context(Arc::new(settings))
}

pub(crate) fn command_environment(
    context: &ToolContext<'_>,
    base: &ExecEnvironment,
) -> Result<ExecEnvironment> {
    let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
        return Ok(base.clone());
    };
    // Decided again for the call that runs: a forbidden command is refused here with Codex's
    // reason, and one every rule explicitly allows runs outside the sandbox on its first attempt.
    match command_requirement(context, settings) {
        ExecApprovalRequirement::Forbidden { reason } => {
            return Err(ra_tools::exec_command::rejection(reason));
        }
        ExecApprovalRequirement::Skip {
            bypass_sandbox: true,
            ..
        } => {
            return Ok(base
                .clone()
                .with_sandbox(ExecSandboxPolicy::new(SandboxLevel::Unconfined)));
        }
        ExecApprovalRequirement::Skip { .. } | ExecApprovalRequirement::NeedsApproval { .. } => {}
    }
    let mut environment = base.clone();
    let policy = match &settings.sandbox_policy {
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. } => {
            // External sandbox enforcement, including its network policy, is the embedding host's
            // responsibility, exactly as Codex's externalSandbox contract states.
            ExecSandboxPolicy::new(SandboxLevel::Unconfined)
        }
        SandboxPolicy::ReadOnly { network_access } => {
            environment = environment.without_temp_dir();
            ExecSandboxPolicy::new(SandboxLevel::WorkspaceWrite)
                .with_network(network(*network_access))
        }
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => {
            let mut policy = ExecSandboxPolicy::new(SandboxLevel::WorkspaceWrite)
                .with_network(network(*network_access))
                .with_writable_root(settings.cwd.clone());
            for root in writable_roots {
                policy = policy.with_writable_root(root.clone());
            }
            if !exclude_slash_tmp {
                policy = policy.with_writable_root(PathBuf::from("/tmp"));
            }
            if !exclude_tmpdir_env_var && let Some(path) = std::env::var_os("TMPDIR") {
                policy = policy.with_writable_root(PathBuf::from(path));
            }
            policy
        }
        // A policy this adapter does not know is confined as read-only without network, never
        // granted ambient authority.
        _ => {
            environment = environment.without_temp_dir();
            ExecSandboxPolicy::new(SandboxLevel::WorkspaceWrite).with_network(network(false))
        }
    };
    Ok(environment.with_sandbox(policy))
}

fn network(enabled: bool) -> ExecNetworkAccess {
    if enabled {
        ExecNetworkAccess::Allowed
    } else {
        ExecNetworkAccess::Denied
    }
}

// The signature is the command tool's approval resolver, which may fail; this one never does.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn command_approval(context: &ToolContext<'_>) -> Result<bool> {
    let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
        return Ok(true);
    };
    // Only a command that needs approval asks. A forbidden one is refused when it is called, with
    // the reason the model reads in place of its result.
    Ok(matches!(
        command_requirement(context, settings),
        ExecApprovalRequirement::NeedsApproval { .. }
    ))
}

/// The shell the coding executor runs a command with when the call names none.
const DEFAULT_SHELL: &str = "/bin/sh";

/// What Codex's command policy requires of the call `context` makes.
///
/// The command is judged in Codex's form, `[shell, "-lc" or "-c", cmd]`, whatever argument layout
/// the executor then starts the shell with. The call's per-command sandbox override and requested
/// prefix rule are the defaults until the command tool takes them.
fn command_requirement(
    context: &ToolContext<'_>,
    settings: &CodingThreadSettings,
) -> ExecApprovalRequirement {
    let arguments = context.arguments();
    let cmd = arguments
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let shell = arguments
        .get("shell")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_SHELL);
    let login = arguments
        .get("login")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let command = vec![
        shell.to_owned(),
        if login { "-lc" } else { "-c" }.to_owned(),
        cmd.to_owned(),
    ];
    settings
        .exec_policy
        .create_exec_approval_requirement_for_shell(
            ExecApprovalRequest {
                command: &command,
                approval_policy: settings.approval_policy,
                file_system_sandbox: FileSystemSandboxKind::of(&settings.sandbox_policy),
                sandbox_permissions: SandboxPermissions::UseDefault,
                prefix_rule: None,
                allow_prefix_rules: settings.allow_prefix_rules,
            },
            Path::new(DEFAULT_SHELL),
        )
}

pub(crate) fn patch_approval(context: &ToolContext<'_>) -> Result<bool> {
    let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
        return Ok(true);
    };
    if matches!(settings.sandbox_policy, SandboxPolicy::ReadOnly { .. }) {
        return match settings.approval_policy {
            AskForApproval::Never
            | AskForApproval::Granular {
                sandbox_approval: false,
                ..
            } => Err(Error::caller(
                "apply_patch is denied by the read-only sandbox policy",
            )),
            _ => Ok(true),
        };
    }
    let restricted = !matches!(
        settings.sandbox_policy,
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. }
    );
    // This in-process writer does not enter a kernel sandbox. Follow Codex's patch-safety path
    // for an unavailable patch sandbox: restricted patches require approval, even when their
    // paths are within the workspace. Directory capabilities alone do not constrain hard links.
    if restricted {
        return match settings.approval_policy {
            AskForApproval::Never
            | AskForApproval::Granular {
                sandbox_approval: false,
                ..
            } => Err(Error::caller(
                "patch rejected: the patch executor cannot enforce the selected sandbox",
            )),
            _ => Ok(true),
        };
    }
    Ok(matches!(
        settings.approval_policy,
        AskForApproval::UnlessTrusted
    ))
}
