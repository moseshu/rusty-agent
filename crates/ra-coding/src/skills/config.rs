// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Host skill enablement configuration entries and their effective ordered rules.

use std::{collections::HashSet, path::PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// One configured skill override, selected by exactly one of `path` or `name`.
///
/// Codex reads these entries from `[[skills.config]]`. Rusty accepts them from the host in any
/// serialization format, without loading Codex's configuration files or layer stack.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SkillConfig {
    /// Path-based selector. Codex stores an absolute path; relative paths are ignored here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Name-based selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether the selected skills are enabled.
    pub enabled: bool,
}

/// Selects configured skills by their full name or canonical document path.
///
/// Paths use native local paths instead of Codex's `PathUri`, because Host discovery is local.
/// As with upstream effective rules, selectors are already resolved: names match exactly and
/// paths must use the canonical `SKILL.md` path, not its directory or a display alias.
/// [`SkillConfigRules::from_configs`] performs that resolution for configuration entries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SkillConfigRuleSelector {
    /// Complete, case-sensitive skill name, including its plugin namespace when present.
    Name(String),
    /// Canonical native path to the skill document.
    Path(PathBuf),
}

/// Enables or disables every skill matched by its selector.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SkillConfigRule {
    /// Name or document path to match.
    pub selector: SkillConfigRuleSelector,
    /// Whether matching skills are enabled after this rule is applied.
    pub enabled: bool,
}

/// Ordered effective skill enablement rules from the host configuration.
///
/// The default has no overrides, so discovered skills remain enabled. Build rules from
/// configuration entries with [`SkillConfigRules::from_configs`], which canonicalizes paths and
/// drops invalid entries as Codex's loader does. Directly constructed rules are used as given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SkillConfigRules {
    /// Rules applied in order, with later matches overriding earlier ones.
    pub entries: Vec<SkillConfigRule>,
}

impl SkillConfigRules {
    /// Resolves configuration entries into effective rules, lowest precedence first.
    ///
    /// Path selectors are canonicalized when the path exists, and otherwise kept as written.
    /// Names are trimmed. Entries with an empty name, with both or neither selectors, or with a
    /// relative path are ignored with a warning. A repeated selector replaces its earlier rule and
    /// moves to the end. Entries from several configuration layers can be concatenated in
    /// precedence order; Codex applies the same replacement across its user and session layers.
    #[must_use]
    pub fn from_configs(configs: impl IntoIterator<Item = SkillConfig>) -> Self {
        let mut entries = Vec::new();
        for entry in configs {
            let Some(selector) = skill_config_rule_selector(&entry) else {
                continue;
            };
            // Preserve layer order so a later name selector can override an earlier path selector
            // for the same loaded skill.
            entries.retain(|entry: &SkillConfigRule| entry.selector != selector);
            entries.push(SkillConfigRule {
                selector,
                enabled: entry.enabled,
            });
        }

        Self { entries }
    }

    /// Applies rules in order; later rules override earlier rules for matching skills.
    ///
    /// Explicit path selectors remain effective even when no current skill matches.
    #[allow(clippy::needless_pass_by_value)] // Preserve the upstream cloneable iterator contract.
    pub fn resolve_disabled_paths<'a>(
        &self,
        skills: impl IntoIterator<Item = (&'a str, &'a PathBuf)> + Clone,
    ) -> HashSet<PathBuf> {
        let mut disabled_paths = HashSet::new();

        for entry in &self.entries {
            match &entry.selector {
                SkillConfigRuleSelector::Path(path) => {
                    if entry.enabled {
                        disabled_paths.remove(path);
                    } else {
                        disabled_paths.insert(path.clone());
                    }
                }
                SkillConfigRuleSelector::Name(name) => {
                    for (skill_name, path) in skills.clone() {
                        if skill_name != name {
                            continue;
                        }
                        if entry.enabled {
                            disabled_paths.remove(path);
                        } else {
                            disabled_paths.insert(path.clone());
                        }
                    }
                }
            }
        }

        disabled_paths
    }
}

fn skill_config_rule_selector(entry: &SkillConfig) -> Option<SkillConfigRuleSelector> {
    match (entry.path.as_ref(), entry.name.as_deref()) {
        // Codex deserializes an absolute path type; without a configuration base directory, a
        // relative host path cannot be resolved and is ignored rather than guessed.
        (Some(path), None) if !path.is_absolute() => {
            warn!("ignoring skills.config entry with a relative path selector");
            None
        }
        (Some(path), None) => Some(SkillConfigRuleSelector::Path(
            path.canonicalize().unwrap_or_else(|_| path.clone()),
        )),
        (None, Some(name)) => {
            let name = name.trim();
            if name.is_empty() {
                warn!("ignoring empty skills.config name override");
                None
            } else {
                Some(SkillConfigRuleSelector::Name(name.to_string()))
            }
        }
        (Some(_), Some(_)) => {
            warn!("ignoring skills.config entry with both path and name selectors");
            None
        }
        (None, None) => {
            warn!("ignoring skills.config entry without a path or name selector");
            None
        }
    }
}
