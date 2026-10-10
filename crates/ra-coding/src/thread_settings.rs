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
//! rule explicitly allows runs outside the sandbox. The call's `sandbox_permissions`,
//! `justification` and `prefix_rule` are checked and applied as Codex's `exec_command` handler
//! does: `justification` needs an explicit `sandbox_permissions`, an approval policy that rejects
//! sandbox prompts refuses a sandbox override, and an approved `require_escalated` runs its first
//! attempt outside the sandbox.
//!
//! A call is decided once, when its approval is asked, and runs as decided, as Codex's handler
//! decides a command once before asking and running it: an amendment approved meanwhile, see
//! [`CodingThreadSettings::persist_approved_execpolicy_amendment`], changes later calls and not
//! this one. The decision is held in memory between the two, so a call resumed in another process
//! is decided again when it runs.
//! Invocation identity covers the tool and complete arguments. Settled calls retain only their
//! identity until the turn ends, preventing call-ID reuse. The app-server listener forwards events
//! to [`ThreadSettingsAdapter::on_thread_event`] for cleanup; an embedding host loading threads
//! outside that server must forward their events to the adapter itself.
//!
//! Restricted in-process patches use Codex's unavailable-patch-sandbox approval path: this writer
//! has directory capabilities, but does not run beneath the command executor's kernel sandbox.
//! Commands do enter that sandbox. The existing executor's workspace policy grants entire writable
//! roots; it does not yet port Codex's nested protected-directory rules. Those remain an execution
//! adapter parity gap rather than a new provider-neutral permission rule.

use crate::exec_policy::{
    AllowPrefixRules, ExecApprovalRequest, ExecApprovalRequirement, ExecPolicyManager,
    FileSystemSandboxKind, SandboxPermissions, prompt_is_rejected_by_policy,
};
use ra_core::{
    error::{Error, Result},
    item::{CallId, RunItemKind},
    permission::PermissionMode,
    state::{ArgumentFingerprint, RunId},
    tool::{ToolContext, ToolLookupKey},
};
use ra_exec::sandbox::{
    ExecEnvironment, NetworkAccess as ExecNetworkAccess, SandboxLevel,
    SandboxPolicy as ExecSandboxPolicy,
};
use ra_protocol::server::{
    ReviewDecision, ThreadSettingsAdapter,
    protocol::{ApprovalsReviewer, AskForApproval, SandboxPolicy, ThreadPolicyOverrides},
};
use ra_runtime::{permission::PermissionEngine, runner::RunConfig, thread::ThreadEvent};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
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
    rules_home: Option<PathBuf>,
    command_decisions: Arc<CommandDecisions>,
}

impl CodingThreadSettings {
    /// What the command policy decided for a call that asked for approval and has not run yet: the
    /// reason and proposed amendment its approval request carries.
    #[must_use]
    pub fn pending_command_requirement(
        &self,
        run_id: &RunId,
        call_id: &CallId,
    ) -> Option<ExecApprovalRequirement> {
        self.command_decisions
            .pending()
            .get(&(run_id.clone(), call_id.clone()))
            .and_then(|record| record.decision.as_ref())
            .map(|decision| decision.requirement.clone())
    }

    /// Applies the amendment a command was approved with, as Codex's `exec_approval` does: its
    /// prefix is appended to the rules file under the host's rules home and allowed from now on.
    /// Any other decision is left alone.
    ///
    /// Returns Codex's warning when the amendment cannot be applied, including when the host set
    /// no rules home. The approval stands either way: the call runs as it was decided.
    pub async fn persist_approved_execpolicy_amendment(
        &self,
        decision: &ReviewDecision,
    ) -> Option<String> {
        let ReviewDecision::ApprovedExecpolicyAmendment {
            proposed_execpolicy_amendment,
        } = decision
        else {
            return None;
        };
        let result = match &self.rules_home {
            Some(rules_home) => self
                .exec_policy
                .append_amendment_and_update(rules_home, proposed_execpolicy_amendment)
                .await
                .map_err(|err| err.to_string()),
            None => Err("no rules home is configured for this thread".to_owned()),
        };
        result.err().map(|err| {
            let message = format!("Failed to apply execpolicy amendment: {err}");
            tracing::warn!("{message}");
            message
        })
    }

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
                rules_home: None,
                command_decisions: Arc::new(CommandDecisions::default()),
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

    /// The folder approved amendments are written under, as `rules/default.rules`: Codex's
    /// `codex_home`. Naming it among the exec policy's configuration folders as well makes the
    /// amendments apply again when the rules are next loaded.
    #[must_use]
    pub fn with_rules_home(mut self, rules_home: impl Into<PathBuf>) -> Self {
        self.initial.rules_home = Some(rules_home.into());
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

    fn on_thread_event(&self, config: &RunConfig, event: &ThreadEvent) {
        let Some(settings) = config
            .app_context()
            .and_then(|value| value.downcast_ref::<CodingThreadSettings>())
        else {
            return;
        };
        match event {
            ThreadEvent::Item { turn_id, item } => {
                if let RunItemKind::ToolCallOutput(output) = item.kind() {
                    settings
                        .command_decisions
                        .settle(&RunId::new(turn_id), output.call_id());
                }
            }
            ThreadEvent::TurnComplete { turn_id, .. }
            | ThreadEvent::TurnAborted { turn_id, .. } => {
                settings.command_decisions.finish_run(&RunId::new(turn_id));
            }
            _ => {}
        }
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
    // The call runs as it was decided when its approval was asked; one decided for the first time
    // here, without asking, is decided the same way. A forbidden command is refused with Codex's
    // reason.
    let decision = settings
        .command_decisions
        .begin_execution(context, || command_decision(context, settings))?;
    if let ExecApprovalRequirement::Forbidden { reason } = decision.requirement {
        return Err(ra_tools::exec_command::rejection(reason));
    }
    // Codex's first-attempt sandbox override: a command every rule explicitly allows, or an
    // approved `require_escalated` one, runs outside the sandbox. Codex keeps the sandbox when the
    // policy denies reads; the policies this adapter lowers have no denied reads.
    let bypass_sandbox = matches!(
        decision.requirement,
        ExecApprovalRequirement::Skip {
            bypass_sandbox: true,
            ..
        }
    ) || decision
        .sandbox_permissions
        .requires_escalated_permissions();
    if bypass_sandbox {
        return Ok(base
            .clone()
            .with_sandbox(ExecSandboxPolicy::new(SandboxLevel::Unconfined)));
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

pub(crate) fn command_approval(context: &ToolContext<'_>) -> Result<bool> {
    let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
        return Ok(true);
    };
    // Only a command that needs approval asks. A forbidden one, or one whose arguments Codex's
    // handler refuses, is refused when it is called, with the reason the model reads in place of
    // its result.
    let decision = if let Some(decision) = settings.command_decisions.current(context)? {
        decision
    } else {
        let Ok(decision) = command_decision(context, settings) else {
            return Ok(false);
        };
        settings.command_decisions.record(context, decision)?
    };
    let needs_approval = matches!(
        decision.requirement,
        ExecApprovalRequirement::NeedsApproval { .. }
    );
    Ok(needs_approval)
}

/// The shell the coding executor runs a command with when the call names none.
const DEFAULT_SHELL: &str = "/bin/sh";

/// Codex's refusal of a `justification` sent without an explicit `sandbox_permissions`.
const JUSTIFICATION_WITHOUT_SANDBOX_PERMISSIONS: &str = "`justification` requires an explicit `sandbox_permissions`; use `sandbox_permissions: \"require_escalated\"` for unsandboxed execution, or omit `justification`.";

/// The arguments of a command call the policy reads.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommandArguments {
    cmd: String,
    shell: Option<String>,
    login: Option<bool>,
    sandbox_permissions: Option<SandboxPermissions>,
    justification: Option<String>,
    prefix_rule: Option<Vec<String>>,
}

/// What was decided about one command call, kept from its approval to its execution.
#[derive(Clone, Debug)]
struct CommandDecision {
    requirement: ExecApprovalRequirement,
    sandbox_permissions: SandboxPermissions,
}

/// The invocation identity agents-python binds to a provider call ID. The existing canonical
/// argument digest preserves JSON value types and ignores object key order.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CommandIdentity {
    tool: ToolLookupKey,
    arguments: ArgumentFingerprint,
}

impl CommandIdentity {
    fn of(context: &ToolContext<'_>) -> Self {
        Self {
            tool: context.origin().lookup_key().clone(),
            arguments: ArgumentFingerprint::compute(context.arguments()),
        }
    }
}

/// Retains identity after settlement, as agents-python's invocation records do, so a completed
/// call ID cannot authorize a different command. Only a pending call retains its decision.
#[derive(Debug)]
struct CommandRecord {
    identity: CommandIdentity,
    decision: Option<CommandDecision>,
}

impl CommandRecord {
    fn check(&self, context: &ToolContext<'_>, identity: &CommandIdentity) -> Result<()> {
        if &self.identity != identity {
            return Err(Error::caller(format!(
                "Model reused tool call ID `{}` for a different invocation. Use a unique call ID for each tool invocation.",
                context.call_id()
            )));
        }
        Ok(())
    }

    fn pending_decision(&self, context: &ToolContext<'_>) -> Result<CommandDecision> {
        self.decision.clone().ok_or_else(|| {
            Error::caller(format!(
                "Tool call `{}` has already settled. Use a unique call ID for each tool invocation.",
                context.call_id()
            ))
        })
    }
}

/// Frozen decisions and invocation identities for active turns. The selected thread adapter
/// observes settled outputs and turn completion through the existing thread listener: Codex owns
/// the pending decision in its tool request, while this interrupted runtime must keep it outside
/// the tool until approval resumes. Turn termination releases the records, including refusals and
/// calls cancelled before entering the tool.
#[derive(Debug, Default)]
struct CommandDecisions {
    pending: Mutex<HashMap<(RunId, CallId), CommandRecord>>,
}

impl CommandDecisions {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<(RunId, CallId), CommandRecord>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn current(&self, context: &ToolContext<'_>) -> Result<Option<CommandDecision>> {
        let identity = CommandIdentity::of(context);
        self.pending()
            .get(&call_key(context))
            .map(|record| {
                record.check(context, &identity)?;
                record.pending_decision(context)
            })
            .transpose()
    }

    /// Keeps the first decision made for the same invocation, including concurrent checks.
    fn record(
        &self,
        context: &ToolContext<'_>,
        decision: CommandDecision,
    ) -> Result<CommandDecision> {
        let identity = CommandIdentity::of(context);
        let mut pending = self.pending();
        let record = pending.entry(call_key(context)).or_insert(CommandRecord {
            identity: identity.clone(),
            decision: Some(decision),
        });
        record.check(context, &identity)?;
        record.pending_decision(context)
    }

    fn begin_execution(
        &self,
        context: &ToolContext<'_>,
        decide: impl FnOnce() -> Result<CommandDecision>,
    ) -> Result<CommandDecision> {
        let identity = CommandIdentity::of(context);
        let mut pending = self.pending();
        if let Some(record) = pending.get_mut(&call_key(context)) {
            record.check(context, &identity)?;
            let decision = record.pending_decision(context)?;
            record.decision = None;
            return Ok(decision);
        }
        let decision = decide()?;
        pending.insert(
            call_key(context),
            CommandRecord {
                identity,
                decision: None,
            },
        );
        Ok(decision)
    }

    fn settle(&self, run_id: &RunId, call_id: &CallId) {
        if let Some(record) = self.pending().get_mut(&(run_id.clone(), call_id.clone())) {
            record.decision = None;
        }
    }

    fn finish_run(&self, run_id: &RunId) {
        self.pending().retain(|(run, _), _| run != run_id);
    }
}

fn call_key(context: &ToolContext<'_>) -> (RunId, CallId) {
    (context.run().run_id().clone(), context.call_id().clone())
}

/// What Codex's `exec_command` handler and command policy decide about the call `context` makes,
/// or the refusal the model reads.
///
/// The command is judged in Codex's form, `[shell, "-lc" or "-c", cmd]`, whatever argument layout
/// the executor then starts the shell with.
fn command_decision(
    context: &ToolContext<'_>,
    settings: &CodingThreadSettings,
) -> Result<CommandDecision> {
    let arguments: CommandArguments =
        serde_json::from_value(context.arguments().clone()).map_err(|error| {
            ra_tools::exec_command::rejection(format!("invalid arguments: {error}"))
        })?;
    if arguments.justification.is_some() && arguments.sandbox_permissions.is_none() {
        return Err(ra_tools::exec_command::rejection(
            JUSTIFICATION_WITHOUT_SANDBOX_PERMISSIONS,
        ));
    }
    let sandbox_permissions = arguments.sandbox_permissions.unwrap_or_default();
    let approval_policy = settings.approval_policy;
    // Codex formats the policy with `Debug`, which differs from Codex's only for `Granular`: this
    // build's variant carries its fields itself rather than in a `GranularApprovalConfig`.
    if sandbox_permissions.requests_sandbox_override()
        && prompt_is_rejected_by_policy(approval_policy, false).is_some()
    {
        return Err(ra_tools::exec_command::rejection(format!(
            "approval policy is {approval_policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {approval_policy:?}"
        )));
    }
    let command = vec![
        arguments.shell.unwrap_or_else(|| DEFAULT_SHELL.to_owned()),
        if arguments.login.unwrap_or(false) {
            "-lc"
        } else {
            "-c"
        }
        .to_owned(),
        arguments.cmd,
    ];
    let requirement = settings
        .exec_policy
        .create_exec_approval_requirement_for_shell(
            ExecApprovalRequest {
                command: &command,
                approval_policy,
                file_system_sandbox: FileSystemSandboxKind::of(&settings.sandbox_policy),
                sandbox_permissions,
                prefix_rule: arguments.prefix_rule.as_deref(),
                allow_prefix_rules: settings.allow_prefix_rules,
            },
            Path::new(DEFAULT_SHELL),
        );
    Ok(CommandDecision {
        requirement,
        sandbox_permissions,
    })
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
