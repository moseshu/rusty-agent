// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Core Host selection preserves discovery order, path identity and ambiguity handling.

use super::{
    SkillInput,
    catalog::{SkillCatalog, SkillCatalogEntry},
    mentions::extract_tool_mentions,
};
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

/// Resolves core Host mentions, preserving catalog order and rejecting ambiguous plain names.
/// `connector_slug_counts` contains lowercase connector names; conflicting plain names require
/// explicit paths. Structured skill paths precede text, and discovery aliases match canonical paths.
/// Text links require absolute native paths after home expansion. Only structured skill inputs
/// may resolve a relative path against the process cwd, as in Codex.
#[must_use]
pub fn collect_host_skill_mentions<S: std::hash::BuildHasher>(
    inputs: &[SkillInput],
    catalog: &SkillCatalog,
    connector_slug_counts: &HashMap<String, usize, S>,
) -> Vec<SkillCatalogEntry> {
    let mut counts = HashMap::<&str, usize>::new();
    for entry in catalog.entries.iter().filter(|e| e.enabled) {
        *counts.entry(&entry.name).or_default() += 1;
    }
    let mut selected = Vec::new();
    let mut seen_names = HashSet::new();
    let mut seen_paths = HashSet::new();
    let mut blocked_plain_names = HashSet::new();
    for input in inputs {
        if let SkillInput::Skill { name, path } = input {
            blocked_plain_names.insert(name.as_str());
            let Some(path) = structured_path(path) else {
                continue;
            };
            let Some(entry) = catalog
                .entries
                .iter()
                .find(|entry| matches_path(entry, &path))
            else {
                continue;
            };
            if !entry.enabled || !seen_paths.insert(entry.main_prompt.as_str()) {
                continue;
            }
            seen_names.insert(entry.name.as_str());
            selected.push(entry.clone());
        }
    }
    for input in inputs {
        let SkillInput::Text { text } = input else {
            continue;
        };
        let mentions = extract_tool_mentions(text);
        let paths: HashSet<_> = mentions
            .paths()
            .filter(|path| {
                !["app://", "mcp://", "plugin://"]
                    .iter()
                    .any(|prefix| path.starts_with(prefix))
            })
            .filter_map(|path| {
                native_path(Path::new(path.strip_prefix("skill://").unwrap_or(path)))
            })
            .collect();
        for entry in &catalog.entries {
            if !entry.enabled || seen_paths.contains(entry.main_prompt.as_str()) {
                continue;
            }
            if paths.iter().any(|path| matches_path(entry, path)) {
                seen_paths.insert(entry.main_prompt.as_str());
                seen_names.insert(entry.name.as_str());
                selected.push(entry.clone());
            }
        }
        for entry in &catalog.entries {
            if !entry.enabled
                || seen_paths.contains(entry.main_prompt.as_str())
                || blocked_plain_names.contains(entry.name.as_str())
            {
                continue;
            }
            if !mentions.plain_names().any(|name| name == entry.name) {
                continue;
            }
            if counts.get(entry.name.as_str()).copied().unwrap_or(0) != 1
                || connector_slug_counts
                    .get(&entry.name.to_ascii_lowercase())
                    .copied()
                    .unwrap_or(0)
                    != 0
            {
                continue;
            }
            if seen_names.insert(entry.name.as_str()) {
                seen_paths.insert(entry.main_prompt.as_str());
                selected.push(entry.clone());
            }
        }
    }
    selected
}
fn matches_path(entry: &SkillCatalogEntry, path: &Path) -> bool {
    native_path(Path::new(entry.main_prompt.as_str())).as_deref() == Some(path)
        || entry
            .display_path
            .as_deref()
            .and_then(|display| native_path(Path::new(display)))
            .as_deref()
            == Some(path)
}
pub(super) fn native_path(path: &Path) -> Option<PathBuf> {
    let path = expand_home_directory(path);
    path.is_absolute().then(|| normalize_absolute_path(&path))
}

fn structured_path(path: &Path) -> Option<PathBuf> {
    let expanded = expand_home_directory(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().ok()?.join(expanded)
    };
    Some(normalize_absolute_path(&absolute))
}

fn expand_home_directory(path: &Path) -> PathBuf {
    if let Some(path_str) = path.to_str()
        && let Some(rest) = path_str.strip_prefix('~')
        && let Some(home) = std::env::home_dir()
    {
        if rest.is_empty() {
            return home;
        } else if let Some(rest) = rest.strip_prefix('/') {
            return home.join(rest.trim_start_matches('/'));
        } else if cfg!(windows)
            && let Some(rest) = rest.strip_prefix('\\')
        {
            return home.join(rest.trim_start_matches('\\'));
        }
    }
    path.to_path_buf()
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}
