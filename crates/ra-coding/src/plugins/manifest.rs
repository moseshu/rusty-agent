// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Host plugin manifest parsing. Resources remain lexically resolved absolute paths.
//!
//! The generic models preserve resource mapping, while executor URI parsing belongs to the
//! environment-owned package adapter. This module neither installs nor activates a package.

use super::AbsolutePathBuf;
use super::discovery::{AGENT_PLUGIN_MANIFEST_RELATIVE_PATH, find_plugin_manifest_path};
use super::hook_config::HooksFile;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::{fs, io, path::Path};

mod agent;
mod cache;
use agent::parse_agent_plugin_manifest;
pub(crate) use cache::ManifestCache;

const MAX_DEFAULT_PROMPT_COUNT: usize = 3;
const MAX_DEFAULT_PROMPT_LEN: usize = 128;

/// A manifest with resources on the local host.
pub type PluginManifest = super::model::PluginManifest<AbsolutePathBuf>;
/// Host hook configuration declarations.
pub type PluginManifestHooks = super::model::PluginManifestHooks<AbsolutePathBuf>;
/// Host presentation metadata.
pub type PluginManifestInterface = super::model::PluginManifestInterface<AbsolutePathBuf>;
/// Host MCP configuration declarations.
pub type PluginManifestMcpServers = super::model::PluginManifestMcpServers<AbsolutePathBuf>;
/// Host component paths.
pub type PluginManifestPaths = super::model::PluginManifestPaths<AbsolutePathBuf>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PluginManifestFormat {
    Legacy,
    AgentPlugin,
}

pub(crate) struct LoadedPluginManifest {
    pub(crate) manifest: PluginManifest,
    pub(crate) format: PluginManifestFormat,
}

/// Loads a host manifest, returning no package for an absent or invalid selected manifest.
#[must_use]
pub fn load_plugin_manifest(plugin_root: &Path) -> Option<PluginManifest> {
    load_plugin_manifest_with_format(plugin_root).map(|loaded| loaded.manifest)
}

/// Whether the selected, successfully parsed package uses the portable format.
#[must_use]
pub fn is_agent_plugin_manifest(plugin_root: &Path) -> bool {
    load_plugin_manifest_with_format(plugin_root)
        .is_some_and(|loaded| loaded.format == PluginManifestFormat::AgentPlugin)
}

pub(crate) fn load_plugin_manifest_with_format(plugin_root: &Path) -> Option<LoadedPluginManifest> {
    ManifestCache::disabled().load(plugin_root)
}

pub(crate) fn load_plugin_command_paths(
    plugin_root: &Path,
) -> io::Result<Option<Vec<std::path::PathBuf>>> {
    let Some(manifest_path) = find_plugin_manifest_path(plugin_root) else {
        return Ok(None);
    };
    let manifest =
        serde_json::from_str::<RawPluginCommandManifest>(&fs::read_to_string(manifest_path)?)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let Some(commands) = manifest.commands else {
        return Ok(None);
    };
    let plugin_root = host_absolute_path(plugin_root)?;
    Ok(Some(
        resolve_manifest_paths(&plugin_root, "commands", Some(&commands))
            .into_iter()
            .map(AbsolutePathBuf::into_path_buf)
            .collect(),
    ))
}

pub(crate) fn parse_plugin_manifest(
    plugin_root: &Path,
    manifest_path: &Path,
    contents: &str,
) -> Result<PluginManifest, serde_json::Error> {
    parse_resolved_plugin_manifest(
        &ManifestCache::disabled(),
        plugin_root,
        manifest_path,
        contents,
        None,
    )
}

fn parse_resolved_plugin_manifest(
    cache: &ManifestCache,
    plugin_root: &Path,
    manifest_path: &Path,
    contents: &str,
    overlay: Option<(&Path, &str)>,
) -> Result<PluginManifest, serde_json::Error> {
    let root = host_absolute_path(plugin_root).map_err(serde_json::Error::io)?;
    let path = host_absolute_path(manifest_path).map_err(serde_json::Error::io)?;
    let overlay = overlay
        .map(|(path, contents)| host_absolute_path(path).map(|path| (path, contents)))
        .transpose()
        .map_err(serde_json::Error::io)?;
    let overlay = overlay.as_ref().map(|(path, contents)| (path, *contents));
    cache.parse(&root, &path, contents, overlay, || {
        parse_manifest_contents(&root, &path, contents, overlay)
    })
}

fn parse_manifest_contents(
    root: &AbsolutePathBuf,
    path: &AbsolutePathBuf,
    contents: &str,
    overlay: Option<(&AbsolutePathBuf, &str)>,
) -> Result<PluginManifest, serde_json::Error> {
    if host_path_url(root).is_none() {
        return Err(compatibility_json_error("invalid file URI path"));
    }
    if path.as_path() == root.as_path().join(AGENT_PLUGIN_MANIFEST_RELATIVE_PATH) {
        parse_agent_plugin_manifest(root, path, contents, overlay)
    } else {
        parse_legacy_plugin_manifest(root, path, contents)
    }
}

#[derive(Deserialize)]
struct RawPluginCommandManifest {
    #[serde(default)]
    commands: Option<RawPluginManifestPaths>,
}

pub(crate) fn host_absolute_path(path: &Path) -> io::Result<AbsolutePathBuf> {
    let expanded = crate::permissions::path::expand_home_directory(path);
    if !expanded.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path is not absolute: {}", path.display()),
        ));
    }
    AbsolutePathBuf::from_absolute_path(expanded)
}

// Preserve the host subset of PathUri::from_abs_path: a URI that implies a different
// convention, contains a null, or cannot represent the native path is opaque upstream.
pub(crate) fn host_path_url(path: &AbsolutePathBuf) -> Option<url::Url> {
    let mut url = url::Url::from_file_path(path.as_path()).ok()?;
    if url.path().contains("%00") {
        return None;
    }
    if url.host_str() == Some("localhost") {
        url.set_host(None).ok()?;
    }
    let has_windows_drive = url
        .path_segments()?
        .find(|segment| !segment.is_empty())
        .is_some_and(
            |segment| matches!(segment.as_bytes(), [drive, b':'] | [drive, b'%', b'3', b'A' | b'a'] if drive.is_ascii_alphabetic()),
        );
    let is_windows = url.host_str().is_some() || has_windows_drive;
    if has_windows_drive && url.host_str().is_none() {
        let path = url.path();
        let drive_start = path.bytes().position(|byte| byte != b'/')?;
        let drive_letter = char::from(path.as_bytes()[drive_start]).to_ascii_uppercase();
        let normalized = format!(
            "{}{drive_letter}{}",
            &path[..drive_start],
            &path[drive_start + 1..]
        );
        url.set_path(&normalized);
    }
    (is_windows == cfg!(windows) && url.host_str() != Some("")).then_some(url)
}

pub(crate) fn host_basename(path: &AbsolutePathBuf) -> Option<String> {
    let url = host_path_url(path)?;
    let segment = url.path_segments()?.rfind(|segment| !segment.is_empty())?;
    // As PathUri::basename does, retain the encoded spelling for non-UTF-8 names.
    Some(
        percent_encoding::percent_decode_str(segment)
            .decode_utf8()
            .map_or_else(|_| segment.to_owned(), std::borrow::Cow::into_owned),
    )
}

fn parse_legacy_plugin_manifest(
    root: &AbsolutePathBuf,
    path: &AbsolutePathBuf,
    contents: &str,
) -> Result<PluginManifest, serde_json::Error> {
    resolve_raw_plugin_manifest(
        root,
        path,
        serde_json::from_str::<RawPluginManifest>(contents)?,
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPluginManifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    // Keep manifest paths as raw strings so we can validate the required `./...` syntax before
    // resolving them under the plugin root.
    #[serde(default)]
    skills: Option<RawPluginManifestPaths>,
    #[serde(default)]
    mcp_servers: Option<RawPluginManifestMcpServers>,
    #[serde(default)]
    apps: Option<String>,
    #[serde(default)]
    hooks: Option<RawPluginManifestHooks>,
    #[serde(default)]
    interface: Option<RawPluginManifestInterface>,
    #[serde(default)]
    extensions: JsonValue,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPluginManifestInterface {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    short_description: Option<String>,
    #[serde(default)]
    long_description: Option<String>,
    #[serde(default)]
    developer_name: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    #[serde(alias = "websiteURL")]
    website_url: Option<String>,
    #[serde(default)]
    #[serde(alias = "privacyPolicyURL")]
    privacy_policy_url: Option<String>,
    #[serde(default)]
    #[serde(alias = "termsOfServiceURL")]
    terms_of_service_url: Option<String>,
    #[serde(default)]
    default_prompt: Option<RawPluginManifestDefaultPrompt>,
    #[serde(default)]
    brand_color: Option<String>,
    #[serde(default)]
    composer_icon: Option<String>,
    #[serde(default)]
    logo: Option<String>,
    #[serde(default)]
    logo_dark: Option<String>,
    #[serde(default)]
    screenshots: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPluginManifestDefaultPrompt {
    String(String),
    List(Vec<RawPluginManifestDefaultPromptEntry>),
    Invalid(JsonValue),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPluginManifestDefaultPromptEntry {
    String(String),
    Invalid(JsonValue),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPluginManifestPaths {
    Path(String),
    Paths(Vec<String>),
    Invalid(JsonValue),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPluginManifestMcpServers {
    Path(String),
    Object(std::collections::BTreeMap<String, JsonValue>),
    Invalid(JsonValue),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPluginManifestHooks {
    Path(String),
    Paths(Vec<String>),
    Inline(Box<HooksFile>),
    InlineList(Vec<HooksFile>),
    Invalid(JsonValue),
}

// Keep the upstream parser's fallible signature and field resolution order.
#[allow(clippy::too_many_lines, clippy::unnecessary_wraps)]
fn resolve_raw_plugin_manifest(
    plugin_root: &AbsolutePathBuf,
    manifest_path: &AbsolutePathBuf,
    raw: RawPluginManifest,
) -> Result<PluginManifest, serde_json::Error> {
    let RawPluginManifest {
        name: raw_name,
        version,
        description,
        keywords,
        skills,
        mcp_servers,
        apps,
        hooks,
        interface,
        extensions,
    } = raw;
    let name = host_basename(plugin_root)
        .filter(|_| raw_name.trim().is_empty())
        .unwrap_or(raw_name);
    let manifest_path_for_warning = manifest_path.to_string();
    let version = version.and_then(|version| {
        let version = version.trim();
        (!version.is_empty()).then(|| version.to_string())
    });
    let interface = interface.and_then(|interface| {
        let RawPluginManifestInterface {
            display_name,
            short_description,
            long_description,
            developer_name,
            category,
            capabilities,
            website_url,
            privacy_policy_url,
            terms_of_service_url,
            default_prompt,
            brand_color,
            composer_icon,
            logo,
            logo_dark,
            screenshots,
        } = interface;

        let interface = super::model::PluginManifestInterface {
            display_name,
            short_description,
            long_description,
            developer_name,
            category,
            capabilities,
            website_url,
            privacy_policy_url,
            terms_of_service_url,
            default_prompt: resolve_default_prompts(
                &manifest_path_for_warning,
                default_prompt.as_ref(),
            ),
            brand_color,
            composer_icon: resolve_interface_asset_path(
                plugin_root,
                "interface.composerIcon",
                composer_icon.as_deref(),
            ),
            logo: resolve_interface_asset_path(plugin_root, "interface.logo", logo.as_deref()),
            logo_dark: resolve_interface_asset_path(
                plugin_root,
                "interface.logoDark",
                logo_dark.as_deref(),
            ),
            screenshots: screenshots
                .iter()
                .filter_map(|screenshot| {
                    resolve_interface_asset_path(
                        plugin_root,
                        "interface.screenshots",
                        Some(screenshot),
                    )
                })
                .collect(),
        };

        let has_fields = interface.display_name.is_some()
            || interface.short_description.is_some()
            || interface.long_description.is_some()
            || interface.developer_name.is_some()
            || interface.category.is_some()
            || !interface.capabilities.is_empty()
            || interface.website_url.is_some()
            || interface.privacy_policy_url.is_some()
            || interface.terms_of_service_url.is_some()
            || interface.default_prompt.is_some()
            || interface.brand_color.is_some()
            || interface.composer_icon.is_some()
            || interface.logo.is_some()
            || interface.logo_dark.is_some()
            || !interface.screenshots.is_empty();

        has_fields.then_some(interface)
    });
    Ok(super::model::PluginManifest {
        name,
        version,
        description,
        keywords,
        paths: super::model::PluginManifestPaths {
            skills: resolve_manifest_paths(plugin_root, "skills", skills.as_ref()),
            onboarding_skill: resolve_openai_onboarding_skill(
                plugin_root,
                extensions.get("com.openai"),
            ),
            mcp_servers: resolve_manifest_mcp_servers(plugin_root, mcp_servers),
            apps: resolve_manifest_path(plugin_root, "apps", apps.as_deref()),
            hooks: resolve_manifest_hooks(plugin_root, hooks),
        },
        interface,
    })
}

fn compatibility_json_error(message: impl Into<String>) -> serde_json::Error {
    serde_json::Error::io(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

fn resolve_manifest_hooks(
    plugin_root: &AbsolutePathBuf,
    hooks: Option<RawPluginManifestHooks>,
) -> Option<super::model::PluginManifestHooks<AbsolutePathBuf>> {
    match hooks? {
        RawPluginManifestHooks::Path(path) => {
            resolve_manifest_path(plugin_root, "hooks", Some(&path))
                .map(|path| super::model::PluginManifestHooks::Paths(vec![path]))
        }
        RawPluginManifestHooks::Paths(paths) => {
            let hooks = paths
                .iter()
                .filter_map(|path| resolve_manifest_path(plugin_root, "hooks", Some(path)))
                .collect::<Vec<_>>();
            (!hooks.is_empty()).then_some(super::model::PluginManifestHooks::Paths(hooks))
        }
        RawPluginManifestHooks::Inline(hooks) => {
            Some(super::model::PluginManifestHooks::Inline(vec![*hooks]))
        }
        RawPluginManifestHooks::InlineList(hooks) => {
            (!hooks.is_empty()).then_some(super::model::PluginManifestHooks::Inline(hooks))
        }
        RawPluginManifestHooks::Invalid(value) => {
            tracing::warn!(
                "ignoring hooks: expected a string, string array, object, or object array; found {}",
                json_value_type(&value)
            );
            None
        }
    }
}

fn resolve_manifest_mcp_servers(
    plugin_root: &AbsolutePathBuf,
    mcp_servers: Option<RawPluginManifestMcpServers>,
) -> Option<super::model::PluginManifestMcpServers<AbsolutePathBuf>> {
    match mcp_servers? {
        RawPluginManifestMcpServers::Path(path) => {
            resolve_manifest_path(plugin_root, "mcpServers", Some(&path))
                .map(super::model::PluginManifestMcpServers::Path)
        }
        RawPluginManifestMcpServers::Object(servers) => match serde_json::to_string(&servers) {
            Ok(servers) => Some(super::model::PluginManifestMcpServers::Object(servers)),
            Err(err) => {
                tracing::warn!("ignoring mcpServers: failed to serialize object: {err}");
                None
            }
        },
        RawPluginManifestMcpServers::Invalid(value) => {
            tracing::warn!(
                "ignoring mcpServers: expected a string or object; found {}",
                json_value_type(&value)
            );
            None
        }
    }
}

fn resolve_interface_asset_path(
    plugin_root: &AbsolutePathBuf,
    field: &'static str,
    path: Option<&str>,
) -> Option<AbsolutePathBuf> {
    resolve_manifest_path(plugin_root, field, path)
}

fn resolve_default_prompts(
    manifest_path: &str,
    value: Option<&RawPluginManifestDefaultPrompt>,
) -> Option<Vec<String>> {
    match value? {
        RawPluginManifestDefaultPrompt::String(prompt) => {
            resolve_default_prompt_str(manifest_path, "interface.defaultPrompt", prompt)
                .map(|prompt| vec![prompt])
        }
        RawPluginManifestDefaultPrompt::List(values) => {
            let mut prompts = Vec::new();
            for (index, item) in values.iter().enumerate() {
                if prompts.len() >= MAX_DEFAULT_PROMPT_COUNT {
                    warn_invalid_default_prompt(
                        manifest_path,
                        "interface.defaultPrompt",
                        &format!("maximum of {MAX_DEFAULT_PROMPT_COUNT} prompts is supported"),
                    );
                    break;
                }

                match item {
                    RawPluginManifestDefaultPromptEntry::String(prompt) => {
                        let field = format!("interface.defaultPrompt[{index}]");
                        if let Some(prompt) =
                            resolve_default_prompt_str(manifest_path, &field, prompt)
                        {
                            prompts.push(prompt);
                        }
                    }
                    RawPluginManifestDefaultPromptEntry::Invalid(value) => {
                        let field = format!("interface.defaultPrompt[{index}]");
                        warn_invalid_default_prompt(
                            manifest_path,
                            &field,
                            &format!("expected a string, found {}", json_value_type(value)),
                        );
                    }
                }
            }

            (!prompts.is_empty()).then_some(prompts)
        }
        RawPluginManifestDefaultPrompt::Invalid(value) => {
            warn_invalid_default_prompt(
                manifest_path,
                "interface.defaultPrompt",
                &format!(
                    "expected a string or array of strings, found {}",
                    json_value_type(value)
                ),
            );
            None
        }
    }
}

fn resolve_default_prompt_str(manifest_path: &str, field: &str, prompt: &str) -> Option<String> {
    let prompt = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if prompt.is_empty() {
        warn_invalid_default_prompt(manifest_path, field, "prompt must not be empty");
        return None;
    }
    if prompt.chars().count() > MAX_DEFAULT_PROMPT_LEN {
        warn_invalid_default_prompt(
            manifest_path,
            field,
            &format!("prompt must be at most {MAX_DEFAULT_PROMPT_LEN} characters"),
        );
        return None;
    }
    Some(prompt)
}

fn warn_invalid_default_prompt(manifest_path: &str, field: &str, message: &str) {
    tracing::warn!(path = %manifest_path, "ignoring {field}: {message}");
}

fn resolve_openai_onboarding_skill(
    plugin_root: &AbsolutePathBuf,
    extension: Option<&JsonValue>,
) -> Option<AbsolutePathBuf> {
    let path = extension
        .and_then(|extension| extension.get("onboardingSkill"))
        .and_then(JsonValue::as_str)?;
    // This extension accepts relative paths with or without the legacy `./` prefix.
    let path = format!("./{}", path.strip_prefix("./").unwrap_or(path));
    resolve_manifest_path(
        plugin_root,
        "extensions[\"com.openai\"].onboardingSkill",
        Some(&path),
    )
}

fn json_value_type(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

fn resolve_manifest_paths(
    plugin_root: &AbsolutePathBuf,
    field: &'static str,
    paths: Option<&RawPluginManifestPaths>,
) -> Vec<AbsolutePathBuf> {
    match paths {
        Some(RawPluginManifestPaths::Path(path)) => {
            resolve_manifest_path(plugin_root, field, Some(path))
                .map(|path| vec![path])
                .unwrap_or_default()
        }
        Some(RawPluginManifestPaths::Paths(paths)) => paths
            .iter()
            .filter_map(|path| resolve_manifest_path(plugin_root, field, Some(path)))
            .collect(),
        Some(RawPluginManifestPaths::Invalid(value)) => {
            tracing::warn!(
                "ignoring {field}: expected a string or string array; found {}",
                json_value_type(value)
            );
            Vec::new()
        }
        None => Vec::new(),
    }
}

fn resolve_manifest_path(
    plugin_root: &AbsolutePathBuf,
    field: &'static str,
    path: Option<&str>,
) -> Option<AbsolutePathBuf> {
    let path = path?;
    if path.is_empty() {
        return None;
    }
    let Some(relative_path) = path.strip_prefix("./") else {
        tracing::warn!("ignoring {field}: path must start with `./` relative to plugin root");
        return None;
    };
    if relative_path.is_empty() {
        tracing::warn!("ignoring {field}: path must not be `./`");
        return None;
    }
    let has_parent_component = if cfg!(windows) {
        relative_path
            .split(['/', '\\'])
            .any(|component| component == "..")
    } else {
        relative_path.split('/').any(|component| component == "..")
    };
    if has_parent_component {
        tracing::warn!("ignoring {field}: path must not contain '..'");
        return None;
    }
    let has_windows_root = cfg!(windows)
        && (relative_path.starts_with('\\')
            || matches!(relative_path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic()));
    if relative_path.starts_with('/') || has_windows_root {
        tracing::warn!("ignoring {field}: path must stay within the plugin root");
        return None;
    }
    if relative_path.contains('\0') {
        tracing::warn!(
            "ignoring {field}: path must resolve under plugin root: invalid file URI path"
        );
        return None;
    }
    if host_path_url(plugin_root).is_none() {
        tracing::warn!(
            "ignoring {field}: path must resolve under plugin root: invalid file URI path"
        );
        return None;
    }
    let resolved =
        match AbsolutePathBuf::from_absolute_path(plugin_root.as_path().join(relative_path)) {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!("ignoring {field}: path must resolve under plugin root: {err}");
                return None;
            }
        };
    if host_path_url(&resolved).is_none() || !resolved.as_path().starts_with(plugin_root.as_path())
    {
        tracing::warn!("ignoring {field}: path must stay within the plugin root");
        return None;
    }
    Some(resolved)
}

/// Access to synchronous path resolution for the external integration-test workspace.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use super::AbsolutePathBuf;
    use std::path::Path;

    /// Resolves a resource using the same checked root and path validation as Host parsing.
    #[must_use]
    pub fn resolve_resource(plugin_root: &Path, path: &str) -> Option<AbsolutePathBuf> {
        let plugin_root = super::host_absolute_path(plugin_root).ok()?;
        super::resolve_manifest_path(&plugin_root, "skills", Some(path))
    }

    /// An owner-scoped cache bridge; the release API remains crate-internal.
    #[derive(Debug, Default)]
    pub struct Cache(super::ManifestCache);

    impl Cache {
        /// Creates an uncached parser, as used by standalone Host manifest loading.
        #[must_use]
        pub fn disabled() -> Self {
            Self(super::ManifestCache::disabled())
        }

        /// Reads and validates through this cache owner.
        #[must_use]
        pub fn load(&self, root: &Path) -> Option<super::PluginManifest> {
            self.0.load(root).map(|loaded| loaded.manifest)
        }

        /// Parses a revision with an observer immediately before the real parser runs.
        pub fn parse_with(
            &self,
            root: &Path,
            path: &Path,
            contents: &str,
            overlay: Option<(&Path, &str)>,
            before_parse: impl FnOnce(),
        ) -> Result<super::PluginManifest, serde_json::Error> {
            let root = super::host_absolute_path(root).map_err(serde_json::Error::io)?;
            let path = super::host_absolute_path(path).map_err(serde_json::Error::io)?;
            let overlay = overlay
                .map(|(path, contents)| {
                    super::host_absolute_path(path).map(|path| (path, contents))
                })
                .transpose()
                .map_err(serde_json::Error::io)?;
            let overlay = overlay.as_ref().map(|(path, contents)| (path, *contents));
            self.0.parse(&root, &path, contents, overlay, || {
                before_parse();
                super::parse_manifest_contents(&root, &path, contents, overlay)
            })
        }
    }
}
