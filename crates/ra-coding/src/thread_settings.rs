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
//! explicit embedding-host responsibility. The client is the only approval reviewer: the server
//! refuses Codex's automatic reviewer, which is not ported, and a snapshot that names it anyway
//! asks the client nothing, so its calls are refused.
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
//! The adapter also asks the client about the calls a run stops on, as Codex's app-server asks for
//! its shell and patch tools: a command with Codex's `item/commandExecution/requestApproval`, in
//! the form it is decided and cached in, with the reason and command-prefix rule its kept decision
//! gives; a patch with `item/fileChange/requestApproval`. An answer is applied to the thread's
//! approval cache and, approving a rule, to its rules home, whose failure is the user's warning.
//! Product MCP calls use the source form elicitation or blocking question. Only Auto decisions
//! can populate their server/plugin/tool session cache; Prompt and Writes stay invocation-local.
//!
//! Permissions `request_permissions` granted the turn or the session are added to every later
//! command that stays in the sandbox, as Codex's `apply_granted_turn_permissions` adds them: having
//! been approved when they were given, they do not make the command ask; its sandbox gains the
//! roots they write and read and, when they give it, the network; and its approval request and
//! cache keys carry them. The executor's sandboxes can neither exclude a path from a root they open
//! nor deny a read, so a command is refused when a grant denies a path or makes a subtree of a
//! writable root read-only. Path precedence includes both baseline roots and all grants; an
//! equally specific write overrides a read, as Codex's policy does.
//!
//! A command may also ask for permissions of its own with `sandbox_permissions:
//! "with_additional_permissions"` and `additional_permissions`, once the host turns on Codex's
//! `exec_permission_approvals` feature (see [`crate::host::CodingHost::with_exec_permission_approvals`]).
//! Its paths are read relative to the directory it runs in and checked as Codex's handler checks
//! them; merged with the grants, they widen its sandbox, and the command asks unless the grants
//! already give everything it asked for.
//!
//! Input to a running terminal follows Codex's stdin review: a command's launch records the
//! sandbox it runs in, whether that left the sandbox its decision selected, and the grants it ran
//! with, and input asks first only when the terminal runs with authority a command launched now
//! would not get, as a command approval of kind `writeStdin`.
//!
//! Invocation identity covers the tool and complete arguments. Settled calls retain only their
//! identity until the turn ends, preventing call-ID reuse. The app-server listener forwards events
//! to [`ThreadSettingsAdapter::on_thread_event`] for cleanup; an embedding host loading threads
//! outside that server must forward their events to the adapter itself.
//!
//! Restricted in-process patches use Codex's unavailable-patch-sandbox approval path: this writer
//! has directory capabilities, but does not run beneath the command executor's kernel sandbox. A
//! patch the grants already let write every path it touches is preapproved and does not ask, as
//! Codex's patch handler preapproves it.
//! Commands do enter that sandbox. The existing executor's workspace policy grants entire writable
//! roots; it does not yet port Codex's nested protected-directory rules. Those remain an execution
//! adapter parity gap rather than a new provider-neutral permission rule.

use crate::additional_permissions::{
    EffectiveAdditionalPermissions, apply_granted_turn_permissions, granted_sandbox,
    implicit_granted_permissions, normalize_and_validate_additional_permissions, patch_preapproved,
};
use crate::approvals::{
    ApprovalCacheKey, ApprovalStore, CommandApproval, CommandCacheKey, LOCAL_ENVIRONMENT_ID,
    patch_cache_keys,
};
use crate::exec_policy::{
    AllowPrefixRules, ExecApprovalRequest, ExecApprovalRequirement, ExecPolicyManager,
    FileSystemSandboxKind, SandboxPermissions, prompt_is_rejected_by_policy,
};
use crate::permissions::{
    AbsolutePathBufGuard, AdditionalPermissionProfile, FileSystemSandboxPolicyContext,
};
use crate::request_permissions::{PolicyPaths, ThreadPermissions, wire_additional_profile};
use ra_core::{
    error::{Error, Result},
    item::{CallId, RunItemKind, ToolApproval},
    state::{ArgumentFingerprint, RunId},
    tool::{Tool, ToolContext, ToolLookupKey},
};
use ra_exec::output::ExecOutputSummary;
use ra_exec::sandbox::{
    ExecEnvironment, NetworkAccess as ExecNetworkAccess, SandboxLevel,
    SandboxPolicy as ExecSandboxPolicy,
    unix_local::{EnvPolicy, ResourceLimits},
};
use ra_exec::session::BackgroundTerminal;
use ra_protocol::server::{
    PendingApproval, ReviewDecision, ThreadServerRequests, ThreadSettingsAdapter,
    protocol::{
        ApprovalsReviewer, AskForApproval, CommandAction, CommandExecutionApprovalDecision,
        CommandExecutionApprovalKind, CommandExecutionRequestApprovalParams,
        FileChangeRequestApprovalParams, SandboxPolicy, ServerRequestPayload,
        ThreadPolicyOverrides,
    },
};
use ra_runtime::{permission::PermissionEngine, runner::RunConfig, thread::ThreadEvent};
use ra_tools::{
    exec_command::{ExecCommandAttempt, ExecCommandPolicy, SandboxDenialAction},
    write_stdin::{StdinReview, WriteStdinPolicy},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

mod product;
mod profile;

pub use product::ProductPermissionSource;

/// A policy captured by one model step and its tools.
#[derive(Clone, Debug)]
pub struct CodingThreadSettings {
    pub(crate) mcp_approvals: Arc<Mutex<std::collections::BTreeSet<crate::mcp::McpApprovalKey>>>,
    cwd: PathBuf,
    approval_policy: AskForApproval,
    approvals_reviewer: ApprovalsReviewer,
    /// The legacy projection of `profile_permissions` reported on the wire and lowered to the
    /// executor.
    sandbox_policy: SandboxPolicy,
    /// The canonical permission state: Codex's session `Permissions`, with the requirement
    /// constraints of the configuration the thread was loaded with.
    profile_permissions: crate::config::Permissions,
    active_permission_profile: Option<ra_filesystem::models::ActivePermissionProfile>,
    file_system_read_policy: Option<crate::permissions::FileSystemPermissions>,
    base_permission: PermissionEngine,
    exec_policy: Arc<ExecPolicyManager>,
    allow_prefix_rules: AllowPrefixRules,
    rules_home: Option<PathBuf>,
    command_decisions: Arc<CommandDecisions>,
    approvals: Arc<Mutex<ApprovalStore>>,
    permissions: Arc<ThreadPermissions>,
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
                let kept = approval
                    .resume()
                    .and_then(|resume| {
                        ra_tools::exec_command::approval_policy_state(resume.state())
                    })
                    .and_then(|state| serde_json::from_value::<CommandResume>(state.clone()).ok());
                let keys =
                    self.command_cache_keys(arguments, kept.as_ref().map(CommandResume::decision));
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

    /// The keys Codex caches a command approval under: the command, where and how it runs, with
    /// the permissions `decision` gave it, and the requirements policy it is decided under.
    /// Arguments the policy cannot read have none.
    fn command_cache_keys(
        &self,
        arguments: &Value,
        decision: Option<&CommandDecision>,
    ) -> Vec<CommandCacheKey> {
        let Ok(arguments) = serde_json::from_value::<CommandArguments>(arguments.clone()) else {
            return Vec::new();
        };
        CommandApproval {
            command: &arguments.command(),
            cwd: self.command_cwd(arguments.workdir.as_deref()),
            tty: arguments.tty.unwrap_or(false),
            sandbox_permissions: decision.map_or_else(
                || arguments.sandbox_permissions.unwrap_or_default(),
                |decision| decision.sandbox_permissions,
            ),
            additional_permissions: decision
                .and_then(|decision| decision.additional_permissions.as_ref()),
        }
        .cache_keys(self.exec_policy.requirements_fingerprint())
    }

    /// Whether the grants preapprove the patch `context` applies: Codex's patch handler adds them
    /// with `apply_granted_turn_permissions`. A patch that does not parse is not preapproved.
    fn patch_preapproved(&self, context: &ToolContext<'_>) -> bool {
        let Some(plan) =
            patch_text(context.arguments()).and_then(|patch| ra_patch::parse_patch(patch).ok())
        else {
            return false;
        };
        let paths: Vec<PathBuf> = plan
            .actions()
            .iter()
            .flat_map(ra_patch::PatchAction::target_paths)
            .map(|path| self.cwd.join(path).components().collect())
            .collect();
        let sandbox = sandboxed_environment(self, &ExecEnvironment::new());
        let policy_paths = PolicyPaths::new(&self.cwd);
        patch_preapproved(
            context,
            self,
            sandbox.sandbox(),
            &paths,
            &policy_paths.context(),
        )
    }

    /// The keys Codex caches a patch approval under, one per path it touches. A patch that does
    /// not parse has none.
    fn patch_cache_keys(&self, arguments: &Value) -> Vec<ApprovalCacheKey> {
        patch_text(arguments)
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

    /// The request asking the client about `pending`: Codex's `ExecApprovalRequest` for a command
    /// and `ApplyPatchApprovalRequest` for a patch, as its app-server sends them. Other tools are
    /// asked nothing.
    fn approval_request(&self, pending: PendingApproval<'_>) -> Option<ServerRequestPayload> {
        // The server refuses an automatic reviewer, which is not ported; a snapshot that still
        // names one asks nobody, so the call is refused rather than put to the client instead.
        if self.approvals_reviewer == ApprovalsReviewer::AutoReview {
            return None;
        }
        let approval = pending.approval();
        if let Some(state) = crate::mcp::McpApprovalState::from_approval(approval) {
            return Some(state.request(pending));
        }
        if approval.namespace().is_some() {
            return None;
        }
        match approval.tool_name() {
            EXEC_COMMAND_TOOL => self.command_approval_request(pending),
            WRITE_STDIN_TOOL => stdin_approval_request(pending),
            // A patch is asked about with no reason and no grant root, as Codex asks one its
            // patch safety check sends to the user.
            APPLY_PATCH_TOOL => Some(ServerRequestPayload::FileChangeRequestApproval(
                FileChangeRequestApprovalParams::new(
                    pending.thread_id(),
                    pending.turn_id(),
                    approval.call_id().as_str(),
                    pending.started_at_ms(),
                ),
            )),
            _ => None,
        }
    }

    /// Codex's command approval request: the command in the form it is decided and cached in,
    /// where it runs, why it asks — the retry reason, then the reason its decision gives, then the
    /// call's justification — the permissions it runs with beyond the sandbox's, the command-prefix
    /// rule its decision proposes, and the decisions Codex offers by default, which are only
    /// approving once and cancelling for a command with permissions beyond the sandbox's. The
    /// decision is the one kept with the approval, so the request
    /// says what the approved call will run as, wherever the approval is answered.
    fn command_approval_request(
        &self,
        pending: PendingApproval<'_>,
    ) -> Option<ServerRequestPayload> {
        let approval = pending.approval();
        let arguments =
            serde_json::from_value::<CommandArguments>(approval.arguments().clone()).ok()?;
        let resume = approval
            .resume()
            .and_then(|resume| ra_tools::exec_command::approval_policy_state(resume.state()))
            .and_then(|state| serde_json::from_value::<CommandResume>(state.clone()).ok());
        let (requirement, additional_permissions, retry_reason) = match resume {
            Some(CommandResume::Approval { decision, .. }) => (
                Some(decision.requirement),
                decision.additional_permissions,
                None,
            ),
            Some(CommandResume::SandboxRetry {
                decision,
                retry_reason,
                ..
            }) => (
                Some(decision.requirement),
                decision.additional_permissions,
                Some(retry_reason),
            ),
            None => (None, None, None),
        };
        let additional_permissions = additional_permissions.as_ref().map(wire_additional_profile);
        let approval_reason = match &requirement {
            Some(ExecApprovalRequirement::NeedsApproval { reason, .. }) => reason.clone(),
            _ => None,
        };
        let reason = retry_reason
            .or(approval_reason)
            .or_else(|| arguments.justification.clone());
        let amendment = requirement
            .as_ref()
            .and_then(ExecApprovalRequirement::proposed_execpolicy_amendment)
            .cloned();
        let mut params = CommandExecutionRequestApprovalParams::new(
            pending.thread_id(),
            pending.turn_id(),
            approval.call_id().as_str(),
            pending.started_at_ms(),
        )
        .with_environment_id(LOCAL_ENVIRONMENT_ID)
        .with_command(shlex_join(&arguments.command()))
        .with_cwd(
            self.command_cwd(arguments.workdir.as_deref())
                .to_string_lossy(),
        )
        .with_command_actions(vec![CommandAction::unknown(arguments.cmd.clone())])
        .with_available_decisions(CommandExecutionApprovalDecision::default_available(
            None,
            amendment.as_ref(),
            None,
            additional_permissions.as_ref(),
        ));
        if let Some(reason) = reason {
            params = params.with_reason(reason);
        }
        if let Some(additional_permissions) = additional_permissions {
            params = params.with_additional_permissions(additional_permissions);
        }
        if let Some(amendment) = amendment {
            params = params.with_proposed_execpolicy_amendment(amendment);
        }
        Some(ServerRequestPayload::CommandExecutionRequestApproval(
            params,
        ))
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

    /// The canonical permission state: the profile, its constraints and the active named
    /// profile, from which [`Self::sandbox_policy`] is projected.
    #[must_use]
    pub const fn permissions_state(&self) -> &crate::config::Permissions {
        &self.profile_permissions
    }

    /// The named profile the thread runs with, if one is selected.
    #[must_use]
    pub const fn active_permission_profile(
        &self,
    ) -> Option<&ra_filesystem::models::ActivePermissionProfile> {
        self.active_permission_profile.as_ref()
    }

    /// The thread's working directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// What `request_permissions` granted the session: Codex's `granted_session_permissions`.
    #[must_use]
    pub fn granted_session_permissions(&self) -> Option<AdditionalPermissionProfile> {
        self.permissions.session()
    }

    /// What `request_permissions` granted the turn `run_id` while it runs: Codex's
    /// `granted_turn_permissions`.
    #[must_use]
    pub fn granted_turn_permissions(&self, run_id: &RunId) -> Option<AdditionalPermissionProfile> {
        self.permissions.turn(run_id)
    }

    pub(crate) fn file_system_read_policy(
        &self,
    ) -> Option<&crate::permissions::FileSystemPermissions> {
        self.file_system_read_policy.as_ref()
    }

    pub(crate) fn thread_permissions(&self) -> &ThreadPermissions {
        &self.permissions
    }
}

/// Host-selected permission catalog. Unknown profiles are rejected without changing the thread.
#[derive(Clone, Debug)]
pub struct CodingThreadSettingsAdapter {
    initial: CodingThreadSettings,
    profiles: BTreeMap<String, SandboxPolicy>,
    file_system_read_profiles: BTreeMap<String, crate::permissions::FileSystemPermissions>,
    /// The product configuration named profiles are resolved through, as Codex resolves them by
    /// reloading its configuration; `None` for a host that supplies its catalog directly.
    product: Option<Arc<ProductPermissionSource>>,
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
        let profile_permissions =
            profile::unconstrained_permissions(AskForApproval::default(), &sandbox_policy, &cwd)
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
                profile_permissions,
                approval_policy: AskForApproval::default(),
                approvals_reviewer: ApprovalsReviewer::default(),
                active_permission_profile: None,
                file_system_read_policy: None,
                base_permission: PermissionEngine::default(),
                exec_policy: Arc::new(ExecPolicyManager::default()),
                allow_prefix_rules: AllowPrefixRules::default(),
                rules_home: None,
                command_decisions: Arc::new(CommandDecisions::default()),
                approvals: Arc::default(),
                permissions: Arc::default(),
                mcp_approvals: Arc::default(),
            },
            profiles,
            file_system_read_profiles: BTreeMap::new(),
            product: None,
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
        let name = name.into();
        self.file_system_read_profiles.remove(&name);
        self.profiles.insert(name, policy);
        Ok(self)
    }

    /// Sets restricted read entries for in-process executor filesystem operations.
    /// Legacy command sandboxing remains selected separately by `sandbox_policy`.
    /// With no entries every live skill read is denied; discovery materializations remain usable.
    /// This product filter does not restrict shell or ordinary filesystem tools.
    pub fn with_file_system_read_policy(
        mut self,
        policy: crate::permissions::FileSystemPermissions,
    ) -> Result<Self> {
        self.validate_file_system_read_policy(&policy)?;
        self.initial.file_system_read_policy = Some(policy);
        Ok(self)
    }

    /// Registers the restricted read component of a named thread permission profile.
    /// The corresponding legacy command policy must already be registered with `with_profile`.
    pub fn with_file_system_read_profile(
        mut self,
        name: impl Into<String>,
        policy: crate::permissions::FileSystemPermissions,
    ) -> Result<Self> {
        let name = name.into();
        if !self.profiles.contains_key(&name) {
            return Err(Error::config(format!(
                "unknown permissions profile: {name}"
            )));
        }
        self.validate_file_system_read_policy(&policy)?;
        self.file_system_read_profiles.insert(name, policy);
        Ok(self)
    }

    fn validate_file_system_read_policy(
        &self,
        policy: &crate::permissions::FileSystemPermissions,
    ) -> Result<()> {
        let paths = PolicyPaths::new(&self.initial.cwd);
        crate::permissions::normalize_additional_permissions_with_context(
            AdditionalPermissionProfile::new().with_file_system(policy.clone()),
            &paths.context(),
        )
        .map_err(Error::config)?;
        Ok(())
    }

    /// Installs the explicitly selected product snapshot on a framework configuration.
    ///
    /// Each thread initialized gets an approval cache and permission grants of its own, as each
    /// Codex session has.
    pub fn initialize(&self, config: RunConfig) -> RunConfig {
        let mut settings = self.initial.clone();
        settings.base_permission = config.permission().clone();
        settings.approvals = Arc::default();
        settings.permissions = Arc::default();
        settings.mcp_approvals = Arc::default();
        install(config, settings)
    }

    /// Applies the policy fields every adapter resolves the same way: the approval policy and
    /// reviewer, a legacy sandbox policy, and — for a host-supplied catalog — a named profile.
    /// Each change is checked against the thread's requirement constraints first, as Codex's
    /// session previews its settings, so a refused update leaves the thread as it was.
    fn prepare_settings(
        &self,
        mut settings: CodingThreadSettings,
        update: &ThreadPolicyOverrides,
    ) -> std::result::Result<CodingThreadSettings, String> {
        if let Some(policy) = update.approval_policy() {
            settings
                .profile_permissions
                .approval_policy
                .set(policy)
                .map_err(|error| error.to_string())?;
            settings.approval_policy = policy;
        }
        if let Some(reviewer) = update.approvals_reviewer() {
            settings.approvals_reviewer = reviewer;
        }
        if let Some(policy) = update.sandbox_policy() {
            profile::set_session_legacy_policy(
                &mut settings.profile_permissions,
                policy,
                &settings.cwd,
            )?;
            settings.sandbox_policy = policy.clone();
            settings.active_permission_profile = None;
            settings.file_system_read_policy = None;
        }
        if self.product.is_none() {
            if let Some(profile) = update.permissions() {
                self.select_catalog_profile(&mut settings, profile)?;
            } else if let Some(profile) = update.persisted_permissions() {
                // A recorded profile the catalog no longer defines, or that the thread's
                // constraints now refuse, leaves the configured default in place, as Codex's
                // stale `persisted_permission_profile_id` does.
                let _ = self.select_catalog_profile(&mut settings, profile);
            }
        }
        Ok(settings)
    }

    /// Selects `profile` from the host-supplied catalog; the settings change only on success.
    fn select_catalog_profile(
        &self,
        settings: &mut CodingThreadSettings,
        profile: &str,
    ) -> std::result::Result<(), String> {
        let policy = self
            .profiles
            .get(profile)
            .ok_or_else(|| format!("unknown permissions profile: {profile}"))?;
        profile::select_named_legacy_profile(
            &mut settings.profile_permissions,
            profile,
            policy,
            &settings.cwd,
        )?;
        settings.sandbox_policy = policy.clone();
        settings.active_permission_profile =
            Some(ra_filesystem::models::ActivePermissionProfile::new(profile));
        settings.file_system_read_policy = self.file_system_read_profiles.get(profile).cloned();
        Ok(())
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
                initial.permissions = Arc::default();
                initial
            })
    }
}

#[async_trait::async_trait]
impl ThreadSettingsAdapter for CodingThreadSettingsAdapter {
    fn prepare(
        &self,
        current: &RunConfig,
        update: &ThreadPolicyOverrides,
    ) -> std::result::Result<RunConfig, String> {
        update.validate()?;
        if self.product.is_some()
            && (update.permissions().is_some() || update.persisted_permissions().is_some())
        {
            return Err(
                "named permissions are resolved from the product configuration; prepare them asynchronously"
                    .to_owned(),
            );
        }
        let settings = self.prepare_settings(self.snapshot(current), update)?;
        Ok(install(current.clone(), settings))
    }

    async fn prepare_async(
        &self,
        current: &RunConfig,
        update: &ThreadPolicyOverrides,
    ) -> std::result::Result<RunConfig, String> {
        update.validate()?;
        let mut settings = self.snapshot(current);
        let Some(product) = &self.product else {
            return self.prepare(current, update);
        };
        product.select_named_profile(&mut settings, update).await?;
        let settings = self.prepare_settings(settings, update)?;
        Ok(install(current.clone(), settings))
    }

    fn describe(&self, config: &RunConfig) -> Value {
        let settings = self.snapshot(config);
        json!({"cwd": settings.cwd, "approvalPolicy": settings.approval_policy,
            "approvalsReviewer": settings.approvals_reviewer, "sandboxPolicy": settings.sandbox_policy,
            "activePermissionProfile": settings.active_permission_profile.map(|profile| json!({"id": profile.id, "extends": profile.extends})),
            "permissionProfile": serde_json::to_value(settings.profile_permissions.permission_profile()).unwrap_or(Value::Null),
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
                settings.permissions.finish_turn(&RunId::new(turn_id));
            }
            _ => {}
        }
    }

    fn attach_server_requests(&self, config: &RunConfig, requests: ThreadServerRequests) {
        if let Some(settings) = config
            .app_context()
            .and_then(|value| value.downcast_ref::<CodingThreadSettings>())
        {
            settings.permissions.attach(requests);
        }
    }

    fn approval_request(
        &self,
        config: &RunConfig,
        pending: PendingApproval<'_>,
    ) -> Option<ServerRequestPayload> {
        config
            .app_context()
            .and_then(|value| value.downcast_ref::<CodingThreadSettings>())?
            .approval_request(pending)
    }

    /// Commands and patches are asked about whatever kept state their approval carries. Input to
    /// a running terminal is asked about only with the review its stdin policy keeps, which a
    /// `write_stdin` that asks before every input does not have.
    fn requests_approval_for(&self, tool: &dyn Tool) -> bool {
        tool.origin().namespace().is_none()
            && matches!(tool.origin().name(), EXEC_COMMAND_TOOL | APPLY_PATCH_TOOL)
    }

    async fn apply_review_decision(
        &self,
        config: &RunConfig,
        approval: &ToolApproval,
        decision: &ReviewDecision,
    ) -> Option<String> {
        let settings = config
            .app_context()
            .and_then(|value| value.downcast_ref::<CodingThreadSettings>())?;
        if let Some(state) = crate::mcp::McpApprovalState::from_approval(approval) {
            state.apply(settings, decision).await;
            return None;
        }
        settings.cache_review_decision(approval, decision);
        settings
            .persist_approved_execpolicy_amendment(decision)
            .await
    }
}

fn install(config: RunConfig, settings: CodingThreadSettings) -> RunConfig {
    // Never disables framework prompts without changing the selected class policy or rules.
    let permission = if matches!(settings.approval_policy, AskForApproval::Never) {
        settings.base_permission.clone().with_prompts(false)
    } else {
        settings.base_permission.clone()
    };
    config
        .with_permission_engine(permission)
        .with_app_context(Arc::new(settings))
}

/// The coding product's [`ExecCommandPolicy`]: Codex's command policy, and its orchestrator's
/// first attempt and retry after a sandbox denial, over the executor's `base` environment.
pub(crate) struct CodingCommandPolicy {
    base: ExecEnvironment,
    exec_permission_approvals: bool,
}

impl CodingCommandPolicy {
    pub(crate) const fn new(base: ExecEnvironment) -> Self {
        Self {
            base,
            exec_permission_approvals: false,
        }
    }

    /// Lets a call ask for permissions of its own with `with_additional_permissions`: Codex's
    /// `exec_permission_approvals` feature.
    pub(crate) const fn with_exec_permission_approvals(mut self) -> Self {
        self.exec_permission_approvals = true;
        self
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
        command_approval(context, self.exec_permission_approvals)
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
            let decision = command_decision(context, settings, self.exec_permission_approvals)?;
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
                settings.command_decisions.begin_execution(context, || {
                    command_decision(context, settings, self.exec_permission_approvals)
                })?,
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

    fn launch_state(
        &self,
        attempt: &ExecCommandAttempt,
        environment: &ExecEnvironment,
    ) -> Option<Value> {
        let AttemptState { decision, .. } =
            serde_json::from_value(attempt.state()?.clone()).ok()?;
        // Codex's launch permissions: a launch that left the sandbox its decision selected, by an
        // escalation or a rule's trust before it ran or by a retry after a denial.
        let escalated =
            decision.bypasses_sandbox() || environment.sandbox() != Some(&decision.sandbox);
        encode(&TerminalLaunch {
            sandbox: environment.sandbox().cloned(),
            escalated,
            additional_permissions: decision.additional_permissions,
        })
        .ok()
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
            || settings.approved_for_session(
                &settings.command_cache_keys(context.arguments(), Some(&decision)),
            )
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

impl CommandResume {
    /// The decision the call was asked about with.
    const fn decision(&self) -> &CommandDecision {
        match self {
            Self::Approval { decision, .. } | Self::SandboxRetry { decision, .. } => decision,
        }
    }
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

fn command_approval(context: &ToolContext<'_>, exec_permission_approvals: bool) -> Result<bool> {
    let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
        return Ok(true);
    };
    // Only a command that needs approval asks. A forbidden one, or one whose arguments Codex's
    // handler refuses, is refused when it is called, with the reason the model reads in place of
    // its result.
    let decision = if let Some(decision) = settings.command_decisions.current(context)? {
        decision
    } else {
        let Ok(decision) = command_decision(context, settings, exec_permission_approvals) else {
            return Ok(false);
        };
        settings.command_decisions.record(context, decision)?
    };
    // A command the host approved for the session does not ask again.
    let needs_approval = matches!(
        decision.requirement,
        ExecApprovalRequirement::NeedsApproval { .. }
    ) && !settings
        .approved_for_session(&settings.command_cache_keys(context.arguments(), Some(&decision)));
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
    /// Read where the command's directory is known, so relative paths resolve against it.
    additional_permissions: Option<Value>,
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

/// A command as Codex's app-server shows it in an approval request: Codex's `shlex_join`.
fn shlex_join(tokens: &[String]) -> String {
    shlex::try_join(tokens.iter().map(String::as_str))
        .unwrap_or_else(|_| "<command included NUL byte>".to_string())
}

/// The patch an `apply_patch` call carries, as a string or as its `patch` argument.
fn patch_text(arguments: &Value) -> Option<&str> {
    match arguments {
        Value::String(patch) => Some(patch.as_str()),
        Value::Object(arguments) => arguments.get("patch").and_then(Value::as_str),
        _ => None,
    }
}

/// The tools whose approvals the thread caches, by the names the coding host installs them under.
const EXEC_COMMAND_TOOL: &str = "exec_command";
const WRITE_STDIN_TOOL: &str = "write_stdin";
const APPLY_PATCH_TOOL: &str = "apply_patch";

/// What was decided about one command call, kept from its approval to its execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CommandDecision {
    requirement: ExecApprovalRequirement,
    /// The call's sandbox permissions once the grants are added: `with_additional_permissions`
    /// when they add any.
    sandbox_permissions: SandboxPermissions,
    /// What `request_permissions` granted the turn and the session when the call was decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    additional_permissions: Option<AdditionalPermissionProfile>,
    approval_policy: AskForApproval,
    sandbox: ExecSandboxPolicy,
    allow_temp_dir: bool,
}

impl CommandDecision {
    /// Whether the command runs outside the thread's sandbox from its first attempt: a rule
    /// trusts it, or the call asked to leave the sandbox.
    fn bypasses_sandbox(&self) -> bool {
        matches!(
            self.requirement,
            ExecApprovalRequirement::Skip {
                bypass_sandbox: true,
                ..
            }
        ) || self.sandbox_permissions.requires_escalated_permissions()
    }
}

/// What a command's launch recorded with the session it leaves running: the sandbox it runs in,
/// whether that is outside the sandbox its decision selected, and the grants it was launched with
/// — Codex's `TerminalPermissions`, for the permissions this build has.
#[derive(Debug, Serialize, Deserialize)]
struct TerminalLaunch {
    sandbox: Option<ExecSandboxPolicy>,
    escalated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    additional_permissions: Option<AdditionalPermissionProfile>,
}

/// The coding product's [`WriteStdinPolicy`]: Codex's stdin review. Input to a running terminal
/// asks first only when the terminal runs with authority a command launched now would not get —
/// it was launched outside its sandbox, the thread's sandbox changed since, or it holds
/// permissions `request_permissions` granted, which it keeps after its turn's grants end — and is
/// refused where the approval policy rejects sandbox prompts. A terminal this policy did not
/// launch is not reviewed.
///
/// Codex's denied-read check has no counterpart: the executor's sandboxes deny no reads.
pub(crate) struct CodingStdinPolicy;

impl WriteStdinPolicy for CodingStdinPolicy {
    fn review(
        &self,
        context: &ToolContext<'_>,
        terminal: &BackgroundTerminal,
        chars: &str,
    ) -> Result<StdinReview> {
        let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
            return Ok(StdinReview::Allow);
        };
        let Some(launch) = terminal
            .launch_state()
            .and_then(|state| serde_json::from_value::<TerminalLaunch>(state.clone()).ok())
        else {
            return Ok(StdinReview::Allow);
        };
        let baseline = sandboxed_environment(settings, &ExecEnvironment::new())
            .sandbox()
            .cloned();
        // What a command launched now with the terminal's grants would run in. Grants it would be
        // refused leave nothing to match, so the input asks.
        let current = match (&baseline, &launch.additional_permissions) {
            (Some(sandbox), Some(permissions)) => {
                let paths = PolicyPaths::for_command(
                    &settings.cwd,
                    terminal.cwd().unwrap_or(&settings.cwd),
                );
                granted_sandbox(sandbox.clone(), permissions, &paths.context()).ok()
            }
            _ => baseline.clone(),
        };
        let sandbox_permissions = if launch.escalated || launch.sandbox != current {
            SandboxPermissions::RequireEscalated
        } else if launch.sandbox == baseline {
            SandboxPermissions::UseDefault
        } else {
            SandboxPermissions::WithAdditionalPermissions
        };
        if sandbox_permissions == SandboxPermissions::UseDefault {
            return Ok(StdinReview::Allow);
        }
        // Manual approvals shell-quote the input, which cannot preserve NUL bytes.
        if chars.contains('\0') {
            return Ok(StdinReview::Refuse(
                "terminal input contains a NUL byte and cannot be reviewed safely".to_owned(),
            ));
        }
        let authority = if launch.escalated {
            "This terminal was launched outside the sandbox, bypassing any managed network proxy."
        } else if launch
            .sandbox
            .as_ref()
            .is_none_or(|sandbox| sandbox.level() == SandboxLevel::Unconfined)
        {
            "This terminal runs without a filesystem sandbox."
        } else if sandbox_permissions == SandboxPermissions::WithAdditionalPermissions {
            "This terminal retains additional permissions."
        } else {
            "This terminal retains sandbox or network settings that differ from the current permissions."
        };
        let mut reason = format!(
            "Send input to an existing terminal. {authority} The cwd is its launch directory; the terminal's current directory and state may have changed."
        );
        if let Some(grants) = &launch.additional_permissions {
            // The reason also reaches clients that strip the experimental `additionalPermissions`.
            match serde_json::to_string(grants) {
                Ok(grants) => {
                    reason.push_str(" Retained grants: ");
                    reason.push_str(&grants);
                    reason.push('.');
                }
                Err(error) => return Ok(StdinReview::Refuse(error.to_string())),
            }
        }
        let state = StdinApproval {
            launch_call_id: terminal.call_id().map(str::to_owned),
            session_id: terminal.session_id().as_str().to_owned(),
            cwd: terminal.cwd().map(Path::to_path_buf),
            input: chars.to_owned(),
            reason,
            sandbox_permissions,
            additional_permissions: launch.additional_permissions,
        };
        // Codex bounds the complete pretty-printed action plus its reason, including JSON
        // escaping. Never ask about a truncated input or execute an unreviewed tail.
        if !state.can_be_reviewed() {
            return Ok(StdinReview::Refuse(
                "terminal input and permission details are too large to review safely; use a smaller input or start a new terminal with fewer grants".to_owned(),
            ));
        }
        if let Some(reason) = prompt_is_rejected_by_policy(settings.approval_policy, false) {
            return Ok(StdinReview::Refuse(reason.to_owned()));
        }
        encode(&state).map(StdinReview::Ask)
    }
}

/// Codex's request for input to a running terminal: a command approval of kind `writeStdin` for
/// the item of the call that launched the terminal, naming the input call as its approval, the
/// input as Codex's `write_stdin --session-id` command line, and approving once or cancelling as
/// the only decisions.
fn stdin_approval_request(pending: PendingApproval<'_>) -> Option<ServerRequestPayload> {
    let approval = pending.approval();
    let state = approval
        .resume()
        .and_then(|resume| ra_tools::write_stdin::approval_policy_state(resume.state()))
        .and_then(|state| serde_json::from_value::<StdinApproval>(state.clone()).ok())?;
    let call_id = approval.call_id().as_str();
    let mut params = CommandExecutionRequestApprovalParams::new(
        pending.thread_id(),
        pending.turn_id(),
        state.launch_call_id.as_deref().unwrap_or(call_id),
        pending.started_at_ms(),
    )
    .with_kind(CommandExecutionApprovalKind::WriteStdin)
    .with_approval_id(call_id)
    .with_environment_id(LOCAL_ENVIRONMENT_ID)
    .with_command(shlex_join(&[
        "write_stdin".to_owned(),
        "--session-id".to_owned(),
        state.session_id,
        state.input,
    ]))
    .with_reason(state.reason)
    .with_available_decisions(vec![
        CommandExecutionApprovalDecision::Accept,
        CommandExecutionApprovalDecision::Cancel,
    ]);
    if let Some(additional_permissions) = &state.additional_permissions {
        params =
            params.with_additional_permissions(wire_additional_profile(additional_permissions));
    }
    if let Some(cwd) = state.cwd {
        params = params.with_cwd(cwd.to_string_lossy());
    }
    Some(ServerRequestPayload::CommandExecutionRequestApproval(
        params,
    ))
}

/// What an input approval is asked about: Codex's `ApprovalAction::WriteStdin`, for the
/// permissions this build has.
#[derive(Debug, Serialize, Deserialize)]
struct StdinApproval {
    launch_call_id: Option<String>,
    session_id: String,
    cwd: Option<PathBuf>,
    input: String,
    reason: String,
    #[serde(default)]
    sandbox_permissions: SandboxPermissions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    additional_permissions: Option<AdditionalPermissionProfile>,
}

const MAX_STDIN_APPROVAL_BYTES: usize = 8_000;

/// Codex's reviewed stdin action for this executor's string session IDs and pipe input.
/// Launch sandboxes stay host-owned; only the override the review found and the terminal's grants
/// enter the reviewed action.
#[derive(Serialize)]
struct StdinApprovalAction<'a> {
    tool: &'static str,
    session_id: &'a str,
    chars: &'a str,
    cwd: Option<&'a Path>,
    sandbox_permissions: SandboxPermissions,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_permissions: Option<&'a AdditionalPermissionProfile>,
    tty: bool,
}

impl StdinApproval {
    fn can_be_reviewed(&self) -> bool {
        let action = StdinApprovalAction {
            tool: WRITE_STDIN_TOOL,
            session_id: &self.session_id,
            chars: &self.input,
            cwd: self.cwd.as_deref(),
            sandbox_permissions: self.sandbox_permissions,
            additional_permissions: self.additional_permissions.as_ref(),
            tty: false,
        };
        serde_json::to_string_pretty(&action).is_ok_and(|reviewed| {
            reviewed.len().saturating_add(self.reason.len()) <= MAX_STDIN_APPROVAL_BYTES
        })
    }
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
    exec_permission_approvals: bool,
) -> Result<CommandDecision> {
    let arguments: CommandArguments =
        serde_json::from_value(context.arguments().clone()).map_err(|error| {
            ra_tools::exec_command::rejection(format!("invalid arguments: {error}"))
        })?;
    let cwd = settings.command_cwd(arguments.workdir.as_deref());
    let paths = PolicyPaths::for_command(&settings.cwd, &cwd);
    let policy_context = paths.context();
    let effective = command_permissions(
        context,
        settings,
        &arguments,
        &policy_context,
        exec_permission_approvals,
    )?;
    let sandbox_permissions = arguments.sandbox_permissions.unwrap_or_default();
    let approval_policy = settings.approval_policy;
    let command = arguments.command();
    let requirement = settings
        .exec_policy
        .create_exec_approval_requirement_for_shell(
            ExecApprovalRequest {
                command: &command,
                approval_policy,
                file_system_sandbox: FileSystemSandboxKind::of(&settings.sandbox_policy),
                // Preapproved permissions do not ask again.
                sandbox_permissions: if effective.permissions_preapproved {
                    SandboxPermissions::UseDefault
                } else {
                    effective.sandbox_permissions
                },
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
        let environment = sandboxed_environment(settings, &ExecEnvironment::new());
        match (environment.sandbox(), &effective.additional_permissions) {
            (Some(sandbox), Some(permissions)) => {
                let sandbox = granted_sandbox(sandbox.clone(), permissions, &policy_context)
                    .map_err(ra_tools::exec_command::rejection)?;
                environment.with_sandbox(sandbox)
            }
            _ => environment,
        }
    };
    Ok(CommandDecision {
        requirement,
        sandbox_permissions: effective.sandbox_permissions,
        additional_permissions: effective.additional_permissions,
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

/// What the call runs with beyond the thread's sandbox, as Codex's `exec_command` handler decides
/// it: the call's own `additional_permissions`, read relative to the directory it runs in, with the
/// turn's and the session's grants added, checked as Codex checks what a call asks for inline. The
/// permissions returned are the normalized ones the command runs with.
fn command_permissions(
    context: &ToolContext<'_>,
    settings: &CodingThreadSettings,
    arguments: &CommandArguments,
    policy_context: &FileSystemSandboxPolicyContext<'_>,
    exec_permission_approvals: bool,
) -> Result<EffectiveAdditionalPermissions> {
    let additional_permissions = arguments
        .additional_permissions
        .clone()
        .map(|permissions| {
            let _guard = AbsolutePathBufGuard::new(policy_context.cwd());
            serde_json::from_value::<AdditionalPermissionProfile>(permissions)
        })
        .transpose()
        .map_err(|error| {
            ra_tools::exec_command::rejection(format!("invalid arguments: {error}"))
        })?;
    if arguments.justification.is_some() && arguments.sandbox_permissions.is_none() {
        return Err(ra_tools::exec_command::rejection(
            JUSTIFICATION_WITHOUT_SANDBOX_PERMISSIONS,
        ));
    }
    let sandbox_permissions = arguments.sandbox_permissions.unwrap_or_default();
    let approval_policy = settings.approval_policy;
    let effective = apply_granted_turn_permissions(
        context,
        settings,
        policy_context,
        sandbox_permissions,
        additional_permissions.clone(),
    );
    // Codex's `request_permissions_tool` feature also lets preapproved permissions through; grants
    // exist only once that tool was installed and answered.
    let additional_permissions_allowed =
        exec_permission_approvals || effective.permissions_preapproved;
    // Sticky grants were approved when they were given, so they continue through the normal
    // approval flow for the command. Codex formats the policy with `Debug`, which differs from
    // Codex's only for `Granular`: this build's variant carries its fields itself rather than in a
    // `GranularApprovalConfig`.
    if effective.sandbox_permissions.requests_sandbox_override()
        && !effective.permissions_preapproved
        && prompt_is_rejected_by_policy(approval_policy, false).is_some()
    {
        return Err(ra_tools::exec_command::rejection(format!(
            "approval policy is {approval_policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {approval_policy:?}"
        )));
    }
    let normalized = match implicit_granted_permissions(
        sandbox_permissions,
        additional_permissions.as_ref(),
        &effective,
    ) {
        Some(permissions) => Some(permissions),
        None => normalize_and_validate_additional_permissions(
            additional_permissions_allowed,
            approval_policy,
            effective.sandbox_permissions,
            effective.additional_permissions,
            effective.permissions_preapproved,
            policy_context,
        )
        .map_err(ra_tools::exec_command::rejection)?,
    };
    Ok(EffectiveAdditionalPermissions {
        sandbox_permissions: effective.sandbox_permissions,
        additional_permissions: normalized,
        permissions_preapproved: effective.permissions_preapproved,
    })
}

/// Whether a patch asks before it is applied: Codex's patch safety check, and its approval cache
/// when it would ask.
pub(crate) fn patch_approval(context: &ToolContext<'_>) -> Result<bool> {
    let Some(settings) = context.run().app_context::<CodingThreadSettings>() else {
        return Ok(true);
    };
    // A patch the grants already cover runs without asking, as Codex's preapproved patch does,
    // and one touching only paths the host approved for the session does not ask again.
    Ok(patch_asks(settings)?
        && !settings.patch_preapproved(context)
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
