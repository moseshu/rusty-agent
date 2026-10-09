//! Rules and their matches: Codex's `execpolicy/src/rule.rs`.

use std::{any::Any, fmt::Debug, sync::Arc};

use serde::{Deserialize, Serialize};
use shlex::try_join;

use crate::{
    absolute_path::AbsolutePathBuf,
    decision::Decision,
    error::{Error, Result},
    policy::{MatchOptions, Policy},
};

/// Matches a single command token, either a fixed string or one of several allowed alternatives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternToken {
    /// One token.
    Single(String),
    /// Any one of several tokens.
    Alts(Vec<String>),
}

impl PatternToken {
    fn matches(&self, token: &str) -> bool {
        match self {
            Self::Single(expected) => expected == token,
            Self::Alts(alternatives) => alternatives.iter().any(|alt| alt == token),
        }
    }

    /// The tokens this position accepts.
    #[must_use]
    pub fn alternatives(&self) -> &[String] {
        match self {
            Self::Single(expected) => std::slice::from_ref(expected),
            Self::Alts(alternatives) => alternatives,
        }
    }
}

/// Prefix matcher for commands with support for alternative match tokens.
/// First token is fixed since we key by the first token in policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixPattern {
    /// The program, the first token.
    pub first: Arc<str>,
    /// The tokens after it.
    pub rest: Arc<[PatternToken]>,
}

impl PrefixPattern {
    /// The leading tokens of `cmd` this pattern matches, if it matches.
    #[must_use]
    pub fn matches_prefix(&self, cmd: &[String]) -> Option<Vec<String>> {
        let pattern_length = self.rest.len() + 1;
        if cmd.len() < pattern_length || cmd[0] != self.first.as_ref() {
            return None;
        }

        for (pattern_token, cmd_token) in self.rest.iter().zip(&cmd[1..pattern_length]) {
            if !pattern_token.matches(cmd_token) {
                return None;
            }
        }

        Some(cmd[..pattern_length].to_vec())
    }
}

/// Why a command got its decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleMatch {
    /// A prefix rule matched.
    PrefixRuleMatch {
        /// The leading tokens it matched.
        #[serde(rename = "matchedPrefix")]
        matched_prefix: Vec<String>,
        /// Its decision.
        decision: Decision,
        /// The absolute program path it matched by basename, when host executables resolved it.
        #[serde(rename = "resolvedProgram", skip_serializing_if = "Option::is_none")]
        resolved_program: Option<AbsolutePathBuf>,
        /// Optional rationale for why this rule exists.
        ///
        /// This can be supplied for any decision and may be surfaced in different contexts
        /// (e.g., prompt reasons or rejection messages).
        #[serde(skip_serializing_if = "Option::is_none")]
        justification: Option<String>,
    },
    /// No rule matched, and the caller's heuristics decided.
    HeuristicsRuleMatch {
        /// The command they decided.
        command: Vec<String>,
        /// Their decision.
        decision: Decision,
    },
}

impl RuleMatch {
    /// The decision this match contributes.
    #[must_use]
    pub fn decision(&self) -> Decision {
        match self {
            Self::PrefixRuleMatch { decision, .. } | Self::HeuristicsRuleMatch { decision, .. } => {
                *decision
            }
        }
    }

    /// Records the absolute program path a prefix rule matched by basename.
    #[must_use]
    pub fn with_resolved_program(self, resolved_program: &AbsolutePathBuf) -> Self {
        match self {
            Self::PrefixRuleMatch {
                matched_prefix,
                decision,
                justification,
                ..
            } => Self::PrefixRuleMatch {
                matched_prefix,
                decision,
                resolved_program: Some(resolved_program.clone()),
                justification,
            },
            other @ Self::HeuristicsRuleMatch { .. } => other,
        }
    }
}

/// A rule matching commands by their leading tokens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixRule {
    /// The commands it matches.
    pub pattern: PrefixPattern,
    /// What it decides about them.
    pub decision: Decision,
    /// Why it exists, shown in prompts and rejections.
    pub justification: Option<String>,
}

/// The protocol a network rule covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkRuleProtocol {
    /// Plain HTTP.
    Http,
    /// HTTPS through `CONNECT`.
    Https,
    /// SOCKS5 over TCP.
    Socks5Tcp,
    /// SOCKS5 over UDP.
    Socks5Udp,
}

impl NetworkRuleProtocol {
    /// Parses the spelling a rules file uses, including Codex's aliases for HTTPS.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "http" => Ok(Self::Http),
            "https" | "https_connect" | "http-connect" => Ok(Self::Https),
            "socks5_tcp" => Ok(Self::Socks5Tcp),
            "socks5_udp" => Ok(Self::Socks5Udp),
            other => Err(Error::InvalidRule(format!(
                "network_rule protocol must be one of http, https, socks5_tcp, socks5_udp (got {other})"
            ))),
        }
    }

    /// The spelling a rules file is written with.
    #[must_use]
    pub fn as_policy_string(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks5Tcp => "socks5_tcp",
            Self::Socks5Udp => "socks5_udp",
        }
    }
}

/// A host rule for a managed network proxy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkRule {
    /// The normalized host.
    pub host: String,
    /// The protocol it covers.
    pub protocol: NetworkRuleProtocol,
    /// What it decides.
    pub decision: Decision,
    /// Why it exists.
    pub justification: Option<String>,
}

pub(crate) fn normalize_network_rule_host(raw: &str) -> Result<String> {
    let mut host = raw.trim();
    if host.is_empty() {
        return Err(Error::InvalidRule(
            "network_rule host cannot be empty".to_string(),
        ));
    }
    if host.contains("://") || host.contains('/') || host.contains('?') || host.contains('#') {
        return Err(Error::InvalidRule(
            "network_rule host must be a hostname or IP literal (without scheme or path)"
                .to_string(),
        ));
    }

    if let Some(stripped) = host.strip_prefix('[') {
        let Some((inside, rest)) = stripped.split_once(']') else {
            return Err(Error::InvalidRule(
                "network_rule host has an invalid bracketed IPv6 literal".to_string(),
            ));
        };
        let port_ok = rest
            .strip_prefix(':')
            .is_some_and(|port| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()));
        if !rest.is_empty() && !port_ok {
            return Err(Error::InvalidRule(format!(
                "network_rule host contains an unsupported suffix: {raw}"
            )));
        }
        host = inside;
    } else if host.matches(':').count() == 1
        && let Some((candidate, port)) = host.rsplit_once(':')
        && !candidate.is_empty()
        && !port.is_empty()
        && port.chars().all(|c| c.is_ascii_digit())
    {
        host = candidate;
    }

    let normalized = host.trim_end_matches('.').trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(Error::InvalidRule(
            "network_rule host cannot be empty".to_string(),
        ));
    }
    if normalized.contains('*') {
        return Err(Error::InvalidRule(
            "network_rule host must be a specific host; wildcards are not allowed".to_string(),
        ));
    }
    if normalized.chars().any(char::is_whitespace) {
        return Err(Error::InvalidRule(
            "network_rule host cannot contain whitespace".to_string(),
        ));
    }

    Ok(normalized)
}

/// A rule a policy holds, keyed by the program it applies to.
pub trait Rule: Any + Debug + Send + Sync {
    /// The program, a command's first token.
    fn program(&self) -> &str;

    /// What this rule makes of `cmd`, if it applies.
    fn matches(&self, cmd: &[String]) -> Option<RuleMatch>;

    /// This rule, for downcasting to its concrete type.
    fn as_any(&self) -> &dyn Any;
}

/// A shared rule.
pub type RuleRef = Arc<dyn Rule>;

impl Rule for PrefixRule {
    fn program(&self) -> &str {
        self.pattern.first.as_ref()
    }

    fn matches(&self, cmd: &[String]) -> Option<RuleMatch> {
        self.pattern
            .matches_prefix(cmd)
            .map(|matched_prefix| RuleMatch::PrefixRuleMatch {
                matched_prefix,
                decision: self.decision,
                resolved_program: None,
                justification: self.justification.clone(),
            })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Count how many rules match each provided example and error if any example is unmatched.
pub(crate) fn validate_match_examples(
    policy: &Policy,
    rules: &[RuleRef],
    matches: &[Vec<String>],
) -> Result<()> {
    let mut unmatched_examples = Vec::new();
    let options = MatchOptions {
        resolve_host_executables: true,
    };

    for example in matches {
        if !policy
            .matches_for_command_with_options(example, /*heuristics_fallback*/ None, &options)
            .is_empty()
        {
            continue;
        }

        unmatched_examples.push(
            try_join(example.iter().map(String::as_str))
                .unwrap_or_else(|_| "unable to render example".to_string()),
        );
    }

    if unmatched_examples.is_empty() {
        Ok(())
    } else {
        Err(Error::ExampleDidNotMatch {
            rules: rules.iter().map(|rule| format!("{rule:?}")).collect(),
            examples: unmatched_examples,
            location: None,
        })
    }
}

/// Ensure that no rule matches any provided negative example.
pub(crate) fn validate_not_match_examples(
    policy: &Policy,
    _rules: &[RuleRef],
    not_matches: &[Vec<String>],
) -> Result<()> {
    let options = MatchOptions {
        resolve_host_executables: true,
    };

    for example in not_matches {
        if let Some(rule) = policy
            .matches_for_command_with_options(example, /*heuristics_fallback*/ None, &options)
            .first()
        {
            return Err(Error::ExampleDidMatch {
                rule: format!("{rule:?}"),
                example: try_join(example.iter().map(String::as_str))
                    .unwrap_or_else(|_| "unable to render example".to_string()),
                location: None,
            });
        }
    }

    Ok(())
}
