// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Managed MCP identities and positional matchers supplied by environment owners.

use serde::Deserialize;
use std::collections::BTreeMap;

/// Additional managed MCP restrictions supplied by an environment owner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentMcpPolicy {
    /// Optional managed restrictions on configured servers; an empty map permits none.
    pub servers: Option<BTreeMap<String, McpServerRequirement>>,
    /// Optional per-plugin restrictions, keyed by canonical plugin identifier.
    pub plugins: Option<BTreeMap<String, PluginMcpRequirements>>,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
/// The released exact executable or URL identity form.
pub enum McpServerIdentity {
    /// Matches the executable spelling, independently of its arguments.
    Command {
        /// The exact executable spelling.
        command: String,
    },
    /// Matches the complete HTTP URL spelling.
    Url {
        /// The exact HTTP URL spelling.
        url: String,
    },
}

/// String matching operations available to managed MCP server matchers.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "match", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpServerValueMatcher {
    /// Matches the entire value exactly.
    Exact {
        /// The complete allowed value.
        value: String,
    },
    /// Matches values starting with the specified prefix.
    Prefix {
        /// The allowed prefix.
        value: String,
    },
    /// Matches the entire value using a validated regular expression.
    Regex {
        /// The regular expression for full-value matching.
        expression: String,
    },
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// A fixed executable paired with exact-position argument matchers.
pub struct McpServerCommandMatcher {
    /// The complete executable spelling.
    pub executable: String,
    /// One matcher per argument, preserving position and argument count.
    pub args: Vec<McpServerValueMatcher>,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RawMcpServerCommandIdentity {
    command: McpServerCommandMatcher,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RawMcpServerUrlIdentity {
    url: McpServerValueMatcher,
}

/// A requirement for one named MCP server.
///
/// The `Identity` variant preserves the released exact-match contract. The
/// command and URL variants are the normalized matcher-based forms accepted
/// under the `identity` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerRequirement {
    /// The released exact identity contract.
    Identity {
        /// The released exact identity.
        identity: McpServerIdentity,
    },
    /// The executable plus positional argument matchers.
    Command(McpServerCommandMatcher),
    /// A full-value matcher applied to the HTTP URL.
    Url(McpServerValueMatcher),
}

#[derive(Deserialize)]
struct RawMcpServerRequirement {
    identity: RawMcpServerIdentity,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawMcpServerIdentity {
    Exact(McpServerIdentity),
    Command(RawMcpServerCommandIdentity),
    Url(RawMcpServerUrlIdentity),
}

impl<'de> Deserialize<'de> for McpServerRequirement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let RawMcpServerRequirement { identity } =
            RawMcpServerRequirement::deserialize(deserializer)?;
        match identity {
            RawMcpServerIdentity::Exact(identity) => Ok(Self::Identity { identity }),
            RawMcpServerIdentity::Command(matcher) => Ok(Self::Command(matcher.command)),
            RawMcpServerIdentity::Url(matcher) => Ok(Self::Url(matcher.url)),
        }
    }
}

/// Managed MCP server requirements for one plugin.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginMcpRequirements {
    /// Optional restrictions on this plugin's servers; an empty map permits none.
    pub mcp_servers: Option<BTreeMap<String, McpServerRequirement>>,
}

impl PluginMcpRequirements {
    /// Whether this entry contains no server restrictions.
    pub fn is_empty(&self) -> bool {
        self.mcp_servers.as_ref().is_none_or(BTreeMap::is_empty)
    }
}
