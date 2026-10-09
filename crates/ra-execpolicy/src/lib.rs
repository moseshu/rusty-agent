//! # `ra-execpolicy`
//!
//! Command execution policy: prefix rules written in Starlark that decide whether a command may
//! run, needs approval, or is forbidden. A port of Codex's `execpolicy` crate
//! (`codex-rs/execpolicy`), keeping its parser, dialect, rule semantics and file amendments.
//!
//! # What a policy decides
//!
//! A rules file calls three builtins:
//!
//! - `prefix_rule(pattern, decision, match, not_match, justification)` adds a rule for commands
//!   whose leading tokens match `pattern`. Rules accumulate: every rule that matches a command
//!   contributes, and the strictest decision wins (`forbidden` over `prompt` over `allow`). The
//!   first token may list alternatives, which expand into one rule per program; later tokens may
//!   list alternatives too, without expanding. `match` and `not_match` examples are checked when
//!   the file is parsed.
//! - `network_rule(host, protocol, decision, justification)` records a host rule for a managed
//!   network proxy.
//! - `host_executable(name, paths)` lists the absolute paths an executable name may resolve to.
//!   A later definition of the same name replaces the earlier one.
//!
//! [`Policy::check`] evaluates one command, and [`Policy::check_multiple`] the commands a shell
//! script splits into; a command no rule matches is decided by the caller's heuristics. Deciding
//! what those heuristics are, where rules files are loaded from and what an amendment is proposed
//! for belongs to the product that runs commands.
//!
//! # Not ported
//!
//! Codex's `execpolicycheck` command-line subcommand and its binary, and its one-shot
//! `prefix_rule_migration`, which removes allow rules written by earlier Codex versions. Nothing
//! here enforces network rules: they are parsed and kept for a managed network proxy, which this
//! project does not have.
//!
//! **Stability**: `Internal`. A service crate for the coding product's command approvals; its API
//! follows Codex's and may change with it.

pub(crate) mod absolute_path;
pub(crate) mod amend;
pub(crate) mod decision;
pub(crate) mod error;
mod executable_name;
pub(crate) mod parser;
pub(crate) mod policy;
pub mod rule;

pub use absolute_path::AbsolutePathBuf;
pub use amend::{AmendError, blocking_append_allow_prefix_rule, blocking_append_network_rule};
pub use decision::Decision;
pub use error::{Error, ErrorLocation, Result, TextPosition, TextRange};
pub use parser::PolicyParser;
pub use policy::{Evaluation, MatchOptions, Policy, RequirementsExecPolicy};
pub use rule::{
    NetworkRule, NetworkRuleProtocol, PatternToken, PrefixPattern, PrefixRule, Rule, RuleMatch,
    RuleRef,
};
