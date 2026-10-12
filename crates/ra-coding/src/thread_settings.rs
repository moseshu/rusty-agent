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
//! this one. The prepared command, selected sandbox, environment configuration and approval flags
//! travel with the approval as the tool's resume state (see
//! [`ra_core::item::ToolResume`]), so it holds however and wherever the approval is answered.
//!
//! A sandbox refusal within Codex's 150 ms launch grace period follows its orchestrator: under
//! `Never` and `OnRequest` its output is the result; otherwise it runs again outside the sandbox, without
//! asking when the call was already approved and after the host approves it when it was not. That
//! approval is asked from inside the invocation (see [`ra_core::tool::ToolApprovalRequest`]), which
//! is the adaptation an interrupted run needs where Codex waits inside its orchestrator: the state
//! it keeps says the first attempt ran, so the approved call runs only the retry, with the decision
//! made before the first attempt. Later refusals are returned as output without replaying the
//! command, whose earlier operations may already have produced side effects.
//!
//! Each thread caches the approvals its host gives for the session, as Codex's session does (see
//! [`CodingThreadSettings::cache_review_decision`]): a command with the same key as an approved
//! one, or a patch touching only approved paths, runs without asking, and an approved command's
//! sandbox retry does not ask either. Configured permission rules decide before the cache is
//! consulted, as Codex's permission-request hooks decide before its cache.
//!
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

use crate::approvals::{
    ApprovalCacheKey, ApprovalStore, CommandApproval, CommandCacheKey, patch_cache_keys,
};
use crate::exec_policy::{
    AllowPrefixRules, ExecApprovalRequest, ExecApprovalRequirement, ExecPolicyManager,
    FileSystemSandboxKind, SandboxPermissions, prompt_is_rejected_by_policy,
};
use ra_core::{
    error::{Error, Result},
    item::{CallId, RunItemKind, ToolApproval},
    permission::PermissionMode,
    state::{ArgumentFingerprint, RunId},
    tool::{ToolContext, ToolLookupKey},
};
use ra_exec::output::ExecOutputSummary;
use ra_exec::sandbox::{
    ExecEnvironment, NetworkAccess as ExecNetworkAccess, SandboxLevel,
    SandboxPolicy as ExecSandboxPolicy,
    unix_local::{EnvPolicy, ResourceLimits},
};
use ra_protocol::server::{
    ReviewDecision, ThreadSettingsAdapter,
    protocol::{ApprovalsReviewer, AskForApproval, SandboxPolicy, ThreadPolicyOverrides},
};
use ra_runtime::{permission::PermissionEngine, runner::RunConfig, thread::ThreadEvent};
use ra_tools::exec_command::{ExecCommandAttempt, ExecCommandPolicy, SandboxDenialAction};
use serde::{Deserialize, Serialize};
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
    approvals: Arc<Mutex<ApprovalStore>>,
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

    /// Caches `decision` for the call `approval` asks about, as Codex's `with_cached_approval` does
    /// once its prompt is answered: approved for the session, each of the call's keys is approved
    /// for the rest of this thread, so a later command with the same key, or a patch touching only
    /// approved paths, does not ask. Any other decision, and an approval of another tool, is left
    /// alone.
    ///
    /// This only fills the thread's cache: the host still approves the call itself, once. A
    /// session approval is not the framework's sticky approval of the whole tool.
    pub fn cache_review_decision(&self, approval: &ToolApproval, decision: &ReviewDecision) {
        if approval.namespace().is_some() {
            return;
        }
        let arguments = approval.arguments();
        match approval.tool_name() {
            EXEC_COMMAND_TOOL => {
                let keys = self.command_cache_keys(arguments);
                self.approvals().remember(keys, decision);
            }
            APPLY_PATCH_TOOL => {
                let keys = self.patch_cache_keys(arguments);
                self.approvals().remember(keys, decision);
            }
            _ => {}
        }
    }

    fn approvals(&self) -> std::sync::MutexGuard<'_, ApprovalStore> {
        self.approvals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The keys Codex caches a command approval under: the command, where and how it runs, and the
    /// requirements policy it is decided under. Arguments the policy cannot read have none.
    fn command_cache_keys(&self, arguments: &Value) -> Vec<CommandCacheKey> {
        let Ok(arguments) = serde_json::from_value::<CommandArguments>(arguments.clone()) else {
            return Vec::new();
        };
        CommandApproval {
            command: &arguments.command(),
            cwd: self.command_cwd(arguments.workdir.as_deref()),
            tty: arguments.tty.unwrap_or(false),
            sandbox_permissions: arguments.sandbox_permissions.unwrap_or_default(),
        }
        .cache_keys(self.exec_policy.requirements_fingerprint())
    }

    /// The keys Codex caches a patch approval under, one per path it touches. A patch that does
    /// not parse has none.
    fn patch_cache_keys(&self, arguments: &Value) -> Vec<ApprovalCacheKey> {
        let patch = match arguments {
            Value::String(patch) => Some(patch.as_str()),
            Value::Object(arguments) => arguments.get("patch").and_then(Value::as_str),
            _ => None,
        };
        patch
            .and_then(|patch| ra_patch::parse_patch(patch).ok())
            .map(|plan| patch_cache_keys(&self.cwd, &plan))
            .unwrap_or_default()
    }

    /// The directory a command runs in: `workdir` resolved against the thread's workspace, as
    /// `exec_command` resolves it.
    fn command_cwd(&self, workdir: Option<&str>) -> PathBuf {
        workdir
            .map_or_else(|| self.cwd.clone(), |workdir| self.cwd.join(workdir))
            .components()
            .collect()
    }

    /// Whether the host approved every key for the session: the lookup half of Codex's
    /// `with_cached_approval`, made where the call would ask.
    fn approved_for_session<K: Serialize>(&self, keys: &[K]) -> bool {
        self.approvals().all_approved_for_session(keys)
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
                approvals: Arc::default(),
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
    ///
    /// Each thread initialized gets an approval cache of its own, as each Codex session has.
    pub fn initialize(&self, config: RunConfig) -> RunConfig {
        let mut settings = self.initial.clone();
        settings.base_permission = config.permission().clone();
        settings.approvals = Arc::default();
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
                initial.approvals = Arc::default();
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

/// The coding product's [`ExecCommandPolicy`]: Codex's command policy, and its orchestrator's
/// first attempt and retry after a sandbox denial, over the executor's `base` environment.
pub(crate) struct CodingCommandPolicy {
    base: ExecEnvironment,
}

impl CodingCommandPolicy {
    pub(crate) const fn new(base: ExecEnvironment) -> Self {
        Self { base }
    }

    fn environment(&self, decision: &CommandDecision) -> ExecEnvironment {
        let environment = self.base.clone().with_sandbox(decision.sandbox.clone());
        if decision.allow_temp_dir {
            environment
        } else {
            environment.without_temp_dir()
        }
    }
}

impl ExecCommandPolicy for CodingCommandPolicy {
    fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        command_approval(context)
    }

    fn approval_state(&self, context: &ToolContext<'_>) -> Result<Option<Value>> {
        let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
            return Ok(None);
        };
        // The decision the approval is asked about travels with it, so the approved call runs as
        // it was decided even when it resumes in another process.
        let decision = if let Some(decision) = settings.command_decisions.current(context)? {
            decision
        } else {
            let decision = command_decision(context, settings)?;
            settings.command_decisions.record(context, decision)?
        };
        let environment = FrozenEnvironment::of(&self.environment(&decision));
        encode(&CommandResume::Approval {
            decision,
            environment,
        })
        .map(Some)
    }

    fn attempt(&self, context: &ToolContext<'_>) -> Result<ExecCommandAttempt> {
        let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
            return Ok(ExecCommandAttempt::new(self.base.clone()));
        };
        let resume = context
            .resume_state()
            .map(|state| {
                serde_json::from_value::<CommandResume>(state.clone()).map_err(|error| {
                    Error::caller(format!(
                        "exec_command cannot resume from the state its approval kept: {error}"
                    ))
                })
            })
            .transpose()?;
        let (decision, already_approved, environment) = match resume {
            Some(CommandResume::SandboxRetry {
                decision,
                environment,
                ..
            }) => {
                settings.command_decisions.resume_retry(context)?;
                let state = AttemptState {
                    decision,
                    already_approved: true,
                    retry_environment: environment.clone(),
                };
                return Ok(ExecCommandAttempt::new(environment.restore(&self.base))
                    .with_state(encode(&state)?));
            }
            Some(CommandResume::Approval {
                decision,
                environment,
            }) => (
                settings
                    .command_decisions
                    .begin_execution(context, || Ok(decision))?,
                true,
                Some(environment.restore(&self.base)),
            ),
            None => (
                settings
                    .command_decisions
                    .begin_execution(context, || command_decision(context, settings))?,
                false,
                None,
            ),
        };
        if let ExecApprovalRequirement::Forbidden { reason } = &decision.requirement {
            return Err(ra_tools::exec_command::rejection(reason.clone()));
        }
        let environment = environment.unwrap_or_else(|| self.environment(&decision));
        let retry_environment = FrozenEnvironment::of(
            &environment
                .clone()
                .with_sandbox(ExecSandboxPolicy::new(SandboxLevel::Unconfined)),
        );
        let state = AttemptState {
            decision,
            already_approved,
            retry_environment,
        };
        Ok(ExecCommandAttempt::new(environment).with_state(encode(&state)?))
    }

    fn on_sandbox_denial(
        &self,
        context: &ToolContext<'_>,
        attempt: &ExecCommandAttempt,
        _output: &ExecOutputSummary,
    ) -> Result<SandboxDenialAction> {
        let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
            return Ok(SandboxDenialAction::Return);
        };
        let Some(state) = attempt.state() else {
            return Ok(SandboxDenialAction::Return);
        };
        let AttemptState {
            decision,
            already_approved,
            retry_environment,
        } = serde_json::from_value(state.clone()).map_err(|error| {
            Error::caller(format!(
                "exec_command lost the state of its attempt: {error}"
            ))
        })?;
        // Under `Never` or `OnRequest`, Codex does not retry without the sandbox: the refused
        // attempt's output is the result.
        if !wants_no_sandbox_approval(decision.approval_policy) {
            return Ok(SandboxDenialAction::Return);
        }
        // A call the host already approved is not asked about again, nor one whose command it
        // approved for the session, whether that ran this call without asking or not.
        settings.command_decisions.request_retry(context)?;
        if should_bypass_approval(decision.approval_policy, already_approved)
            || settings.approved_for_session(&settings.command_cache_keys(context.arguments()))
        {
            settings.command_decisions.resume_retry(context)?;
            return Ok(SandboxDenialAction::Retry(Box::new(
                retry_environment.restore(&self.base),
            )));
        }
        Ok(SandboxDenialAction::RequestApproval(encode(
            &CommandResume::SandboxRetry {
                decision,
                environment: retry_environment,
                retry_reason: SANDBOX_RETRY_REASON.to_owned(),
            },
        )?))
    }
}

/// The retry reason Codex's orchestrator gives the approval it asks after a sandbox denial.
const SANDBOX_RETRY_REASON: &str = "command failed; retry without sandbox?";

/// Whether the approval policy lets a refused command ask to run outside the sandbox: Codex's
/// `wants_no_sandbox_approval`.
const fn wants_no_sandbox_approval(policy: AskForApproval) -> bool {
    match policy {
        AskForApproval::UnlessTrusted => true,
        AskForApproval::Granular {
            sandbox_approval, ..
        } => sandbox_approval,
        _ => false,
    }
}

/// Whether a retry outside the sandbox runs without asking: Codex's `should_bypass_approval`.
const fn should_bypass_approval(policy: AskForApproval, already_approved: bool) -> bool {
    already_approved || matches!(policy, AskForApproval::Never)
}

/// The state the command tool keeps with an approval, by the point the call resumes from.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
enum CommandResume {
    /// Asked before the command ran, about the decision made then.
    Approval {
        decision: CommandDecision,
        environment: FrozenEnvironment,
    },
    /// The sandbox refused the first attempt, and the host is asked to run it outside.
    SandboxRetry {
        decision: CommandDecision,
        environment: FrozenEnvironment,
        retry_reason: String,
    },
}

/// What one attempt was decided with, for the retry decision after a sandbox denial.
#[derive(Debug, Serialize, Deserialize)]
struct AttemptState {
    decision: CommandDecision,
    already_approved: bool,
    retry_environment: FrozenEnvironment,
}

/// Serializable execution configuration. Scratch directory handles belong to the live runtime;
/// restoration binds its replacement handle only when the approved attempt allowed one.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenEnvironment {
    env_policy: EnvPolicy,
    limits: ResourceLimits,
    sandbox: Option<ExecSandboxPolicy>,
    allow_temp_dir: bool,
}

impl FrozenEnvironment {
    fn of(environment: &ExecEnvironment) -> Self {
        Self {
            env_policy: environment.env_policy().clone(),
            limits: environment.resource_limits().clone(),
            sandbox: environment.sandbox().cloned(),
            allow_temp_dir: environment.temp_dir().is_some(),
        }
    }

    fn restore(&self, base: &ExecEnvironment) -> ExecEnvironment {
        let mut environment = ExecEnvironment::new()
            .with_env_policy(self.env_policy.clone())
            .with_resource_limits(self.limits.clone());
        if let Some(sandbox) = &self.sandbox {
            environment = environment.with_sandbox(sandbox.clone());
        }
        if self.allow_temp_dir
            && let Some(temp_dir) = base.temp_dir()
        {
            environment = environment.with_temp_dir(Arc::clone(temp_dir));
        }
        environment
    }
}

fn encode(value: &impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|error| {
        Error::caller(format!(
            "exec_command could not record its decision: {error}"
        ))
    })
}

/// The sandboxed environment the thread's sandbox policy lowers to.
fn sandboxed_environment(
    settings: &CodingThreadSettings,
    base: &ExecEnvironment,
) -> ExecEnvironment {
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
    environment.with_sandbox(policy)
}

fn network(enabled: bool) -> ExecNetworkAccess {
    if enabled {
        ExecNetworkAccess::Allowed
    } else {
        ExecNetworkAccess::Denied
    }
}

fn command_approval(context: &ToolContext<'_>) -> Result<bool> {
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
    // A command the host approved for the session does not ask again.
    let needs_approval = matches!(
        decision.requirement,
        ExecApprovalRequirement::NeedsApproval { .. }
    ) && !settings
        .approved_for_session(&settings.command_cache_keys(context.arguments()));
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
    workdir: Option<String>,
    shell: Option<String>,
    tty: Option<bool>,
    login: Option<bool>,
    sandbox_permissions: Option<SandboxPermissions>,
    justification: Option<String>,
    prefix_rule: Option<Vec<String>>,
}

impl CommandArguments {
    /// The command in Codex's form, `[shell, "-lc" or "-c", cmd]`, whatever argument layout the
    /// executor then starts the shell with.
    fn command(&self) -> Vec<String> {
        vec![
            self.shell
                .clone()
                .unwrap_or_else(|| DEFAULT_SHELL.to_owned()),
            if self.login.unwrap_or(false) {
                "-lc"
            } else {
                "-c"
            }
            .to_owned(),
            self.cmd.clone(),
        ]
    }
}

/// The tools whose approvals the thread caches, by the names the coding host installs them under.
const EXEC_COMMAND_TOOL: &str = "exec_command";
const APPLY_PATCH_TOOL: &str = "apply_patch";

/// What was decided about one command call, kept from its approval to its execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CommandDecision {
    requirement: ExecApprovalRequirement,
    sandbox_permissions: SandboxPermissions,
    approval_policy: AskForApproval,
    sandbox: ExecSandboxPolicy,
    allow_temp_dir: bool,
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
    retry: RetryPhase,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum RetryPhase {
    #[default]
    NotRequested,
    Waiting,
    Started,
    Settled,
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
            retry: RetryPhase::NotRequested,
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
                retry: RetryPhase::NotRequested,
            },
        );
        Ok(decision)
    }

    /// Records the retry boundary reached by the first attempt.
    fn request_retry(&self, context: &ToolContext<'_>) -> Result<()> {
        let identity = CommandIdentity::of(context);
        let mut pending = self.pending();
        let record = pending
            .get_mut(&call_key(context))
            .ok_or_else(|| Error::caller("the command has no first attempt to retry"))?;
        record.check(context, &identity)?;
        if record.retry != RetryPhase::NotRequested {
            return Err(Error::caller(
                "the command already requested its sandbox retry",
            ));
        }
        record.retry = RetryPhase::Waiting;
        Ok(())
    }

    /// Consumes approval before retry execution, including failures and cancellation. A fresh
    /// process can restore a checkpoint once; a retained invocation cannot execute it twice.
    fn resume_retry(&self, context: &ToolContext<'_>) -> Result<()> {
        let identity = CommandIdentity::of(context);
        let mut pending = self.pending();
        if let Some(record) = pending.get_mut(&call_key(context)) {
            record.check(context, &identity)?;
            if record.retry != RetryPhase::Waiting {
                return Err(Error::caller(
                    "the command's sandbox retry has already started or settled",
                ));
            }
            record.retry = RetryPhase::Started;
            return Ok(());
        }
        pending.insert(
            call_key(context),
            CommandRecord {
                identity,
                decision: None,
                retry: RetryPhase::Started,
            },
        );
        Ok(())
    }

    fn settle(&self, run_id: &RunId, call_id: &CallId) {
        if let Some(record) = self.pending().get_mut(&(run_id.clone(), call_id.clone())) {
            record.decision = None;
            record.retry = RetryPhase::Settled;
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
    let command = arguments.command();
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
    let bypass_sandbox = matches!(
        requirement,
        ExecApprovalRequirement::Skip {
            bypass_sandbox: true,
            ..
        }
    ) || sandbox_permissions.requires_escalated_permissions();
    let selected = if bypass_sandbox {
        ExecEnvironment::new().with_sandbox(ExecSandboxPolicy::new(SandboxLevel::Unconfined))
    } else {
        sandboxed_environment(settings, &ExecEnvironment::new())
    };
    Ok(CommandDecision {
        requirement,
        sandbox_permissions,
        approval_policy,
        sandbox: selected
            .sandbox()
            .cloned()
            .ok_or_else(|| Error::caller("the coding adapter did not select a command sandbox"))?,
        allow_temp_dir: bypass_sandbox
            || matches!(
                settings.sandbox_policy,
                SandboxPolicy::DangerFullAccess
                    | SandboxPolicy::ExternalSandbox { .. }
                    | SandboxPolicy::WorkspaceWrite { .. }
            ),
    })
}

/// Whether a patch asks before it is applied: Codex's patch safety check, and its approval cache
/// when it would ask.
pub(crate) fn patch_approval(context: &ToolContext<'_>) -> Result<bool> {
    let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
        return Ok(true);
    };
    // A patch touching only paths the host approved for the session does not ask again.
    Ok(patch_asks(settings)?
        && !settings.approved_for_session(&settings.patch_cache_keys(context.arguments())))
}

fn patch_asks(settings: &CodingThreadSettings) -> Result<bool> {
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
