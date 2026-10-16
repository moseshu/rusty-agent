// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Raw MCP identities and deterministic model-visible names.
//!
//! Two naming contracts are provided. The Python SDK's bare function names are the default:
//! original MCP names, or `mcp_{server}__{tool}` when server names are included. Codex's
//! namespaced names are an explicitly selected extension for endpoints that carry namespaces.

use ra_core::error::{Error, Result};
use rmcp::model::Tool;
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::Write as _,
};
use tracing::warn;

const LEGACY_MCP_TOOL_NAME_PREFIX: &str = "mcp__";

/// A discovered tool with its protocol identity and allocated model name.
#[derive(Clone, PartialEq)]
pub struct McpToolInfo {
    pub(crate) server_name: String,
    pub(crate) server_index: usize,
    pub(crate) callable_name: String,
    pub(crate) callable_namespace: String,
    pub(crate) namespaced: bool,
    pub(crate) namespace_description: Option<String>,
    pub(crate) tool: Tool,
    pub(crate) connector_id: Option<String>,
}

impl McpToolInfo {
    /// Starts with the protocol name as callable name and no model-facing namespace.
    pub fn new(server_name: impl Into<String>, tool: Tool) -> Self {
        let server_name = server_name.into();
        Self {
            callable_namespace: safe_server_name(&server_name),
            callable_name: tool.name.to_string(),
            server_name,
            server_index: 0,
            namespaced: false,
            namespace_description: None,
            tool,
            connector_id: None,
        }
    }
    /// Records the server's position in the discovery list. Python's names group tools by
    /// server in this order; discovery assigns it.
    #[must_use]
    pub const fn with_server_index(mut self, index: usize) -> Self {
        self.server_index = index;
        self
    }
    /// Uses the server's initialize instructions as the namespace description, as Codex does
    /// for regular MCP servers. Only namespaced names advertise it.
    #[must_use]
    pub fn with_namespace_description(mut self, description: Option<String>) -> Self {
        self.namespace_description = description.filter(|text| !text.trim().is_empty());
        self
    }
    /// Raw routing name of the server.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
    /// Unmodified MCP declaration, including its original name and schema.
    #[must_use]
    pub const fn tool(&self) -> &Tool {
        &self.tool
    }
    /// UI title, preferring explicit title over annotations.title, as in Python's metadata helper.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.tool
            .title
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.tool
                    .annotations
                    .as_ref()
                    .and_then(|a| a.title.as_deref())
                    .filter(|s| !s.is_empty())
            })
    }
    /// Long-form UI description without a title fallback.
    #[must_use]
    pub fn description_for_ui(&self) -> Option<&str> {
        self.tool.description.as_deref().filter(|s| !s.is_empty())
    }
    /// Model description follows Python's description, title, then empty-string precedence.
    #[must_use]
    pub fn description_for_model(&self) -> &str {
        self.description_for_ui()
            .or_else(|| self.title())
            .unwrap_or_default()
    }
    /// Allocated model-facing name.
    #[must_use]
    pub fn callable_name(&self) -> &str {
        &self.callable_name
    }
    /// Allocated model-facing namespace; present only after namespaced normalization.
    #[must_use]
    pub fn callable_namespace(&self) -> Option<&str> {
        self.namespaced.then_some(self.callable_namespace.as_str())
    }
    /// Description advertised with the namespace, when one is allocated.
    #[must_use]
    pub fn namespace_description(&self) -> Option<&str> {
        self.namespace_description
            .as_deref()
            .filter(|_| self.namespaced)
    }
}

// Ported from Python mcp/_logging.py: URL-derived display names exclude credentials.
pub(crate) fn safe_server_name(name: &str) -> String {
    let prefix = ["sse: ", "streamable_http: ", "streamable-http: "]
        .into_iter()
        .find(|prefix| name.starts_with(prefix))
        .unwrap_or_default();
    let candidate = name.strip_prefix(prefix).unwrap_or(name);
    if !candidate.to_ascii_lowercase().starts_with("http://")
        && !candidate.to_ascii_lowercase().starts_with("https://")
    {
        return name.to_owned();
    }
    let Ok(mut parsed) = url::Url::parse(candidate) else {
        return format!("{prefix}<invalid-url>");
    };
    if parsed.host_str().is_none() {
        return format!("{prefix}<invalid-url>");
    }
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    format!("{prefix}{parsed}")
}
impl std::fmt::Debug for McpToolInfo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpToolInfo")
            .field("server_name", &safe_server_name(&self.server_name))
            .field("callable_namespace", &self.callable_namespace())
            .field("callable_name", &self.callable_name)
            .field("tool", &self.tool)
            .finish_non_exhaustive()
    }
}

// Retain the upstream sanitization, including replacement of hyphens.
fn sanitize_responses_api_tool_name(name: &str) -> String {
    let value: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if value.is_empty() {
        "_".to_owned()
    } else {
        value
    }
}

/// Returns MCP tools with Codex's namespaced model-visible names.
///
/// Raw MCP server/tool names are kept on each [`McpToolInfo`] for protocol calls, while
/// `callable_namespace` / `callable_name` are sanitized and, when necessary, hashed so
/// every model-visible name is unique and <= 128 bytes including its delimiter. Each part
/// also fits the Responses adapter's 64-byte limit. Namespaces are an `OpenAI` Responses
/// feature, so these names are an extension rather than the provider-neutral default.
///
/// When `prefix_mcp_tool_names` is true, the historical `mcp__` namespace
/// prefix is added except for tools from `non_prefixed_mcp_tool_servers`.
pub fn normalize_tools_for_model_with_prefix<I>(
    tools: I,
    prefix_mcp_tool_names: bool,
    non_prefixed_mcp_tool_servers: &[String],
) -> Vec<McpToolInfo>
where
    I: IntoIterator<Item = McpToolInfo>,
{
    let mut seen_raw_names = HashSet::new();
    let mut candidates = Vec::new();
    for tool in tools {
        let raw_namespace_identity = format!(
            "{}\0{}\0{}",
            tool.server_name,
            tool.callable_namespace,
            tool.connector_id.as_deref().unwrap_or_default()
        );
        let raw_tool_identity = format!(
            "{}\0{}\0{}",
            raw_namespace_identity, tool.callable_name, tool.tool.name
        );
        if !seen_raw_names.insert(raw_tool_identity.clone()) {
            warn!("skipping duplicated tool {}", tool.tool.name);
            continue;
        }

        let callable_namespace = callable_namespace_with_prefix(
            &sanitize_responses_api_tool_name(&tool.callable_namespace),
            prefix_mcp_tool_names && !non_prefixed_mcp_tool_servers.contains(&tool.server_name),
        );

        candidates.push(CallableToolCandidate {
            callable_namespace,
            callable_name: sanitize_responses_api_tool_name(&tool.callable_name),
            raw_namespace_identity,
            raw_tool_identity,
            tool,
        });
    }

    allocate_callable_namespaces(&mut candidates);

    let mut tool_identities_by_base = HashMap::<(String, String), HashSet<String>>::new();
    for candidate in &candidates {
        tool_identities_by_base
            .entry((
                candidate.callable_namespace.clone(),
                candidate.callable_name.clone(),
            ))
            .or_default()
            .insert(candidate.raw_tool_identity.clone());
    }
    let colliding_tools = tool_identities_by_base
        .into_iter()
        .filter_map(|(key, identities)| (identities.len() > 1).then_some(key))
        .collect::<HashSet<_>>();
    for candidate in &mut candidates {
        if colliding_tools.contains(&(
            candidate.callable_namespace.clone(),
            candidate.callable_name.clone(),
        )) {
            candidate.callable_name =
                append_hash_suffix(&candidate.callable_name, &candidate.raw_tool_identity);
        }
    }

    candidates.sort_by(|left, right| left.raw_tool_identity.cmp(&right.raw_tool_identity));

    let mut used_names = HashSet::new();
    let mut model_tools = Vec::new();
    for mut candidate in candidates {
        let (callable_namespace, callable_name) = unique_callable_parts(
            &candidate.callable_namespace,
            &candidate.callable_name,
            &candidate.raw_tool_identity,
            &mut used_names,
            MCP_TOOL_NAME_DELIMITER.len(),
        );
        candidate.tool.callable_namespace = callable_namespace;
        candidate.tool.callable_name = callable_name;
        candidate.tool.namespaced = true;
        model_tools.push(candidate.tool);
    }
    model_tools
}

#[derive(Debug)]
struct CallableToolCandidate {
    tool: McpToolInfo,
    raw_namespace_identity: String,
    raw_tool_identity: String,
    callable_namespace: String,
    callable_name: String,
}

fn allocate_callable_namespaces(candidates: &mut [CallableToolCandidate]) {
    let mut namespace_identities_by_base = HashMap::<String, HashSet<String>>::new();
    for candidate in candidates.iter() {
        namespace_identities_by_base
            .entry(candidate.callable_namespace.clone())
            .or_default()
            .insert(candidate.raw_namespace_identity.clone());
    }
    let colliding_namespaces = namespace_identities_by_base
        .into_iter()
        .filter_map(|(namespace, identities)| (identities.len() > 1).then_some(namespace))
        .collect::<HashSet<_>>();
    // Allocate namespaces before tool names so every tool on one server shares its namespace.
    // Reserve unchanged names first, including names that resemble a generated hash suffix.
    let needs_namespace_hash = |candidate: &CallableToolCandidate| {
        candidate.callable_namespace.len() > MAX_CALLABLE_PART_LENGTH
            || colliding_namespaces.contains(&candidate.callable_namespace)
    };
    let mut used_namespaces: HashSet<String> = candidates
        .iter()
        .filter(|candidate| !needs_namespace_hash(candidate))
        .map(|candidate| candidate.callable_namespace.clone())
        .collect();
    let mut allocated_namespaces = HashMap::<String, String>::new();
    candidates.sort_by(|left, right| left.raw_tool_identity.cmp(&right.raw_tool_identity));
    for candidate in candidates.iter_mut() {
        if !needs_namespace_hash(candidate) {
            continue;
        }
        let namespace = allocated_namespaces
            .entry(candidate.raw_namespace_identity.clone())
            .or_insert_with(|| {
                let mut attempt = 0_u32;
                loop {
                    let hash_input = if attempt == 0 {
                        candidate.raw_namespace_identity.clone()
                    } else {
                        format!("{}\0{attempt}", candidate.raw_namespace_identity)
                    };
                    let namespace =
                        append_namespace_hash_suffix(&candidate.callable_namespace, &hash_input);
                    if used_namespaces.insert(namespace.clone()) {
                        break namespace;
                    }
                    attempt = attempt.saturating_add(1);
                }
            });
        candidate.callable_namespace.clone_from(namespace);
    }
}

const MCP_TOOL_NAME_DELIMITER: &str = "__";
const MAX_TOOL_NAME_LENGTH: usize = 128;
// The local Responses adapter validates each namespace and function name separately.
const MAX_CALLABLE_PART_LENGTH: usize = 64;
const CALLABLE_NAME_HASH_LEN: usize = 12;
fn callable_namespace_with_prefix(namespace: &str, prefix_mcp_tool_names: bool) -> String {
    if !prefix_mcp_tool_names || namespace.starts_with(LEGACY_MCP_TOOL_NAME_PREFIX) {
        namespace.to_string()
    } else {
        format!("{LEGACY_MCP_TOOL_NAME_PREFIX}{namespace}")
    }
}

fn sha1_hex(s: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(s.as_bytes());
    let sha1 = hasher.finalize();
    format!("{sha1:x}")
}

fn callable_name_hash_suffix(raw_identity: &str) -> String {
    let hash = sha1_hex(raw_identity);
    format!("_{}", &hash[..CALLABLE_NAME_HASH_LEN])
}

fn append_hash_suffix(value: &str, raw_identity: &str) -> String {
    format!("{value}{}", callable_name_hash_suffix(raw_identity))
}

fn append_namespace_hash_suffix(namespace: &str, raw_identity: &str) -> String {
    let suffix = callable_name_hash_suffix(raw_identity);
    if let Some(namespace) = namespace.strip_suffix(MCP_TOOL_NAME_DELIMITER) {
        format!(
            "{}{}{}",
            truncate_name(
                namespace,
                MAX_CALLABLE_PART_LENGTH - suffix.len() - MCP_TOOL_NAME_DELIMITER.len(),
            ),
            suffix,
            MCP_TOOL_NAME_DELIMITER
        )
    } else {
        format!(
            "{}{suffix}",
            truncate_name(namespace, MAX_CALLABLE_PART_LENGTH - suffix.len())
        )
    }
}

fn truncate_name(value: &str, max_len: usize) -> String {
    value.chars().take(max_len).collect()
}

fn fit_callable_parts_with_hash(
    namespace: &str,
    tool_name: &str,
    raw_identity: &str,
    reserved_len: usize,
) -> (String, String) {
    let suffix = callable_name_hash_suffix(raw_identity);
    let max_tool_len = MAX_TOOL_NAME_LENGTH
        .saturating_sub(namespace.len() + reserved_len)
        .min(MAX_CALLABLE_PART_LENGTH);
    if max_tool_len >= suffix.len() {
        let prefix_len = max_tool_len - suffix.len();
        return (
            namespace.to_string(),
            format!("{}{}", truncate_name(tool_name, prefix_len), suffix),
        );
    }

    let max_namespace_len = MAX_TOOL_NAME_LENGTH.saturating_sub(suffix.len() + reserved_len);
    (truncate_name(namespace, max_namespace_len), suffix)
}

fn unique_callable_parts(
    namespace: &str,
    tool_name: &str,
    raw_identity: &str,
    used_names: &mut HashSet<String>,
    reserved_len: usize,
) -> (String, String) {
    let model_name = format!("{namespace}{tool_name}");
    if model_name.len() + reserved_len <= MAX_TOOL_NAME_LENGTH
        && tool_name.len() <= MAX_CALLABLE_PART_LENGTH
        && used_names.insert(model_name)
    {
        return (namespace.to_string(), tool_name.to_string());
    }

    let mut attempt = 0_u32;
    loop {
        let hash_input = if attempt == 0 {
            raw_identity.to_string()
        } else {
            format!("{raw_identity}\0{attempt}")
        };
        let (namespace, tool_name) =
            fit_callable_parts_with_hash(namespace, tool_name, &hash_input, reserved_len);
        let model_name = format!("{namespace}{tool_name}");
        if used_names.insert(model_name) {
            return (namespace, tool_name);
        }
        attempt = attempt.saturating_add(1);
    }
}

// Python `MCPUtil.get_all_function_tools` and its prefixed-name allocation (src/agents/mcp/util.py).
const MCP_FUNCTION_TOOL_NAME_MAX_LENGTH: usize = 64;
const MCP_FUNCTION_TOOL_HASH_LENGTH: usize = 8;

/// Assigns the Python SDK's bare function names, for tools listed in server order.
///
/// Without `include_server_in_tool_names`, each tool keeps its original MCP name and a name that
/// another server already listed is a configuration error. With it, names become
/// `mcp_{server}__{tool}`, shortened to 64 bytes and hashed on collisions with each other or with
/// `reserved_tool_names`. The original server and tool names are kept for routing.
///
/// # Errors
///
/// Returns a configuration error for duplicate names across servers.
pub fn name_tools_for_function_calling(
    mut tools: Vec<McpToolInfo>,
    include_server_in_tool_names: bool,
    reserved_tool_names: &BTreeSet<String>,
) -> Result<Vec<McpToolInfo>> {
    if include_server_in_tool_names {
        let names = prefixed_tool_names(&tools_snapshot(&tools), reserved_tool_names);
        for (tool, name) in tools.iter_mut().zip(names) {
            tool.callable_name = name;
        }
    }
    for tool in &mut tools {
        tool.namespaced = false;
    }
    let mut tool_names = HashSet::new();
    let mut index = 0;
    while index < tools.len() {
        let server_index = tools[index].server_index;
        let end = tools[index..]
            .iter()
            .position(|tool| tool.server_index != server_index)
            .map_or(tools.len(), |offset| index + offset);
        let server_tool_names: BTreeSet<_> = tools[index..end]
            .iter()
            .map(|tool| tool.callable_name.clone())
            .collect();
        let duplicates: Vec<_> = server_tool_names
            .iter()
            .filter(|name| tool_names.contains(*name))
            .map(|name| python_repr(name))
            .collect();
        if !duplicates.is_empty() {
            let mut message = format!(
                "Duplicate tool names found across MCP servers: {}",
                duplicates.join(", ")
            );
            if !include_server_in_tool_names {
                message.push_str(
                    ". Select `McpToolNaming::ServerPrefixed` when discovering tools to prefix \
                     tool names with their server name and avoid collisions.",
                );
            }
            return Err(Error::config(message));
        }
        tool_names.extend(server_tool_names);
        index = end;
    }
    Ok(tools)
}

fn tools_snapshot(tools: &[McpToolInfo]) -> Vec<(usize, String, String)> {
    tools
        .iter()
        .map(|tool| {
            (
                tool.server_index,
                safe_server_name(&tool.server_name),
                tool.tool.name.to_string(),
            )
        })
        .collect()
}

fn safe_tool_name_part(value: &str, fallback: &str) -> String {
    let safe: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches(|c| c == '_' || c == '-');
    if safe.is_empty() {
        fallback.to_owned()
    } else {
        safe.to_owned()
    }
}

fn shorten_tool_name(base_name: &str, seed: &str, force_hash: bool) -> String {
    if !force_hash && base_name.len() <= MCP_FUNCTION_TOOL_NAME_MAX_LENGTH {
        return base_name.to_owned();
    }
    let suffix = format!("_{}", &sha1_hex(seed)[..MCP_FUNCTION_TOOL_HASH_LENGTH]);
    let stem_length = MCP_FUNCTION_TOOL_NAME_MAX_LENGTH - suffix.len();
    // Base names are ASCII after `safe_tool_name_part`, so byte slicing is a character slice.
    let stem = base_name[..base_name.len().min(stem_length)].trim_end_matches(['_', '-']);
    let stem = if stem.is_empty() { "mcp" } else { stem };
    format!("{stem}{suffix}")
}

fn prefixed_tool_base_name(server_name: &str, tool_name: &str) -> String {
    format!(
        "mcp_{}__{}",
        safe_tool_name_part(server_name, "server"),
        safe_tool_name_part(tool_name, "tool")
    )
}

// Entries are (server index, credential-free server name, original tool name), in listing order.
fn prefixed_tool_names(
    tools: &[(usize, String, String)],
    reserved_names: &BTreeSet<String>,
) -> Vec<String> {
    let base_names: Vec<_> = tools
        .iter()
        .map(|(_, server, tool)| prefixed_tool_base_name(server, tool))
        .collect();
    let mut base_name_counts = HashMap::<&str, usize>::new();
    for name in &base_names {
        *base_name_counts.entry(name).or_default() += 1;
    }
    let mut candidates: Vec<_> = tools
        .iter()
        .zip(&base_names)
        .enumerate()
        .map(|(position, ((server_index, server, tool), base_name))| {
            let seed = format!("{server}\0{tool}");
            let force_hash =
                base_name_counts[base_name.as_str()] > 1 || reserved_names.contains(base_name);
            let initial_name = shorten_tool_name(base_name, &seed, force_hash);
            (initial_name, seed, *server_index, position, base_name)
        })
        .collect();
    candidates.sort();
    let mut used_names: HashSet<String> = reserved_names.iter().cloned().collect();
    let mut names = vec![String::new(); tools.len()];
    for (initial_name, seed, _, position, base_name) in candidates {
        let mut public_name = initial_name;
        let mut collision_index = 1_u32;
        while used_names.contains(&public_name) {
            public_name = shorten_tool_name(base_name, &format!("{seed}\0{collision_index}"), true);
            collision_index = collision_index.saturating_add(1);
        }
        used_names.insert(public_name.clone());
        names[position] = public_name;
    }
    names
}

// Python's `repr` of a str, which its duplicate-name diagnostics use so control and bidi
// characters cannot forge the message.
fn python_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut rendered = String::with_capacity(value.len() + 2);
    rendered.push(quote);
    for c in value.chars() {
        match c {
            '\\' => rendered.push_str("\\\\"),
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            c if c == quote => {
                rendered.push('\\');
                rendered.push(c);
            }
            c if is_python_printable(c) => rendered.push(c),
            c if u32::from(c) <= 0xff => {
                let _ = write!(rendered, "\\x{:02x}", u32::from(c));
            }
            c if u32::from(c) <= 0xffff => {
                let _ = write!(rendered, "\\u{:04x}", u32::from(c));
            }
            c => {
                let _ = write!(rendered, "\\U{:08x}", u32::from(c));
            }
        }
    }
    rendered.push(quote);
    rendered
}

// Approximates `str.isprintable`: control, format, separator (other than space), surrogate and
// private-use characters are escaped.
fn is_python_printable(c: char) -> bool {
    let code = u32::from(c);
    !(c.is_control()
        || matches!(
            code,
            0x00a0
                | 0x00ad
                | 0x0600..=0x0605
                | 0x061c
                | 0x06dd
                | 0x070f
                | 0x1680
                | 0x180e
                | 0x2000..=0x200f
                | 0x2028..=0x202f
                | 0x205f..=0x2064
                | 0x2066..=0x206f
                | 0x3000
                | 0xe000..=0xf8ff
                | 0xfeff
                | 0xfff9..=0xfffb
                | 0x000f_0000..=0x0010_ffff
        ))
}
