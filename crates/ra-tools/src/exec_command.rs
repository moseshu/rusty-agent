//! Runs a command in a shell; returns output or a `session_id` for background execution.
//!
//! # Three layers, and what each one is allowed to know
//!
//! - `ra-tools` — this file — owns the model-facing schema, argument decoding, and the sentence the
//!   model reads back. It knows what a tool call is and nothing about process groups.
//! - [`ra_exec`] owns process lifetime, signalling, output capture, and sessions. It knows nothing
//!   about tools, which is why its failures arrive as [`ExecError`] and are given a tool's name
//!   here rather than there.
//! - The product layer supplies the workspace root and, through
//!   [`ExecCommandTool::with_policy_resolvers`], the policy that decides whether a command runs at
//!   all and in which sandbox.
//!
//! # Every argument in the schema does something
//!
//! A field the model must fill and nothing reads is not free: it costs tokens on every call, and it
//! tells the model it has control it does not have. `tty` is the sharpest case — a request for a
//! terminal that quietly produced a pipe would leave the model waiting for a prompt that can never
//! arrive, so it is refused with a sentence instead. The approval-shaped arguments Codex carries
//! (`sandbox_permissions`, `justification`, `prefix_rule`) are advertised only once policy resolvers
//! are installed, for the same reason: the host's policy is what reads them, so a tool without one
//! neither advertises nor accepts them. Their descriptions are Codex's; Codex's
//! `with_additional_permissions` value is not offered yet.
//!
//! Output ceilings are host configuration rather than an argument, on the same grounds `read_file`
//! states: the model cannot know the context budget it is spending against, so an argument asking
//! for more output only adds a way to ask for more than the host can afford.

use std::{
    fmt,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    event::{
        FnHostEventSink, HostEvent, HostEventBody, HostEventEmitter,
        exec::{ExecEvent, ExecExitedEvent, ExecSessionId},
    },
    item::CallId,
    state::{ArgumentFingerprint, RunId},
    tool::{
        DecodedToolInput, FuncSchema, ObservationMetadata, ResourceClaim, Tool, ToolApprovalPolicy,
        ToolApprovalRequest, ToolArgumentDecodeError, ToolConcurrency, ToolContext,
        ToolFailureHandling, ToolLookupKey, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
        Truncation, TruncationStage,
    },
};
use ra_exec::{
    command::{ExecLimits, ExecRequest},
    fs::{RootedFileSystem, Workspace},
    output::ExecOutputSummary,
    sandbox::{ExecEnvironment, is_likely_sandbox_denied},
    session::{ExecError, ExecExecutionResult, ExecSessionState, ProcessManager},
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::session_return::{ReturnReason, SessionReturn, Watched};

/// The advertised name. Identity and schema must agree on it or [`Tool::validate`] refuses.
const TOOL_NAME: &str = "exec_command";

/// Declares an argument form of `exec_command`: the arguments every form takes, then `extra`.
macro_rules! exec_command_input {
    ($name:ident { $($extra:tt)* }) => {
        // The doc comment below is the model-facing description. Measurements show that the
        // tools doing the work carry short descriptions and the dangerous ones carry contracts;
        // this is both, so the description says what the tool returns and the argument
        // descriptions carry the rest.
        #[derive(Debug, Clone, Deserialize, JsonSchema, ToolInput)]
        #[serde(deny_unknown_fields)]
        /// Runs a command in a shell. Returns its output, or a `session_id` when the command is still
        /// running after the yield timeout.
        struct $name {
            /// Command to execute in the shell.
            cmd: String,
            /// Working directory for the command. A relative path resolves against the workspace root.
            workdir: Option<String>,
            /// Shell to run the command with. Defaults to `/bin/sh`.
            shell: Option<String>,
            /// Request a terminal. Not available; leave unset or false.
            tty: Option<bool>,
            /// Run the shell as a login shell, so that profile files are read.
            login: Option<bool>,
            /// Milliseconds to wait before the command yields to the background (default: 10000). The host
            /// may cap this; asking for longer than it allows yields at the cap.
            yield_time_ms: Option<u64>,
            /// Milliseconds after which the command is stopped for good. Unset means the host's limit.
            timeout_ms: Option<u64>,
            $($extra)*
        }
    };
}

exec_command_input!(ExecCommandInput {});

// The approval arguments are read from the call's arguments by the host's policy resolvers, not by
// this tool, which only advertises and accepts them.
exec_command_input!(PolicyExecCommandInput {
    /// Per-command sandbox override. Defaults to `use_default`; use `require_escalated` for
    /// unsandboxed execution.
    #[allow(dead_code)]
    sandbox_permissions: Option<SandboxPermissionsArgument>,
    /// User-facing approval question for `require_escalated`; omit otherwise.
    #[allow(dead_code)]
    justification: Option<String>,
    /// Reusable approval prefix for `cmd`, only with `sandbox_permissions: "require_escalated"`;
    /// for example ["git", "pull"].
    #[allow(dead_code, clippy::doc_link_with_quotes)]
    prefix_rule: Option<Vec<String>>,
});

impl From<PolicyExecCommandInput> for ExecCommandInput {
    fn from(input: PolicyExecCommandInput) -> Self {
        Self {
            cmd: input.cmd,
            workdir: input.workdir,
            shell: input.shell,
            tty: input.tty,
            login: input.login,
            yield_time_ms: input.yield_time_ms,
            timeout_ms: input.timeout_ms,
        }
    }
}

// The values of Codex's `sandbox_permissions` this build offers. A plain comment rather than a doc
// comment, which would reach the model as the schema's description.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxPermissionsArgument {
    UseDefault,
    RequireEscalated,
}

/// Ceilings and timing defaults applied by the `exec_command` tool.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecCommandLimits {
    max_output_bytes: usize,
    default_yield_time_ms: u64,
}

impl Default for ExecCommandLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecCommandLimits {
    /// Creates the default limits.
    ///
    /// The ceiling is per stream, so a command that writes to both can return twice it. That is
    /// deliberate: a failing build whose diagnostics all land on stderr must not lose them to a
    /// budget that stdout has already spent.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_output_bytes: 100 * 1024,
            default_yield_time_ms: 10_000,
        }
    }

    /// Sets the ceiling on captured bytes per stream before truncation.
    #[must_use]
    pub const fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Sets the default yield timeout in milliseconds.
    #[must_use]
    pub const fn with_default_yield_time_ms(mut self, ms: u64) -> Self {
        self.default_yield_time_ms = ms;
        self
    }

    /// Ceiling on captured bytes per stream before truncation.
    #[must_use]
    pub const fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// Default yield timeout in milliseconds.
    #[must_use]
    pub const fn default_yield_time_ms(&self) -> u64 {
        self.default_yield_time_ms
    }
}

/// The `exec_command` tool.
#[derive(Clone)]
pub struct ExecCommandTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    /// The schema with the approval arguments, advertised once policy resolvers are installed.
    policy_func_schema: FuncSchema,
    options: ToolOptions,
    process_manager: Arc<ProcessManager>,
    limits: ExecCommandLimits,
    root: Option<PathBuf>,
    rooted_filesystem: Option<Arc<RootedFileSystem>>,
    policy: Option<Arc<dyn ExecCommandPolicy>>,
}

/// Prepared command and policy state kept with the approval. The request owns defaults and the
/// resolved working directory, so restoring a question cannot select a different command.
#[derive(Serialize, Deserialize)]
struct ExecCommandResume {
    run_id: RunId,
    call_id: CallId,
    tool: ToolLookupKey,
    arguments: ArgumentFingerprint,
    policy_state: serde_json::Value,
    request: ExecRequest,
    /// If the retry cannot launch, this unsuccessful attempt becomes the command's final exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failed_attempt: Option<ExecExitedEvent>,
}

impl ExecCommandResume {
    fn new(
        context: &ToolContext<'_>,
        policy_state: serde_json::Value,
        request: ExecRequest,
    ) -> Self {
        Self {
            run_id: context.run().run_id().clone(),
            call_id: context.call_id().clone(),
            tool: context.origin().lookup_key().clone(),
            arguments: ArgumentFingerprint::compute(context.arguments()),
            policy_state,
            request,
            failed_attempt: None,
        }
    }

    fn encode(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self)
            .map_err(|error| Error::caller(format!("cannot save command approval: {error}")))
    }

    fn check(&self, context: &ToolContext<'_>) -> Result<()> {
        if self.run_id != *context.run().run_id()
            || self.call_id != *context.call_id()
            || self.tool != *context.origin().lookup_key()
            || self.arguments != ArgumentFingerprint::compute(context.arguments())
        {
            return Err(Error::caller(format!(
                "Model reused tool call ID `{}` for a different invocation. Use a unique call ID for each tool invocation.",
                context.call_id()
            )));
        }
        Ok(())
    }
}

/// The state an [`ExecCommandPolicy`] kept with the approval a command asked for, read from the
/// state `exec_command` keeps with that approval ([`ToolResume::state`]): what the policy
/// returned from [`ExecCommandPolicy::approval_state`], or with
/// [`SandboxDenialAction::RequestApproval`]. `None` for state that is not `exec_command`'s.
///
/// [`ToolResume::state`]: ra_core::item::ToolResume::state
#[must_use]
pub fn approval_policy_state(state: &serde_json::Value) -> Option<&serde_json::Value> {
    state.get("policy_state")
}

/// A process exit is not the logical command's end until its policy has decided whether to retry.
/// Codex detects early sandbox denials before publishing execution completion. This executor
/// reports process exits independently, so hold that boundary until the tool makes the decision.
/// Start and output events remain immediate; yielded sessions forward their eventual exits.
struct AttemptExitEvents {
    emitter: HostEventEmitter,
    source: HostEventEmitter,
    pending: Arc<Mutex<Option<Vec<HostEvent>>>>,
}

impl AttemptExitEvents {
    fn new(source: &HostEventEmitter) -> Self {
        let pending = Arc::new(Mutex::new(Some(Vec::new())));
        let sink_pending = Arc::clone(&pending);
        let sink_source = source.clone();
        let sink = FnHostEventSink::new(move |event: HostEvent| {
            if matches!(event.body(), HostEventBody::Exec(ExecEvent::Exited(_))) {
                let mut guard = sink_pending.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(events) = guard.as_mut() {
                    events.push(event);
                    return;
                }
            }
            sink_source.sink().emit(event);
        });
        Self {
            emitter: HostEventEmitter::new(
                source.agent_id().clone(),
                source.allocator().clone(),
                Arc::new(sink),
            ),
            source: source.clone(),
            pending,
        }
    }

    fn release(&self) {
        let events = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        for event in events.into_iter().flatten() {
            self.source.sink().emit(event);
        }
    }

    fn discard(&self) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    fn failed_attempt(&self) -> Option<ExecExitedEvent> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()?
            .iter()
            .find_map(|event| match event.body() {
                HostEventBody::Exec(ExecEvent::Exited(exit)) => Some(exit.clone()),
                _ => None,
            })
    }
}

impl Drop for AttemptExitEvents {
    fn drop(&mut self) {
        self.release();
    }
}

/// Host callback evaluated against the immutable context captured for a model step.
pub type EnvironmentResolver = dyn Fn(&ToolContext<'_>) -> Result<ExecEnvironment> + Send + Sync;

/// Host approval callback evaluated for each command.
pub type ApprovalResolver = dyn Fn(&ToolContext<'_>) -> Result<bool> + Send + Sync;

/// The error an [`ExecCommandPolicy`] returns to refuse a command: the model reads `reason`
/// verbatim as the command's result, as Codex returns a rejected command's reason.
#[must_use]
pub fn rejection(reason: impl Into<String>) -> Error {
    ExecCommandFailure::Rejected(reason.into()).into_error()
}

/// Codex's launch grace period: only a sandbox refusal detected while opening the process
/// enters the orchestrator's retry path. Later refusals are returned as command output.
const EARLY_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(150);

/// The host policy deciding whether each command asks first, and in which sandbox it runs: the
/// part of Codex's tool orchestrator a host supplies.
///
/// The tool calls it in order: [`Self::needs_approval`] and, when the call asks,
/// [`Self::approval_state`] before the call; [`Self::attempt`] for the environment the command runs
/// in; and [`Self::on_sandbox_denial`] when that attempt's sandbox most likely refused the command
/// as it started. Each is handed the call's context, including
/// [`ToolContext::resume_state`] when the call resumes from an approval.
pub trait ExecCommandPolicy: Send + Sync {
    /// Whether the call asks its host before it runs.
    ///
    /// # Errors
    ///
    /// An error fails the run, as [`Tool::needs_approval`]'s does.
    fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool>;

    /// State to keep with the approval the call asks for: see [`Tool::approval_state`].
    ///
    /// # Errors
    ///
    /// An error fails the run.
    fn approval_state(&self, _context: &ToolContext<'_>) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }

    /// The environment the call's command runs in.
    ///
    /// # Errors
    ///
    /// A [`rejection`] refuses the command with its reason; any other error is the call's failure.
    fn attempt(&self, context: &ToolContext<'_>) -> Result<ExecCommandAttempt>;

    /// What follows an attempt its sandbox most likely refused as the command started. The default
    /// returns the attempt's output.
    ///
    /// # Errors
    ///
    /// An error is the call's failure.
    fn on_sandbox_denial(
        &self,
        _context: &ToolContext<'_>,
        _attempt: &ExecCommandAttempt,
        _output: &ExecOutputSummary,
    ) -> Result<SandboxDenialAction> {
        Ok(SandboxDenialAction::Return)
    }
}

/// The environment one attempt runs a command in, with whatever the policy wants back if the
/// sandbox refuses it.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ExecCommandAttempt {
    environment: ExecEnvironment,
    state: Option<serde_json::Value>,
}

impl ExecCommandAttempt {
    /// An attempt in `environment`.
    #[must_use]
    pub const fn new(environment: ExecEnvironment) -> Self {
        Self {
            environment,
            state: None,
        }
    }

    /// Keeps `state` for [`ExecCommandPolicy::on_sandbox_denial`].
    #[must_use]
    pub fn with_state(mut self, state: serde_json::Value) -> Self {
        self.state = Some(state);
        self
    }

    /// The environment the command runs in.
    #[must_use]
    pub const fn environment(&self) -> &ExecEnvironment {
        &self.environment
    }

    /// The state the policy kept with this attempt.
    #[must_use]
    pub const fn state(&self) -> Option<&serde_json::Value> {
        self.state.as_ref()
    }
}

/// What follows an attempt its sandbox refused: the branches of Codex's orchestrator after a
/// sandbox denial.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum SandboxDenialAction {
    /// The attempt's output is the call's result.
    Return,
    /// Run the command again in this environment, without asking.
    Retry(Box<ExecEnvironment>),
    /// Ask the host before running it again; the call resumes with this state in
    /// [`ToolContext::resume_state`] once approved.
    RequestApproval(serde_json::Value),
}

/// The policy [`ExecCommandTool::with_policy_resolvers`] installs: an environment and an approval
/// callback, and no retry after a sandbox denial.
struct ResolverPolicy {
    environment: Arc<EnvironmentResolver>,
    approval: Arc<ApprovalResolver>,
}

impl ExecCommandPolicy for ResolverPolicy {
    fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        (self.approval)(context)
    }

    fn attempt(&self, context: &ToolContext<'_>) -> Result<ExecCommandAttempt> {
        (self.environment)(context).map(ExecCommandAttempt::new)
    }
}

impl fmt::Debug for ExecCommandTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecCommandTool")
            .field("origin", &self.origin)
            .field("root", &self.root)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ExecCommandTool {
    /// Creates an ambient execution tool that runs anywhere the process can.
    pub fn new() -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<ExecCommandInput>(TOOL_NAME)?,
            policy_func_schema: FuncSchema::for_input::<PolicyExecCommandInput>(TOOL_NAME)?,
            options: base_options(),
            process_manager: Arc::new(ProcessManager::default()),
            limits: ExecCommandLimits::new(),
            root: None,
            rooted_filesystem: None,
            policy: None,
        })
    }

    /// Creates a tool whose working directories are confined to `root`, which must exist.
    ///
    /// The confinement is on the directory a command starts in, not on what it then does: a shell
    /// can name any path it likes once it is running. Keeping a command from reaching outside the
    /// workspace is the sandbox's job, and this root is what tells it where the workspace is.
    pub fn rooted(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let workspace = Workspace::open(root).map_err(|error| {
            Error::config(format!(
                "exec_command workspace root `{}` cannot be opened as a workspace",
                root.display()
            ))
            .with_source(error)
        })?;
        Self::for_workspace(&workspace)
    }

    /// Creates a command tool rooted in an already-opened workspace capability.
    pub fn for_workspace(workspace: &Workspace) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<ExecCommandInput>(TOOL_NAME)?,
            policy_func_schema: FuncSchema::for_input::<PolicyExecCommandInput>(TOOL_NAME)?,
            options: base_options()
                .with_resource_claim(ResourceClaim::exclusive(workspace.resource_id().clone())),
            process_manager: Arc::new(ProcessManager::default()),
            limits: ExecCommandLimits::new(),
            root: Some(workspace.root().to_path_buf()),
            rooted_filesystem: Some(Arc::clone(workspace.filesystem())),
            policy: None,
        })
    }

    /// Associates an existing [`ProcessManager`] runtime with this tool.
    ///
    /// Sharing one is what lets the interactive stdin tool address a session this tool started.
    #[must_use]
    pub fn with_manager(mut self, manager: Arc<ProcessManager>) -> Self {
        self.process_manager = manager;
        self
    }

    /// Installs host callbacks for per-step execution and per-call approval policy.
    ///
    /// The tool then advertises and accepts Codex's approval arguments, `sandbox_permissions`,
    /// `justification` and `prefix_rule`, which the callbacks read from the call's arguments.
    #[must_use]
    pub fn with_policy_resolvers(
        mut self,
        environment: Arc<EnvironmentResolver>,
        approval: Arc<ApprovalResolver>,
    ) -> Self {
        self.install_policy(Arc::new(ResolverPolicy {
            environment,
            approval,
        }));
        self
    }

    /// Installs the host policy that decides whether each command asks first and in which
    /// sandbox it runs, including after its sandbox refuses it.
    ///
    /// The tool then advertises and accepts Codex's approval arguments, `sandbox_permissions`,
    /// `justification` and `prefix_rule`, which the policy reads from the call's arguments.
    #[must_use]
    pub fn with_command_policy(mut self, policy: Arc<dyn ExecCommandPolicy>) -> Self {
        self.install_policy(policy);
        self
    }

    fn install_policy(&mut self, policy: Arc<dyn ExecCommandPolicy>) {
        self.func_schema = self.policy_func_schema.clone();
        self.policy = Some(policy);
        self.options = self
            .options
            .clone()
            .with_approval(ToolApprovalPolicy::Dynamic);
    }

    /// Replaces the output and execution ceilings.
    #[must_use]
    pub const fn with_limits(mut self, limits: ExecCommandLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The ceilings in force.
    #[must_use]
    pub const fn limits(&self) -> &ExecCommandLimits {
        &self.limits
    }

    /// The workspace root, when this tool is confined to one.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The process manager backing this tool.
    #[must_use]
    pub fn process_manager(&self) -> &Arc<ProcessManager> {
        &self.process_manager
    }

    /// The call's decoded arguments, decoding them here when the runtime has not.
    fn take_input<T: ra_core::tool::ToolInput + serde::de::DeserializeOwned>(
        &self,
        context: &mut ToolContext<'_>,
    ) -> Result<T> {
        context.take_decoded_input::<T>()?.map_or_else(
            || {
                serde_json::from_value(context.arguments().clone()).map_err(|error| {
                    ExecCommandFailure::BadArguments(ToolArgumentDecodeError::Deserialize {
                        input_type: self.func_schema.input_type_name(),
                        message: error.to_string(),
                    })
                    .into_error()
                })
            },
            Ok,
        )
    }

    /// Turns decoded arguments into the request `ra-exec` runs.
    fn request(&self, input: ExecCommandInput) -> ExecResult<ExecRequest> {
        if input.tty == Some(true) {
            return Err(ExecCommandFailure::PtyUnavailable);
        }
        let cwd = self.resolve_cwd(input.workdir.as_deref())?;

        let mut limits = ExecLimits::new()
            .with_initial_yield_timeout(Duration::from_millis(
                input
                    .yield_time_ms
                    .unwrap_or(self.limits.default_yield_time_ms),
            ))
            .with_max_capture_bytes(self.limits.max_output_bytes);
        if let Some(timeout_ms) = input.timeout_ms {
            limits = limits.with_total_timeout(Some(Duration::from_millis(timeout_ms)));
        }

        let mut request = ExecRequest::new(input.cmd)
            .with_shell(input.shell)
            .with_login(input.login.unwrap_or(false))
            .with_limits(limits);
        if let Some(cwd) = cwd {
            request = request.with_cwd(cwd);
        }
        let limits = request.limits().tightened_by(self.process_manager.limits());
        Ok(request.with_limits(limits))
    }

    fn policy_context<'a>(
        &'a self,
        context: &'a ToolContext<'_>,
        resume: Option<&'a ExecCommandResume>,
    ) -> ToolContext<'a> {
        let context = ToolContext::new(context.run(), self, context.call_id(), context.arguments())
            .with_caller(context.caller())
            .with_services(context.services());
        match resume {
            Some(resume) => context.with_resume_state(&resume.policy_state),
            None => context,
        }
    }

    async fn execute_attempt(
        &self,
        mut context: ToolContext<'_>,
        resume: Option<&ExecCommandResume>,
        attempt: ExecCommandAttempt,
        emitter: Option<&HostEventEmitter>,
    ) -> Result<ToolOutput> {
        let request = if let Some(resume) = resume {
            resume.request.clone()
        } else {
            let input = if self.policy.is_some() {
                self.take_input::<PolicyExecCommandInput>(&mut context)?
                    .into()
            } else {
                self.take_input::<ExecCommandInput>(&mut context)?
            };
            self.request(input)
                .map_err(ExecCommandFailure::into_error)?
        }
        .with_call_id(context.call_id().as_str());
        // Read before the request is handed over, because the yielded result reports how long it
        // waited and the effective figure is the one the request carries, not the one asked for.
        let yielded_after = request.limits().initial_yield_timeout();

        let attempt_events = self
            .policy
            .as_ref()
            .and(emitter)
            .map(AttemptExitEvents::new);
        let attempt_emitter = attempt_events
            .as_ref()
            .map(|events| &events.emitter)
            .or(emitter);
        let mut outcome = self
            .process_manager
            .execute_with_environment(request.clone(), attempt_emitter, attempt.environment())
            .await
            .map_err(|error| ExecCommandFailure::from(&error).into_error())?;

        // Only startup refusals enter Codex's orchestrator. A later refusal is command output;
        // replaying the whole command could duplicate side effects from its earlier operations.
        if let (Some(policy), ExecExecutionResult::Completed(summary)) = (&self.policy, &outcome)
            && summary.duration() <= EARLY_EXIT_GRACE_PERIOD
            && is_likely_sandbox_denied(summary)
        {
            match policy.on_sandbox_denial(
                &self.policy_context(&context, resume),
                &attempt,
                summary,
            )? {
                SandboxDenialAction::Retry(environment) => {
                    outcome = self
                        .process_manager
                        .execute_with_environment(request, emitter, environment.as_ref())
                        .await
                        .map_err(|error| ExecCommandFailure::from(&error).into_error())?;
                    if let Some(events) = &attempt_events {
                        events.discard();
                    }
                }
                SandboxDenialAction::RequestApproval(state) => {
                    let mut state = ExecCommandResume::new(&context, state, request);
                    state.failed_attempt = attempt_events
                        .as_ref()
                        .and_then(AttemptExitEvents::failed_attempt);
                    let state = state.encode()?;
                    if let Some(events) = &attempt_events {
                        events.discard();
                    }
                    return Err(ToolApprovalRequest::new(state).into_error(TOOL_NAME));
                }
                SandboxDenialAction::Return => {}
            }
        }

        if let Some(events) = &attempt_events {
            events.release();
        }

        Ok(match outcome {
            ExecExecutionResult::Completed(summary) => completed_output(&summary),
            ExecExecutionResult::Yielded {
                session_id,
                summary,
            } => {
                // The yielded result is the first delivery on this session's interactive output
                // stream. Record its exact snapshot rather than the buffers' current end: output
                // can arrive between the yield and this bookkeeping, and it belongs to the next
                // `write_stdin` result.
                //
                // Discarded on purpose. The only way to fail here is a session the registry no
                // longer holds, and the identifier below is the whole handle the model gets:
                // refusing would report a command that actually started as a failure, and take
                // with it the means to collect that command's output or cancel it. A repeated
                // line of output is the smaller price, and it is the price of the arm below too.
                let _ = self
                    .process_manager
                    .mark_interactive_output_delivered(
                        &session_id,
                        as_u64(summary.stdout_bytes()),
                        as_u64(summary.stderr_bytes()),
                    )
                    .await;
                yielded_output(&session_id, &summary, yielded_after)
            }
            // `ExecExecutionResult` is `#[non_exhaustive]`, so the wildcard is mandatory. A result
            // this build cannot name is still a result, and answering the call is what keeps the
            // history well formed.
            other => ToolOutput::text(format!(
                "The command ran, but this build cannot describe how it ended ({:?} bytes of output captured).",
                other.summary().total_bytes()
            )),
        })
    }

    /// Resolves a requested working directory against the workspace root.
    ///
    /// Two refusals rather than one, for the reason `read_file` separates them: a path that left the
    /// workspace and a path spelled with `..` are different news, and only the first is a boundary
    /// violation. An absolute path inside the root is the same request written another way and is
    /// accepted.
    fn resolve_cwd(&self, requested: Option<&str>) -> ExecResult<Option<PathBuf>> {
        let Some(requested) = requested else {
            return Ok(self.root.clone());
        };

        let requested_path = Path::new(requested);
        let Some(root) = &self.root else {
            return Ok(Some(requested_path.to_path_buf()));
        };

        let relative = if requested_path.is_absolute() {
            match requested_path.strip_prefix(root) {
                Ok(relative) => relative.to_path_buf(),
                Err(_) => return Err(ExecCommandFailure::OutsideRoot(requested.to_owned())),
            }
        } else {
            requested_path.to_path_buf()
        };

        for component in relative.components() {
            match component {
                Component::ParentDir => {
                    return Err(ExecCommandFailure::AmbiguousParent(requested.to_owned()));
                }
                // Reachable on Windows, where `C:dir` is relative and still carries a prefix.
                Component::RootDir | Component::Prefix(_) => {
                    return Err(ExecCommandFailure::OutsideRoot(requested.to_owned()));
                }
                Component::CurDir | Component::Normal(_) => {}
            }
        }

        // Resolved through the root's directory capability rather than by inspecting the pathname,
        // so a symbolic link inside the workspace that points out of it is refused instead of
        // followed. What remains is that the shell resolves the path a second time when it starts:
        // a link swapped in between these two moments would not be caught here, and closing that
        // gap is the sandbox's job rather than a check's.
        if let Some(filesystem) = &self.rooted_filesystem
            && !filesystem.exists(&relative)
        {
            return Err(ExecCommandFailure::MissingDirectory(requested.to_owned()));
        }

        let resolved = root.join(&relative);
        if !resolved.is_dir() {
            return Err(ExecCommandFailure::MissingDirectory(requested.to_owned()));
        }
        Ok(Some(resolved))
    }
}

#[async_trait]
impl Tool for ExecCommandTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        self.func_schema.tool_schema()
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        Some(&self.func_schema)
    }

    fn decode_input(&self, arguments: &serde_json::Value) -> Result<Option<DecodedToolInput>> {
        self.func_schema
            .decode_value_diagnostic(arguments.clone())
            .map(Some)
            .map_err(|error| ExecCommandFailure::BadArguments(error).into_error())
    }

    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        // Check invocation identity first, including changed arguments that are themselves invalid.
        let needs_approval = self
            .policy
            .as_ref()
            .map_or(Ok(true), |policy| policy.needs_approval(context))?;
        // Invalid requests are handled by the invocation's normal failure renderer, so the model
        // can correct them without asking the host to approve a command that cannot start.
        let input = if self.policy.is_some() {
            serde_json::from_value::<PolicyExecCommandInput>(context.arguments().clone())
                .map(ExecCommandInput::from)
        } else {
            serde_json::from_value::<ExecCommandInput>(context.arguments().clone())
        };
        let Ok(input) = input else {
            return Ok(false);
        };
        if self.request(input).is_err() {
            return Ok(false);
        }
        Ok(needs_approval)
    }

    async fn approval_state(&self, context: &ToolContext<'_>) -> Result<Option<serde_json::Value>> {
        let Some(policy_state) = self
            .policy
            .as_ref()
            .map_or(Ok(None), |policy| policy.approval_state(context))?
        else {
            return Ok(None);
        };
        let input: PolicyExecCommandInput = serde_json::from_value(context.arguments().clone())
            .map_err(|error| rejection(format!("invalid arguments: {error}")))?;
        let request = self
            .request(input.into())
            .map_err(ExecCommandFailure::into_error)?;
        ExecCommandResume::new(context, policy_state, request)
            .encode()
            .map(Some)
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let resume = context
            .resume_state()
            .map(|state| {
                serde_json::from_value::<ExecCommandResume>(state.clone()).map_err(|error| {
                    Error::caller(format!("cannot restore command approval: {error}"))
                })
            })
            .transpose()?;
        if let Some(resume) = &resume {
            if self.policy.is_none() {
                return Err(Error::caller(
                    "restoring a command approval requires its execution policy",
                ));
            }
            resume.check(&context)?;
        }
        let emitter = context.event_emitter();
        // Consume approval before execution can fail, including spawning and cancellation.
        let attempt = self
            .policy
            .as_ref()
            .map(|policy| policy.attempt(&self.policy_context(&context, resume.as_ref())))
            .transpose()?
            .unwrap_or_else(|| ExecCommandAttempt::new(self.process_manager.environment().clone()));
        let result = self
            .execute_attempt(context, resume.as_ref(), attempt, emitter.as_ref())
            .await;
        // Restoring the approval can fail before a new process has any events. Preserve the
        // original unsuccessful attempt as the terminal failure, using a fresh event sequence.
        if let Err(error) = &result
            && ToolApprovalRequest::of(error).is_none()
            && let Some(exit) = resume
                .as_ref()
                .and_then(|resume| resume.failed_attempt.as_ref())
            && let Some(emitter) = &emitter
        {
            emitter.emit_exec(ExecEvent::Exited(exit.clone()))?;
        }
        result
    }

    fn options(&self) -> ToolOptions {
        // The rooted form declares the workspace claim that serializes conflicting commands. An
        // ambient host has no such claim, so commands remain independently schedulable instead of
        // turning every command into a global run lock.
        //
        // `Custom` failure handling is what buys the model a sentence instead of a bare error code,
        // and every failure this tool produces carries one — a gap would turn a mistyped argument
        // into a stopped run.
        self.options.clone()
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        // Matched on the typed cause carried in the error's source, never on its message. Reading
        // the sentence back out of prose is the de-lexicalization rule this project bans, and it
        // also produced the doubled text a message-sniffing version emitted.
        Ok(ExecCommandFailure::of(error).map(|failure| {
            let output = ToolOutput::text(failure.to_string());
            match failure.next_step() {
                Some(next_step) => {
                    output.with_metadata(ObservationMetadata::new().with_guidance(next_step))
                }
                None => output,
            }
        }))
    }
}

fn base_options() -> ToolOptions {
    ToolOptions::new()
        .with_approval(ToolApprovalPolicy::Always)
        .with_failure_handling(ToolFailureHandling::Custom)
        .with_concurrency(ToolConcurrency::Parallel)
}

/// Renders a command that finished within the yield window.
fn completed_output(summary: &ExecOutputSummary) -> ToolOutput {
    let mut body = render_streams(summary);
    if body.is_empty() {
        body = match summary.exit_code() {
            Some(code) => format!("Command exited with status {code} and produced no output."),
            None => "Command finished and produced no output.".to_owned(),
        };
    }

    let mut metadata = ObservationMetadata::new();
    if let Some(code) = summary.exit_code().filter(|code| *code != 0) {
        metadata = metadata.with_guidance(format!("Command exited with status {code}."));
    }
    metadata = with_truncation(metadata, summary);
    ToolOutput::text(body).with_metadata(metadata)
}

/// Renders a command that was still running when the yield timeout elapsed.
///
/// Said in the same vocabulary `write_stdin` uses, because it is the same news: here is some output,
/// here is where the session stands, and here is whether that output is all of it. The model reads
/// both results in one conversation and cannot tell a difference in wording from a difference in
/// what happened.
fn yielded_output(
    session_id: &ExecSessionId,
    summary: &ExecOutputSummary,
    waited: Duration,
) -> ToolOutput {
    // A yield snapshot predates closeout. Keep that observation even if the process has since
    // finished: only a subsequent poll can deliver the bytes produced after this snapshot.
    SessionReturn {
        session_id,
        watched: Watched::Closeout,
        waited,
        reason: ReturnReason::Timeout,
        state: ExecSessionState::Running,
        closed: false,
        exit_code: summary.exit_code(),
        stdout: summary.stdout().to_owned(),
        stderr: summary.stderr().to_owned(),
        matched: None,
        // This first delivery carries everything the session has produced, so the whole capture is
        // the span it covers and the whole capture's loss is the span's loss.
        delivered_bytes: as_u64(summary.total_bytes()),
        omitted_bytes: as_u64(
            summary
                .total_bytes()
                .saturating_sub(summary.retained_bytes()),
        ),
    }
    .render()
}

/// Joins the two streams, labelling stderr only when there is something on both.
fn render_streams(summary: &ExecOutputSummary) -> String {
    crate::session_return::render_streams(summary.stdout().trim(), summary.stderr().trim())
}

/// Records what the capture ceiling cut, in source bytes on both sides of the pair.
///
/// The retained count comes from the buffers rather than from the rendered body, which also carries
/// the marker standing in for the omitted middle; counting that marker would report more output
/// surviving than the command produced.
fn with_truncation(
    metadata: ObservationMetadata,
    summary: &ExecOutputSummary,
) -> ObservationMetadata {
    if !summary.is_truncated() {
        return metadata;
    }
    metadata
        .with_truncation(Truncation::new(
            TruncationStage::Tool,
            as_u64(summary.total_bytes()),
            as_u64(summary.retained_bytes()),
        ))
        .with_guidance(
            "Output was cut to fit; the middle is missing. Narrow the command or filter its output to see the rest.",
        )
}

type ExecResult<T> = std::result::Result<T, ExecCommandFailure>;

/// Why a command could not run, or could not be asked for.
///
/// It travels in an [`Error`]'s source rather than in its message, so the model-facing sentence is
/// produced once, in [`Tool::handle_failure`], from a value rather than from prose.
#[derive(Debug)]
enum ExecCommandFailure {
    /// The argument object does not match the schema.
    BadArguments(ToolArgumentDecodeError),
    /// The working directory is outside the workspace root.
    OutsideRoot(String),
    /// The working directory is spelled with `..`, which this tool refuses to resolve.
    AmbiguousParent(String),
    /// The working directory does not exist below the root.
    MissingDirectory(String),
    /// A terminal was requested, and this build has none to give.
    PtyUnavailable,
    /// The process could not be started.
    Spawn(String),
    /// Every execution slot is held by something that will not stop.
    Busy(String),
    /// An execution session could not be addressed.
    Session(String),
    /// The host's command policy refused the command; the text is its reason, read verbatim.
    Rejected(String),
}

impl ExecCommandFailure {
    /// Which failure class the host records. Everything the model could have avoided by sending
    /// different arguments is `InvalidInput`; the rest is the tool failing to do its job.
    const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::BadArguments(_)
            | Self::OutsideRoot(_)
            | Self::AmbiguousParent(_)
            | Self::MissingDirectory(_)
            | Self::PtyUnavailable => ToolErrorKind::InvalidInput,
            Self::Spawn(_) | Self::Busy(_) | Self::Session(_) => ToolErrorKind::ExecutionFailed,
            Self::Rejected(_) => ToolErrorKind::PermissionDenied,
        }
    }

    fn into_error(self) -> Error {
        Error::tool(self.kind(), TOOL_NAME, self.to_string()).with_source(self)
    }

    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    /// The next step that follows the diagnosis, kept out of [`fmt::Display`] so a log line gets the
    /// fact without an instruction addressed to a model. None when the diagnosis is the answer.
    const fn next_step(&self) -> Option<&'static str> {
        Some(match self {
            Self::BadArguments(_) => "Send `cmd`, and leave the rest unset unless you need it.",
            Self::OutsideRoot(_) => "Run the command inside the workspace root.",
            Self::AmbiguousParent(_) => "Send `workdir` without `..`.",
            Self::MissingDirectory(_) => "Check the directory exists, or run from the root.",
            Self::PtyUnavailable => {
                "Run the command without `tty`, in a form that needs no terminal."
            }
            Self::Spawn(_) => "Check the command and the shell, then try again.",
            // Not "stop one": by the time this failure exists every one of them has already been
            // told to stop and outlived the deadline for doing so, so a stop request against any
            // of them is a call that returns having done nothing. Waiting is the only move the
            // model has, and the stuck processes are something the user needs to hear about.
            Self::Busy(_) => {
                "Wait for them to be reclaimed and try again; tell the user if it persists."
            }
            Self::Session(_) => "Start the command again rather than addressing that session.",
            // The reason is the whole answer, as Codex returns a rejected command's reason alone.
            Self::Rejected(_) => return None,
        })
    }
}

impl From<&ExecError> for ExecCommandFailure {
    fn from(error: &ExecError) -> Self {
        match error {
            ExecError::Spawn { command, source } => {
                Self::Spawn(format!("`{command}` could not be started: {source}"))
            }
            // Says that they were already asked, because that is what rules out the obvious reply:
            // there is no command here left to stop.
            ExecError::AtCapacity { max_sessions } => Self::Busy(format!(
                "all {max_sessions} slots are held by commands that did not stop when they were told to"
            )),
            // Reachable through a shared process manager rather than through `call`, and
            // `ExecError` is `#[non_exhaustive]`, so both are folded into one class: the model's
            // move is the same for all of them, which is to run the command again.
            other => Self::Session(other.to_string()),
        }
    }
}

impl fmt::Display for ExecCommandFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The decoder's own words, because they name the offending field — which is the whole
            // of what makes this failure correctable on the next turn.
            Self::BadArguments(reason) => write!(f, "Invalid arguments: {reason}."),
            Self::OutsideRoot(path) => write!(f, "`{path}` is outside the workspace."),
            Self::AmbiguousParent(path) => {
                write!(f, "`{path}` uses `..`, which this tool does not resolve.")
            }
            Self::MissingDirectory(path) => write!(f, "No such directory: `{path}`."),
            Self::PtyUnavailable => {
                write!(
                    f,
                    "This build runs commands on pipes and cannot allocate a terminal."
                )
            }
            Self::Spawn(reason) => write!(f, "The command did not start: {reason}."),
            Self::Busy(reason) => write!(f, "No room to run another command: {reason}."),
            Self::Session(reason) => write!(f, "The execution session is unavailable: {reason}."),
            Self::Rejected(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for ExecCommandFailure {}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
