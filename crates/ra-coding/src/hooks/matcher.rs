// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Compiled hook matchers retain their original spelling for configuration identity.
//! Regex parsing happens during discovery; dispatch only evaluates admitted matchers.

use ra_core::hook::HookEventName;

#[derive(Debug, Clone)]
/// A compiled matcher retaining its original configuration spelling.
pub enum HookMatcher {
    /// An empty or wildcard pattern matching every occurrence, even without an input.
    All(String),
    /// Case-sensitive exact names separated by literal pipe characters.
    Exact(String),
    /// A regular expression evaluated against a present input.
    Regex(regex::Regex),
}

impl HookMatcher {
    /// Compiles wildcard, simple-name alternatives or regular expression syntax.
    pub fn new(pattern: &str) -> Result<Self, regex::Error> {
        if pattern.is_empty() || pattern == "*" {
            Ok(Self::All(pattern.to_string()))
        } else if pattern
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '|')
        {
            Ok(Self::Exact(pattern.to_string()))
        } else {
            regex::Regex::new(pattern).map(Self::Regex)
        }
    }

    /// Returns the original spelling used for configuration identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::All(pattern) | Self::Exact(pattern) => pattern,
            Self::Regex(regex) => regex.as_str(),
        }
    }

    /// Whether this admitted matcher accepts the supplied occurrence input.
    #[must_use]
    pub fn matches(&self, input: Option<&str>) -> bool {
        match self {
            Self::All(_) => true,
            Self::Exact(pattern) => {
                input.is_some_and(|input| pattern.split('|').any(|candidate| candidate == input))
            }
            Self::Regex(regex) => input.is_some_and(|input| regex.is_match(input)),
        }
    }
}

impl PartialEq for HookMatcher {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for HookMatcher {}

/// Retains matchers only for events whose dispatch has an occurrence identity.
#[must_use]
pub fn matcher_pattern_for_event(event_name: HookEventName, matcher: Option<&str>) -> Option<&str> {
    match event_name {
        HookEventName::PreToolUse
        | HookEventName::PermissionRequest
        | HookEventName::PostToolUse
        | HookEventName::SessionStart
        | HookEventName::SessionEnd
        | HookEventName::SubagentStart
        | HookEventName::SubagentStop
        | HookEventName::PreCompact
        | HookEventName::PostCompact => matcher,
        _ => None,
    }
}

/// An absent matcher accepts every occurrence.
#[must_use]
pub fn matches_matcher(matcher: Option<&HookMatcher>, input: Option<&str>) -> bool {
    matcher.is_none_or(|matcher| matcher.matches(input))
}

/// Canonical tool identity precedes any explicit matcher aliases.
#[must_use]
pub fn matcher_inputs<'a>(tool_name: &'a str, matcher_aliases: &'a [String]) -> Vec<&'a str> {
    // Keep the canonical name first so matcher previews and execution preserve
    // the same primary identity that hook stdin will serialize.
    std::iter::once(tool_name)
        .chain(matcher_aliases.iter().map(String::as_str))
        .collect()
}
