// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Full-value managed MCP matching without constructing live connections.

use super::McpServerIdentity;
use super::McpServerRequirement;
use super::McpServerValueMatcher;
use super::mcp_types::McpServerConfig;
use super::mcp_types::McpServerTransportConfig;
use regex_lite::Regex;

fn compile_full_regex(expression: &str) -> Result<Regex, String> {
    Regex::new(&format!(r"\A(?:{expression})\z")).map_err(|err| {
        format!("regex `{expression}` cannot be used for full-value matching: {err}")
    })
}

fn validate_value_matcher(matcher: &McpServerValueMatcher) -> Result<(), String> {
    let McpServerValueMatcher::Regex { expression } = matcher else {
        return Ok(());
    };

    Regex::new(expression).map_err(|err| format!("invalid regex `{expression}`: {err}"))?;
    compile_full_regex(expression).map(|_| ())
}

fn matches_value(matcher: &McpServerValueMatcher, candidate: &str) -> bool {
    match matcher {
        McpServerValueMatcher::Exact { value } => candidate == value,
        McpServerValueMatcher::Prefix { value } => candidate.starts_with(value),
        McpServerValueMatcher::Regex { expression } => compile_full_regex(expression)
            .ok()
            .is_some_and(|regex| regex.is_match(candidate)),
    }
}

/// Validates all argument or URL matchers, including their full-value regex wrappers.
pub fn validate_mcp_server_requirement(requirement: &McpServerRequirement) -> Result<(), String> {
    match requirement {
        McpServerRequirement::Identity { .. } => Ok(()),
        McpServerRequirement::Command(matcher) => {
            for (index, arg) in matcher.args.iter().enumerate() {
                validate_value_matcher(arg)
                    .map_err(|err| format!("invalid argument matcher at index {index}: {err}"))?;
            }
            Ok(())
        }
        McpServerRequirement::Url(matcher) => validate_value_matcher(matcher),
    }
}

impl McpServerConfig {
    /// Whether the configured transport satisfies the exact or matcher-based requirement.
    #[must_use]
    pub fn matches_requirement(&self, requirement: &McpServerRequirement) -> bool {
        // HTTP requirements intentionally authorize the complete server configuration by URL.
        match (requirement, &self.transport) {
            (
                McpServerRequirement::Identity {
                    identity:
                        McpServerIdentity::Command {
                            command: want_command,
                        },
                },
                McpServerTransportConfig::Stdio {
                    command: got_command,
                    ..
                },
            ) => got_command == want_command,
            (
                McpServerRequirement::Identity {
                    identity: McpServerIdentity::Url { url: want_url },
                },
                McpServerTransportConfig::StreamableHttp { url: got_url, .. },
            ) => got_url == want_url,
            (
                McpServerRequirement::Command(matcher),
                McpServerTransportConfig::Stdio { command, args, .. },
            ) => {
                matcher.executable == *command
                    && matcher.args.len() == args.len()
                    && matcher
                        .args
                        .iter()
                        .zip(args)
                        .all(|(matcher, arg)| matches_value(matcher, arg))
            }
            (
                McpServerRequirement::Url(matcher),
                McpServerTransportConfig::StreamableHttp { url, .. },
            ) => matches_value(matcher, url),
            _ => false,
        }
    }
}

/// Access to matcher internals for the external upstream test suite.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use super::McpServerValueMatcher;
    /// Applies one matcher to the complete candidate value.
    #[must_use]
    pub fn matches_value(matcher: &McpServerValueMatcher, candidate: &str) -> bool {
        super::matches_value(matcher, candidate)
    }
    /// Validates full-match regular expression compatibility.
    pub fn validate_value_matcher(matcher: &McpServerValueMatcher) -> Result<(), String> {
        super::validate_value_matcher(matcher)
    }
}
