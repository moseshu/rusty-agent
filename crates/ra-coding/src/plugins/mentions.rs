// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Retain source structured and linked plugin mention operations."
)]
use std::collections::HashSet;

use crate::skills::ToolMentionKind;
use crate::skills::app_id_from_path;
use crate::skills::extract_tool_mentions_with_sigil;
use crate::skills::plugin_config_name_from_path;
use crate::skills::tool_kind_for_path;
use ra_protocol::server::protocol::UserInput;

const PLUGIN_TEXT_MENTION_SIGIL: char = '@';
const TOOL_MENTION_SIGIL: char = '$';

use super::PluginCapabilitySummary;

pub struct CollectedToolMentions {
    pub plain_names: HashSet<String>,
    pub paths: HashSet<String>,
}

#[must_use]
pub fn collect_tool_mentions_from_messages(messages: &[String]) -> CollectedToolMentions {
    collect_tool_mentions_from_messages_with_sigil(messages, TOOL_MENTION_SIGIL)
}

fn collect_tool_mentions_from_messages_with_sigil(
    messages: &[String],
    sigil: char,
) -> CollectedToolMentions {
    let mut plain_names = HashSet::new();
    let mut paths = HashSet::new();
    for message in messages {
        let mentions = extract_tool_mentions_with_sigil(message, sigil);
        plain_names.extend(mentions.plain_names().map(str::to_string));
        paths.extend(mentions.paths().map(str::to_string));
    }
    CollectedToolMentions { plain_names, paths }
}

#[must_use]
pub fn collect_explicit_app_ids(input: &[UserInput]) -> HashSet<String> {
    let messages = input
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<String>>();

    input
        .iter()
        .filter_map(|item| match item {
            UserInput::Mention { path, .. } => Some(path.clone()),
            _ => None,
        })
        .chain(collect_tool_mentions_from_messages(&messages).paths)
        .filter(|path| tool_kind_for_path(path.as_str()) == ToolMentionKind::App)
        .filter_map(|path| app_id_from_path(path.as_str()).map(str::to_string))
        .collect()
}

/// Collect explicit structured or linked `plugin://...` mentions.
#[must_use]
pub fn collect_explicit_plugin_mentions(
    input: &[UserInput],
    plugins: &[PluginCapabilitySummary],
) -> Vec<PluginCapabilitySummary> {
    if plugins.is_empty() {
        return Vec::new();
    }

    // `config_name` stores the full plugin ID, not its display or mention name.
    let mentioned_plugin_ids = collect_explicit_plugin_ids(input);
    plugins
        .iter()
        .filter(|plugin| mentioned_plugin_ids.contains(plugin.config_name.as_str()))
        .cloned()
        .collect()
}

/// Collect exact IDs from explicit `plugin://` references, independently of display names.
///
/// Host plugins use `<plugin>@<marketplace>` IDs; selected roots may supply opaque IDs.
#[must_use]
pub fn collect_explicit_plugin_ids(input: &[UserInput]) -> HashSet<String> {
    let messages = input
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<String>>();

    input
        .iter()
        .filter_map(|item| match item {
            UserInput::Mention { path, .. } => Some(path.clone()),
            _ => None,
        })
        .chain(
            // Plugin plaintext links use `@`, not the default `$` tool sigil.
            collect_tool_mentions_from_messages_with_sigil(&messages, PLUGIN_TEXT_MENTION_SIGIL)
                .paths,
        )
        .filter(|path| tool_kind_for_path(path.as_str()) == ToolMentionKind::Plugin)
        .filter_map(|path| plugin_config_name_from_path(path.as_str()).map(str::to_string))
        .collect()
}
