//! Deciding whether a command runs, asks first, or is refused: Codex's `core/src/exec_policy.rs`,
//! with the approval requirement of `core/src/tools/sandboxing.rs` it produces.
//!
//! # Loading
//!
//! The host names the configuration folders rules come from, lowest precedence first, as Codex's
//! config layer stack does; the rules are every `*.rules` file directly in each folder's `rules`
//! directory, in file-name order. Prefix rules accumulate across files and the strictest matching
//! decision wins; a later `host_executable` definition replaces an earlier one. A requirements
//! policy the host passes is merged on top. A rules file that fails to *parse* leaves the policy at
//! the requirements policy alone and is reported as a warning; failing to read a rules directory or
//! file is an error.
//!
//! # Deciding
//!
//! A command is split into the commands its shell script runs (see
//! [`crate::shell_command::parse_shell_lc_plain_commands`]); a script that cannot be split is
//! judged whole. When the shell is neither the default one nor in `/bin` or `/usr/bin`, the
//! executable is judged on its own beside the commands it appears to run, so the script's words can
//! add restrictions but never grant the executable trust. Each command is matched against the
//! rules, and a command no rule matches is decided by [`render_decision_for_unmatched_command`]:
//! a dangerous command asks (or is refused under `Never`), `UnlessTrusted` asks, and under
//! `OnRequest` and `Granular` a restricted sandbox asks only for a per-command sandbox override.
//!
//! # Differences from Codex
//!
//! - The host passes the configuration folders and requirements policy directly; Codex derives
//!   them from its config layer stack, including project trust and the switch that ignores user
//!   and project rules, and loads requirements from `requirements.toml`. Which folders to trust is
//!   the host's decision here.
//! - Codex also overlays a policy per execution environment at decision time; a coding thread has
//!   one environment, whose policy is the requirements policy merged at load.
//! - [`AllowPrefixRules::Ignore`] is Codex's `IgnoreForCyberModel`, chosen there from the model
//!   catalog; here the host chooses it.
//! - Windows and PowerShell rules, and Codex's reuse of a parent's policy by spawned agents, are
//!   not ported.

mod executable_identity;

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, PoisonError, RwLock},
};

use ra_execpolicy::{
    Decision, Error as ExecPolicyRuleError, Evaluation, MatchOptions, Policy, PolicyParser,
    PrefixRule, RequirementsExecPolicy, RuleMatch,
};
use ra_protocol::server::protocol::{AskForApproval, ExecPolicyAmendment, SandboxPolicy};
use serde::{Deserialize, Serialize};
use shlex::try_join as shlex_try_join;
use thiserror::Error;

use crate::shell_command::{
    DangerousCommandMatch, dangerous_command_match, parse_shell_lc_plain_commands,
};

const PROMPT_CONFLICT_REASON: &str =
    "approval required by policy, but AskForApproval is set to Never";
const REJECT_SANDBOX_APPROVAL_REASON: &str =
    "approval required by policy, but AskForApproval::Granular.sandbox_approval is false";
const REJECT_RULES_APPROVAL_REASON: &str =
    "approval required by policy rule, but AskForApproval::Granular.rules is false";
const RULES_DIR_NAME: &str = "rules";
const RULE_EXTENSION: &str = "rules";

/// Prefixes never proposed as a reusable rule from a model's `prefix_rule`: each would approve an
/// interpreter, a shell or a tool broad enough to run anything.
pub static BANNED_PREFIX_SUGGESTIONS: &[&[&str]] = &[
    &["/bin/bash"],
    &["/bin/bash", "-c"],
    &["/bin/bash", "-lc"],
    &["/bin/sh"],
    &["/bin/sh", "-c"],
    &["/bin/sh", "-lc"],
    &["/bin/zsh"],
    &["/bin/zsh", "-c"],
    &["/bin/zsh", "-lc"],
    &["Rscript"],
    &["bash"],
    &["bash", "-c"],
    &["bash", "-lc"],
    &["bun"],
    &["bun", "-e"],
    &["bun", "run"],
    &["cmd"],
    &["cmd", "/c"],
    &["cmd", "/k"],
    &["cmd.exe"],
    &["cmd.exe", "/c"],
    &["cmd.exe", "/k"],
    &["dash"],
    &["dash", "-c"],
    &["deno"],
    &["deno", "eval"],
    &["env"],
    &["fish"],
    &["fish", "-c"],
    &["git"],
    &["julia"],
    &["julia", "-e"],
    &["ksh"],
    &["ksh", "-c"],
    &["lua"],
    &["lua", "-e"],
    &["node"],
    &["node", "-e"],
    &["nodejs"],
    &["nodejs", "-e"],
    &["npm", "run"],
    &["osascript"],
    &["perl"],
    &["perl", "-e"],
    &["php"],
    &["php", "-r"],
    &["pnpm", "run"],
    &["powershell"],
    &["powershell", "-Command"],
    &["powershell", "-EncodedCommand"],
    &["powershell", "-File"],
    &["powershell", "-c"],
    &["powershell.exe"],
    &["powershell.exe", "-Command"],
    &["powershell.exe", "-EncodedCommand"],
    &["powershell.exe", "-File"],
    &["powershell.exe", "-c"],
    &["pwsh"],
    &["pwsh", "-Command"],
    &["pwsh", "-EncodedCommand"],
    &["pwsh", "-File"],
    &["pwsh", "-c"],
    &["pwsh", "-e"],
    &["pwsh", "-ec"],
    &["pwsh", "-f"],
    &["py"],
    &["py", "-3"],
    &["pypy"],
    &["pypy3"],
    &["python"],
    &["python", "-"],
    &["python", "-c"],
    &["python3"],
    &["python3", "-"],
    &["python3", "-c"],
    &["pythonw"],
    &["pyw"],
    &["rm"],
    &["ruby"],
    &["ruby", "-e"],
    &["sh"],
    &["sh", "-c"],
    &["sh", "-lc"],
    &["sudo"],
    &["yarn", "run"],
    &["zsh"],
    &["zsh", "-c"],
    &["zsh", "-lc"],
];

/// The per-command sandbox override a command asks for: Codex's `SandboxPermissions`.
#[derive(Debug, Clone, Copy, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPermissions {
    /// Run with the turn's configured sandbox policy unchanged.
    #[default]
    UseDefault,
    /// Request to run outside the sandbox.
    RequireEscalated,
    /// Request to stay in the sandbox while widening permissions for this command only.
    WithAdditionalPermissions,
}

impl SandboxPermissions {
    /// Whether this asks for full unsandboxed execution.
    #[must_use]
    pub const fn requires_escalated_permissions(self) -> bool {
        matches!(self, Self::RequireEscalated)
    }

    /// Whether this asks for any override beyond the default.
    #[must_use]
    pub const fn requests_sandbox_override(self) -> bool {
        !matches!(self, Self::UseDefault)
    }
}

/// How the filesystem sandbox confines a command: the `kind` of Codex's filesystem sandbox policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSystemSandboxKind {
    /// Commands are confined.
    Restricted,
    /// Commands run without filesystem confinement.
    Unrestricted,
    /// An embedding host confines commands outside this process.
    ExternalSandbox,
}

impl FileSystemSandboxKind {
    /// The kind a coding sandbox policy confines with. A policy this module does not know is
    /// treated as restricted, never as ambient authority.
    #[must_use]
    pub fn of(policy: &SandboxPolicy) -> Self {
        match policy {
            SandboxPolicy::DangerFullAccess => Self::Unrestricted,
            SandboxPolicy::ExternalSandbox { .. } => Self::ExternalSandbox,
            _ => Self::Restricted,
        }
    }
}

/// Whether a model's allow prefix rules are honored: Codex's `AllowPrefixRules`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AllowPrefixRules {
    /// Allow rules apply, and reusable amendments are proposed.
    #[default]
    Honor,
    /// Allow prefix rules are dropped and no amendment is proposed. Codex selects this for models
    /// its catalog marks; here the host selects it.
    Ignore,
}

/// What a command needs before it runs: Codex's `ExecApprovalRequirement`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecApprovalRequirement {
    /// No approval required for this tool call.
    Skip {
        /// The first attempt should skip sandboxing, because every command it runs is explicitly
        /// allowed by a rule.
        bypass_sandbox: bool,
        /// A rule that would skip future approvals for similar commands, offered only if the
        /// command fails in the sandbox and the user is asked to run it outside.
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    },
    /// Approval required for this tool call.
    NeedsApproval {
        /// Why, when a rule asked.
        reason: Option<String>,
        /// A rule that would skip future approvals for similar commands.
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    },
    /// Execution forbidden for this tool call.
    Forbidden {
        /// The model-visible reason.
        reason: String,
    },
}

impl ExecApprovalRequirement {
    /// The amendment this requirement proposes, if any.
    #[must_use]
    pub const fn proposed_execpolicy_amendment(&self) -> Option<&ExecPolicyAmendment> {
        match self {
            Self::NeedsApproval {
                proposed_execpolicy_amendment: Some(prefix),
                ..
            }
            | Self::Skip {
                proposed_execpolicy_amendment: Some(prefix),
                ..
            } => Some(prefix),
            _ => None,
        }
    }
}

/// The inputs a command no rule matched is decided on: Codex's `UnmatchedCommandContext`.
#[derive(Clone, Copy, Debug)]
pub struct UnmatchedCommandContext {
    /// The approval policy.
    pub approval_policy: AskForApproval,
    /// How the filesystem sandbox confines the command.
    pub file_system_sandbox: FileSystemSandboxKind,
    /// The override the command asks for.
    pub sandbox_permissions: SandboxPermissions,
}

/// One command to decide: Codex's `ExecApprovalRequest`.
#[derive(Clone, Copy, Debug)]
pub struct ExecApprovalRequest<'a> {
    /// The command as the shell is invoked, such as `["/bin/sh", "-c", "git status"]`.
    pub command: &'a [String],
    /// The approval policy.
    pub approval_policy: AskForApproval,
    /// How the filesystem sandbox confines the command.
    pub file_system_sandbox: FileSystemSandboxKind,
    /// The override the command asks for.
    pub sandbox_permissions: SandboxPermissions,
    /// The reusable prefix the model asked to be approved with the command.
    pub prefix_rule: Option<&'a [String]>,
    /// Whether allow prefix rules are honored.
    pub allow_prefix_rules: AllowPrefixRules,
}

/// Where rules come from: Codex's config layer stack, as far as rules loading reads it.
#[derive(Clone, Debug, Default)]
pub struct ExecPolicySources {
    config_folders: Vec<PathBuf>,
    requirements: Option<RequirementsExecPolicy>,
}

impl ExecPolicySources {
    /// No folders and no requirements policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a configuration folder whose `rules` directory is read, with higher precedence than
    /// those added before it.
    #[must_use]
    pub fn with_config_folder(mut self, folder: impl Into<PathBuf>) -> Self {
        self.config_folders.push(folder.into());
        self
    }

    /// Merges `requirements` on top of every folder's rules.
    #[must_use]
    pub fn with_requirements(mut self, requirements: RequirementsExecPolicy) -> Self {
        self.requirements = Some(requirements);
        self
    }

    /// The configuration folders, lowest precedence first.
    #[must_use]
    pub fn config_folders(&self) -> &[PathBuf] {
        &self.config_folders
    }

    /// The requirements policy, if any.
    #[must_use]
    pub const fn requirements(&self) -> Option<&RequirementsExecPolicy> {
        self.requirements.as_ref()
    }
}

/// Why rules could not be loaded: Codex's `ExecPolicyError`.
#[derive(Debug, Error)]
pub enum ExecPolicyError {
    /// A rules directory could not be read.
    #[error("failed to read rules files from {dir}: {source}")]
    ReadDir {
        /// The directory.
        dir: PathBuf,
        /// The cause.
        source: std::io::Error,
    },

    /// A rules file could not be read.
    #[error("failed to read rules file {path}: {source}")]
    ReadFile {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },

    /// A rules file failed to parse.
    #[error("failed to parse rules file {path}: {source}")]
    ParsePolicy {
        /// The file.
        path: String,
        /// The cause.
        source: ExecPolicyRuleError,
    },
}

/// The loaded rules of one coding host, and the decisions made with them: Codex's
/// `ExecPolicyManager`.
#[derive(Debug)]
pub struct ExecPolicyManager {
    policy: RwLock<Arc<Policy>>,
}

impl Default for ExecPolicyManager {
    fn default() -> Self {
        Self::new(Arc::new(Policy::empty()))
    }
}

impl ExecPolicyManager {
    /// A manager deciding with `policy`.
    #[must_use]
    pub fn new(policy: Arc<Policy>) -> Self {
        Self {
            policy: RwLock::new(policy),
        }
    }

    /// Loads the rules `sources` name. A rules file that fails to parse is logged and leaves the
    /// requirements policy alone in force, as Codex's session start does.
    pub async fn load(sources: &ExecPolicySources) -> Result<Self, ExecPolicyError> {
        let (policy, warning) = load_exec_policy_with_warning(sources).await?;
        if let Some(err) = warning.as_ref() {
            tracing::warn!("failed to parse rules: {err}");
        }
        Ok(Self::new(Arc::new(policy)))
    }

    /// The policy in force.
    #[must_use]
    pub fn current(&self) -> Arc<Policy> {
        Arc::clone(&self.policy.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The policy in force, without its allow prefix rules when they are ignored.
    #[must_use]
    pub fn current_for_prefix_rules(&self, allow_prefix_rules: AllowPrefixRules) -> Arc<Policy> {
        let policy = self.current();
        if allow_prefix_rules == AllowPrefixRules::Honor {
            return policy;
        }

        let rules = policy
            .rules()
            .iter_all()
            .flat_map(|(program, rules)| {
                rules.iter().filter_map(move |rule| {
                    let is_allow_prefix = rule
                        .as_any()
                        .downcast_ref::<PrefixRule>()
                        .is_some_and(|prefix| prefix.decision == Decision::Allow);
                    (!is_allow_prefix).then(|| (program.clone(), Arc::clone(rule)))
                })
            })
            .collect();

        Arc::new(Policy::from_parts(
            rules,
            policy.network_rules().to_vec(),
            policy.host_executables().clone(),
        ))
    }

    /// Decides `req.command` as given, splitting a shell script into its commands.
    #[must_use]
    pub fn create_exec_approval_requirement_for_command(
        &self,
        req: ExecApprovalRequest<'_>,
    ) -> ExecApprovalRequirement {
        let commands = commands_for_exec_policy(req.command);
        self.create_exec_approval_requirement_for_parsed_commands(req, &commands)
    }

    fn create_exec_approval_requirement_for_parsed_commands(
        &self,
        req: ExecApprovalRequest<'_>,
        commands: &[Vec<String>],
    ) -> ExecApprovalRequirement {
        let ExecApprovalRequest {
            command,
            approval_policy,
            file_system_sandbox,
            sandbox_permissions,
            prefix_rule,
            allow_prefix_rules,
        } = req;
        let exec_policy = self.current_for_prefix_rules(allow_prefix_rules);
        // Avoid reusable approvals when this model does not honor prefix rules.
        let auto_amendment_allowed = allow_prefix_rules == AllowPrefixRules::Honor;
        let exec_policy_fallback = |cmd: &[String]| {
            render_decision_for_unmatched_command(
                cmd,
                UnmatchedCommandContext {
                    approval_policy,
                    file_system_sandbox,
                    sandbox_permissions,
                },
            )
        };
        let match_options = MatchOptions {
            resolve_host_executables: true,
        };
        let evaluation = exec_policy.check_multiple_with_options(
            commands.iter(),
            &exec_policy_fallback,
            &match_options,
        );

        let requested_amendment = if auto_amendment_allowed {
            derive_requested_execpolicy_amendment_from_prefix_rule(
                prefix_rule,
                &evaluation.matched_rules,
                exec_policy.as_ref(),
                commands,
                &exec_policy_fallback,
                &match_options,
            )
        } else {
            None
        };

        match evaluation.decision {
            Decision::Forbidden => ExecApprovalRequirement::Forbidden {
                reason: derive_forbidden_reason(
                    command,
                    &evaluation,
                    dangerous_command_match_for_heuristics(&evaluation, Decision::Forbidden),
                ),
            },
            Decision::Prompt => {
                let prompt_is_rule = evaluation.matched_rules.iter().any(|rule_match| {
                    is_policy_match(rule_match) && rule_match.decision() == Decision::Prompt
                });
                match prompt_is_rejected_by_policy(approval_policy, prompt_is_rule) {
                    Some(reason) if prompt_is_rule => ExecApprovalRequirement::Forbidden {
                        reason: reason.to_string(),
                    },
                    Some(reason) => ExecApprovalRequirement::Forbidden {
                        reason: derive_rejected_prompt_reason(
                            reason,
                            dangerous_command_match_for_heuristics(&evaluation, Decision::Prompt),
                        ),
                    },
                    None => ExecApprovalRequirement::NeedsApproval {
                        reason: derive_prompt_reason(command, &evaluation),
                        proposed_execpolicy_amendment: requested_amendment.or_else(|| {
                            if auto_amendment_allowed {
                                try_derive_execpolicy_amendment_for_prompt_rules(
                                    &evaluation.matched_rules,
                                )
                            } else {
                                None
                            }
                        }),
                    },
                }
            }
            Decision::Allow => ExecApprovalRequirement::Skip {
                // Bypass sandbox only when every parsed command segment is
                // explicitly allowed by execpolicy.
                bypass_sandbox: commands.iter().all(|command| {
                    exec_policy
                        .matches_for_command_with_options(
                            command,
                            /*heuristics_fallback*/ None,
                            &match_options,
                        )
                        .iter()
                        .any(|rule_match| {
                            is_policy_match(rule_match) && rule_match.decision() == Decision::Allow
                        })
                }),
                proposed_execpolicy_amendment: if auto_amendment_allowed {
                    try_derive_execpolicy_amendment_for_allow_rules(&evaluation.matched_rules)
                } else {
                    None
                },
            },
        }
    }
}

const fn is_policy_match(rule_match: &RuleMatch) -> bool {
    match rule_match {
        RuleMatch::PrefixRuleMatch { .. } => true,
        RuleMatch::HeuristicsRuleMatch { .. } => false,
    }
}

const fn allows_sandbox_approval(approval_policy: AskForApproval) -> bool {
    matches!(
        approval_policy,
        AskForApproval::Granular {
            sandbox_approval: true,
            ..
        }
    )
}

const fn allows_rules_approval(approval_policy: AskForApproval) -> bool {
    matches!(
        approval_policy,
        AskForApproval::Granular { rules: true, .. }
    )
}

/// Returns a rejection reason when `approval_policy` disallows surfacing the current prompt to the
/// user.
///
/// `prompt_is_rule` distinguishes policy-rule prompts from sandbox/escalation prompts so granular
/// `rules` and `sandbox_approval` settings are honored independently. When both are present,
/// policy-rule prompts take precedence. A policy this module does not know never refuses a prompt:
/// asking is the conservative answer.
#[must_use]
pub fn prompt_is_rejected_by_policy(
    approval_policy: AskForApproval,
    prompt_is_rule: bool,
) -> Option<&'static str> {
    match approval_policy {
        AskForApproval::Never => Some(PROMPT_CONFLICT_REASON),
        AskForApproval::Granular { .. } => {
            if prompt_is_rule {
                (!allows_rules_approval(approval_policy)).then_some(REJECT_RULES_APPROVAL_REASON)
            } else {
                (!allows_sandbox_approval(approval_policy))
                    .then_some(REJECT_SANDBOX_APPROVAL_REASON)
            }
        }
        _ => None,
    }
}

/// If a command is not matched by any execpolicy rule, derive a [`Decision`].
///
/// A policy this module does not know is decided as `UnlessTrusted` is: every unmatched command
/// asks.
#[must_use]
pub fn render_decision_for_unmatched_command(
    command: &[String],
    context: UnmatchedCommandContext,
) -> Decision {
    let UnmatchedCommandContext {
        approval_policy,
        file_system_sandbox,
        sandbox_permissions,
    } = context;

    // If the command is flagged as dangerous, we should never allow it to run without approval.
    //
    // We prefer to prompt the user rather than outright forbid the command, but if the user has
    // explicitly disabled prompts, we must forbid the command.
    if dangerous_command_match(command).is_some() {
        return match approval_policy {
            AskForApproval::Never => Decision::Forbidden,
            _ => Decision::Prompt,
        };
    }

    match approval_policy {
        AskForApproval::Never => {
            // We allow the command to run, relying on the sandbox for protection.
            Decision::Allow
        }
        AskForApproval::OnRequest | AskForApproval::Granular { .. } => match file_system_sandbox {
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => {
                // The user has indicated we should "just run" commands in their unrestricted
                // environment, so we do so since the command has not been flagged as dangerous.
                // Under `Granular`, prompt-vs-reject is handled by `prompt_is_rejected_by_policy`.
                Decision::Allow
            }
            FileSystemSandboxKind::Restricted => {
                // In restricted sandboxes, do not prompt for non-escalated, non-dangerous
                // commands; let the sandbox enforce restrictions without a user prompt.
                if sandbox_permissions.requests_sandbox_override() {
                    Decision::Prompt
                } else {
                    Decision::Allow
                }
            }
        },
        // Projects marked untrusted require approval for every command that is not explicitly
        // allowed by an exec policy rule.
        _ => Decision::Prompt,
    }
}

/// The commands a policy judges for `command`: the plain commands of its shell script when it has
/// one that can be split, or the command whole.
#[must_use]
pub fn commands_for_exec_policy(command: &[String]) -> Vec<Vec<String>> {
    if let Some(commands) = parse_shell_lc_plain_commands(command)
        && !commands.is_empty()
    {
        return commands;
    }

    vec![command.to_vec()]
}

/// Renders a rules loading error for display, pointing at the line a parse error is on or around:
/// Codex's `format_exec_policy_error_with_source`.
#[must_use]
pub fn format_exec_policy_error_with_source(error: &ExecPolicyError) -> String {
    match error {
        ExecPolicyError::ParsePolicy { path, source } => {
            let rendered_source = source.to_string();
            let structured_location = source
                .location()
                .map(|location| (PathBuf::from(location.path), location.range.start.line));
            let parsed_location = parse_starlark_line_from_message(&rendered_source);
            let location = match (structured_location, parsed_location) {
                (Some((_, 1)), Some((parsed_path, parsed_line))) if parsed_line > 1 => {
                    Some((parsed_path, parsed_line))
                }
                (Some(structured), _) => Some(structured),
                (None, parsed) => parsed,
            };
            let message = exec_policy_message_for_display(source);
            match location {
                Some((path, line)) => {
                    format!(
                        "{}:{}: {} (problem is on or around line {})",
                        path.display(),
                        line,
                        message,
                        line
                    )
                }
                None => format!("{path}: {message}"),
            }
        }
        _ => error.to_string(),
    }
}

fn exec_policy_message_for_display(source: &ExecPolicyRuleError) -> String {
    let message = source.to_string();
    if let Some(line) = message
        .lines()
        .find(|line| line.trim_start().starts_with("error: "))
    {
        return line.to_owned();
    }
    if let Some(first_line) = message.lines().next()
        && let Some((_, detail)) = first_line.rsplit_once(": starlark error: ")
    {
        return detail.trim().to_string();
    }

    message
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The path and one-based line a Starlark error message starts with, if it names a nonzero line.
#[must_use]
pub fn parse_starlark_line_from_message(message: &str) -> Option<(PathBuf, usize)> {
    let first_line = message.lines().next()?.trim();
    let (path_and_position, _) = first_line.rsplit_once(": starlark error:")?;

    let mut parts = path_and_position.rsplitn(3, ':');
    let _column = parts.next()?.parse::<usize>().ok()?;
    let line = parts.next()?.parse::<usize>().ok()?;
    let path = PathBuf::from(parts.next()?);

    if line == 0 {
        return None;
    }

    Some((path, line))
}

/// Loads the rules `sources` name, returning a rules parse failure as a warning beside the
/// requirements policy alone. Read failures are errors.
pub async fn load_exec_policy_with_warning(
    sources: &ExecPolicySources,
) -> Result<(Policy, Option<ExecPolicyError>), ExecPolicyError> {
    match load_exec_policy(sources).await {
        Ok(policy) => Ok((policy, None)),
        Err(err @ ExecPolicyError::ParsePolicy { .. }) => {
            let policy = sources
                .requirements()
                .map_or_else(Policy::empty, |policy| policy.as_ref().clone());
            Ok((policy, Some(err)))
        }
        Err(err) => Err(err),
    }
}

/// The warning loading `sources` would give, if any.
pub async fn check_execpolicy_for_warnings(
    sources: &ExecPolicySources,
) -> Result<Option<ExecPolicyError>, ExecPolicyError> {
    let (_, warning) = load_exec_policy_with_warning(sources).await?;
    Ok(warning)
}

/// Loads the rules `sources` name, failing on any read or parse error.
pub async fn load_exec_policy(sources: &ExecPolicySources) -> Result<Policy, ExecPolicyError> {
    // Iterate the folders in increasing order of precedence, adding the *.rules from each, so that
    // higher-precedence folders can add to rules defined in lower-precedence ones.
    let mut policy_paths = Vec::new();
    for config_folder in sources.config_folders() {
        let policy_dir = config_folder.join(RULES_DIR_NAME);
        let layer_policy_paths = collect_policy_files(&policy_dir).await?;
        policy_paths.extend(layer_policy_paths);
    }
    tracing::trace!(
        policy_paths = ?policy_paths,
        "loaded exec policies"
    );

    let mut parser = PolicyParser::new();
    for policy_path in &policy_paths {
        let contents = tokio::fs::read_to_string(policy_path)
            .await
            .map_err(|source| ExecPolicyError::ReadFile {
                path: policy_path.clone(),
                source,
            })?;
        let identifier = policy_path.to_string_lossy().to_string();
        parser
            .parse(&identifier, &contents)
            .map_err(|source| ExecPolicyError::ParsePolicy {
                path: identifier,
                source,
            })?;
    }

    let policy = parser.build();
    tracing::debug!("loaded rules from {} files", policy_paths.len());

    let Some(requirements_policy) = sources.requirements() else {
        return Ok(policy);
    };

    Ok(policy.merge_overlay(requirements_policy.as_ref()))
}

/// The `*.rules` files directly in `dir`, sorted; none when `dir` does not exist.
pub async fn collect_policy_files(dir: impl AsRef<Path>) -> Result<Vec<PathBuf>, ExecPolicyError> {
    let dir = dir.as_ref();
    let mut read_dir = match tokio::fs::read_dir(dir).await {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ExecPolicyError::ReadDir {
                dir: dir.to_path_buf(),
                source,
            });
        }
    };

    let mut policy_paths = Vec::new();
    while let Some(entry) =
        read_dir
            .next_entry()
            .await
            .map_err(|source| ExecPolicyError::ReadDir {
                dir: dir.to_path_buf(),
                source,
            })?
    {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .await
            .map_err(|source| ExecPolicyError::ReadDir {
                dir: dir.to_path_buf(),
                source,
            })?;

        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == RULE_EXTENSION)
            && file_type.is_file()
        {
            policy_paths.push(path);
        }
    }

    policy_paths.sort();

    tracing::debug!(
        "loaded {} .rules files in {}",
        policy_paths.len(),
        dir.display()
    );
    Ok(policy_paths)
}

/// Extract the dangerous command a heuristic decision was made on.
fn dangerous_command_match_for_heuristics(
    evaluation: &Evaluation,
    decision: Decision,
) -> Option<DangerousCommandMatch> {
    evaluation
        .matched_rules
        .iter()
        .find_map(|rule_match| match rule_match {
            RuleMatch::HeuristicsRuleMatch {
                command,
                decision: matched_decision,
            } if *matched_decision == decision => dangerous_command_match(command),
            _ => None,
        })
}

/// Derive a proposed execpolicy amendment when a command requires user approval
/// - If any execpolicy rule prompts, return None, because an amendment would not skip that policy
///   requirement.
/// - Otherwise return the first heuristics Prompt.
fn try_derive_execpolicy_amendment_for_prompt_rules(
    matched_rules: &[RuleMatch],
) -> Option<ExecPolicyAmendment> {
    if matched_rules
        .iter()
        .any(|rule_match| is_policy_match(rule_match) && rule_match.decision() == Decision::Prompt)
    {
        return None;
    }

    matched_rules
        .iter()
        .find_map(|rule_match| match rule_match {
            RuleMatch::HeuristicsRuleMatch {
                command,
                decision: Decision::Prompt,
            } => Some(ExecPolicyAmendment::new(command.iter().cloned())),
            _ => None,
        })
}

/// - Note: we only use this amendment when the command fails to run in sandbox and the user is
///   asked to run it outside the sandbox.
/// - The purpose of this amendment is to bypass sandbox for similar commands in the future.
/// - If any execpolicy rule matches, return None, because we would already be running command
///   outside the sandbox.
fn try_derive_execpolicy_amendment_for_allow_rules(
    matched_rules: &[RuleMatch],
) -> Option<ExecPolicyAmendment> {
    if matched_rules.iter().any(is_policy_match) {
        return None;
    }

    matched_rules
        .iter()
        .find_map(|rule_match| match rule_match {
            RuleMatch::HeuristicsRuleMatch {
                command,
                decision: Decision::Allow,
            } => Some(ExecPolicyAmendment::new(command.iter().cloned())),
            _ => None,
        })
}

/// The amendment a model's requested `prefix_rule` becomes: none when it is empty or a banned
/// suggestion, when a rule already matches, or when adding it would not allow every command.
#[must_use]
pub fn derive_requested_execpolicy_amendment_from_prefix_rule(
    prefix_rule: Option<&[String]>,
    matched_rules: &[RuleMatch],
    exec_policy: &Policy,
    commands: &[Vec<String>],
    exec_policy_fallback: &impl Fn(&[String]) -> Decision,
    match_options: &MatchOptions,
) -> Option<ExecPolicyAmendment> {
    let prefix_rule = prefix_rule?;
    if prefix_rule.is_empty() {
        return None;
    }
    if BANNED_PREFIX_SUGGESTIONS.iter().any(|banned| {
        prefix_rule.len() == banned.len()
            && prefix_rule
                .iter()
                .map(String::as_str)
                .eq(banned.iter().copied())
    }) {
        return None;
    }

    // if any policy rule already matches, don't suggest an additional rule that might conflict or
    // not apply
    if matched_rules.iter().any(is_policy_match) {
        return None;
    }

    if prefix_rule_would_approve_all_commands(
        exec_policy,
        prefix_rule,
        commands,
        exec_policy_fallback,
        match_options,
    ) {
        Some(ExecPolicyAmendment::new(prefix_rule.iter().cloned()))
    } else {
        None
    }
}

fn prefix_rule_would_approve_all_commands(
    exec_policy: &Policy,
    prefix_rule: &[String],
    commands: &[Vec<String>],
    exec_policy_fallback: &impl Fn(&[String]) -> Decision,
    match_options: &MatchOptions,
) -> bool {
    let mut policy_with_prefix_rule = exec_policy.clone();
    if policy_with_prefix_rule
        .add_prefix_rule(prefix_rule, Decision::Allow)
        .is_err()
    {
        return false;
    }

    commands.iter().all(|command| {
        policy_with_prefix_rule
            .check_with_options(command, exec_policy_fallback, match_options)
            .decision
            == Decision::Allow
    })
}

/// Only return a reason when a policy rule drove the prompt decision.
fn derive_prompt_reason(command_args: &[String], evaluation: &Evaluation) -> Option<String> {
    let command = render_shlex_command(command_args);

    let most_specific_prompt = evaluation
        .matched_rules
        .iter()
        .filter_map(|rule_match| match rule_match {
            RuleMatch::PrefixRuleMatch {
                matched_prefix,
                decision: Decision::Prompt,
                justification,
                ..
            } => Some((matched_prefix.len(), justification.as_deref())),
            _ => None,
        })
        .max_by_key(|(matched_prefix_len, _)| *matched_prefix_len);

    match most_specific_prompt {
        Some((_matched_prefix_len, Some(justification))) => {
            Some(format!("`{command}` requires approval: {justification}"))
        }
        Some((_matched_prefix_len, None)) => {
            Some(format!("`{command}` requires approval by policy"))
        }
        None => None,
    }
}

fn render_shlex_command(args: &[String]) -> String {
    shlex_try_join(args.iter().map(String::as_str)).unwrap_or_else(|_| args.join(" "))
}

/// Derive a string explaining why the command was forbidden. If `justification` is set by the
/// user, this can contain instructions with recommended alternatives, for example.
fn derive_forbidden_reason(
    command_args: &[String],
    evaluation: &Evaluation,
    dangerous_command_match: Option<DangerousCommandMatch>,
) -> String {
    let command = render_shlex_command(command_args);

    let most_specific_forbidden = evaluation
        .matched_rules
        .iter()
        .filter_map(|rule_match| match rule_match {
            RuleMatch::PrefixRuleMatch {
                matched_prefix,
                decision: Decision::Forbidden,
                justification,
                ..
            } => Some((matched_prefix, justification.as_deref())),
            _ => None,
        })
        .max_by_key(|(matched_prefix, _)| matched_prefix.len());

    match most_specific_forbidden {
        Some((_matched_prefix, Some(justification))) => {
            format!("`{command}` rejected: {justification}")
        }
        Some((matched_prefix, None)) => {
            let prefix = render_shlex_command(matched_prefix);
            format!("`{command}` rejected: policy forbids commands starting with `{prefix}`")
        }
        None => {
            if let Some(dangerous_command_match) = dangerous_command_match {
                let reason = dangerous_command_rejection_reason(dangerous_command_match);
                format!("`{command}` rejected: {reason}")
            } else {
                format!("`{command}` rejected: blocked by policy")
            }
        }
    }
}

/// The reason a prompt the approval policy refuses is reported with: a forced `rm` explains
/// itself, anything else keeps the policy's reason.
#[must_use]
pub fn derive_rejected_prompt_reason(
    fallback_reason: &str,
    dangerous_command_match: Option<DangerousCommandMatch>,
) -> String {
    match dangerous_command_match {
        Some(dangerous_command_match @ DangerousCommandMatch::ForcedRm) => {
            dangerous_command_rejection_reason(dangerous_command_match).to_string()
        }
        Some(DangerousCommandMatch::Other) | None => fallback_reason.to_string(),
    }
}

const fn dangerous_command_rejection_reason(
    dangerous_command_match: DangerousCommandMatch,
) -> &'static str {
    match dangerous_command_match {
        DangerousCommandMatch::ForcedRm => {
            "rm -f style commands are not permitted. Use a safer approach"
        }
        DangerousCommandMatch::Other => "blocked by policy",
    }
}
