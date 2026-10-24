// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Explicit main-prompt reads and contextual skill fragments.

use ra_core::{
    error::{Error, Result},
    item::{ContentBlock, Message, MessageRole, ModelInputItem},
};

use super::{
    SkillInput, SkillToolAuthority, SkillToolAuthoritySelector, SkillToolContext,
    catalog::{
        HostSkillPromptKind, SkillCatalog, SkillCatalogEntry, SkillReadResult, SkillSourceKind,
    },
    collect_explicit_skill_mentions, collect_host_skill_mentions,
    provider::{SkillReadContext, SkillReadRequest},
    required::{EnvironmentSkillRequirements, validate_required_skills},
};

impl SkillToolContext {
    pub(super) async fn contribute_turn_input(
        &self,
        run_id: &ra_core::state::RunId,
        input: &[ModelInputItem],
        required: &[(String, Vec<String>)],
        host_surface: bool,
    ) -> Result<Vec<Message>> {
        if input.is_empty() && required.is_empty() {
            return Ok(Vec::new());
        }
        let mut executor = self
            .catalog_for_run(run_id, SkillToolAuthoritySelector::Executor)
            .await;
        let mut cloud = self
            .catalog_for_run(run_id, SkillToolAuthoritySelector::Cloud)
            .await;
        for (catalog, selector) in [
            (&mut executor, SkillToolAuthoritySelector::Executor),
            (&mut cloud, SkillToolAuthoritySelector::Cloud),
        ] {
            catalog.entries.retain(|entry| {
                SkillToolAuthority::from_authority(&entry.authority)
                    .is_some_and(|authority| authority.selector() == selector)
            });
        }
        validate_required_skills(
            Some((&executor, Some(&cloud))),
            required.iter().map(
                |(environment_id, skill_names)| EnvironmentSkillRequirements {
                    environment_id,
                    skill_names,
                },
            ),
        )
        .map_err(Error::config)?;
        let mut catalog = SkillCatalog::default();
        if !host_surface {
            catalog.extend(executor);
        }
        let mut host = self.host_catalog_for_run(run_id).await;
        host.entries
            .retain(|entry| entry.authority.kind == SkillSourceKind::Host);
        let host_snapshot = super::HostSkillsSnapshot::from_catalog(&host);
        let mut core_host = SkillCatalog::default();
        for entry in host.entries.drain(..) {
            if entry.host_prompt_kind.is_some() {
                core_host.push_entry(entry);
            } else {
                catalog.push_entry(entry);
            }
        }
        if !host_surface {
            catalog.extend(cloud);
        }
        let inputs: Vec<_> = input
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::Message(message) if message.role() == MessageRole::User => {
                    Some(message)
                }
                _ => None,
            })
            .flat_map(Message::content)
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(SkillInput::Text {
                    text: text.text().to_string(),
                }),
                _ => None,
            })
            .collect();
        let mut selected = collect_explicit_skill_mentions(&inputs, &catalog);
        let superseded_names: std::collections::HashSet<_> = selected
            .iter()
            .filter(|entry| entry.authority.kind != SkillSourceKind::Host)
            .map(|entry| entry.name.clone())
            .collect();
        selected.extend(collect_host_skill_mentions(
            &inputs,
            &core_host,
            &std::collections::HashMap::new(),
        ));
        Ok(self
            .load_skill_prompts(run_id, selected, &superseded_names, &host_snapshot)
            .await)
    }

    async fn load_skill_prompts(
        &self,
        run_id: &ra_core::state::RunId,
        selected: Vec<SkillCatalogEntry>,
        superseded_names: &std::collections::HashSet<String>,
        host_snapshot: &super::HostSkillsSnapshot,
    ) -> Vec<Message> {
        let mut extension_fragments = Vec::new();
        let mut host_fragments = Vec::new();
        let mut injected_host_paths = std::collections::HashSet::new();
        for entry in selected {
            let core_host =
                entry.authority.kind == SkillSourceKind::Host && entry.host_prompt_kind.is_some();
            match self.read_main_prompt(&entry, host_snapshot).await {
                Ok(result) => {
                    let contents = if entry.authority.kind == SkillSourceKind::Host
                        && entry.host_prompt_kind == Some(HostSkillPromptKind::Core)
                    {
                        &result.contents
                    } else {
                        truncate(&result.contents, 8_000)
                    };
                    if contents.len() < result.contents.len() {
                        tracing::warn!(%run_id, "Skill `{}` exceeded the main prompt context limit and was truncated.", entry.name);
                    }
                    let fragment = Message::user(render_skill(&entry, contents));
                    let host_path = entry
                        .main_prompt
                        .as_str()
                        .strip_prefix("skill://")
                        .unwrap_or(entry.main_prompt.as_str())
                        .replace('\\', "/");
                    if core_host {
                        // Core reads and reports Host outcomes before suppressing repeated or
                        // superseded fragments, then places retained Host items before extensions.
                        if !superseded_names.contains(&entry.name)
                            && !injected_host_paths.contains(&host_path)
                        {
                            host_fragments.push(fragment);
                        }
                    } else {
                        if entry.authority.kind == SkillSourceKind::Host {
                            injected_host_paths.insert(host_path);
                        }
                        extension_fragments.push(fragment);
                    }
                }
                Err(message) => {
                    if core_host {
                        tracing::warn!(%run_id, "Failed to load skill {} at {}: {message}", entry.name, entry.main_prompt.as_str());
                    } else {
                        tracing::warn!(%run_id, "Failed to load skill `{}`: {message}", entry.name);
                    }
                }
            }
        }
        host_fragments.extend(extension_fragments);
        host_fragments
    }

    async fn read_main_prompt(
        &self,
        entry: &SkillCatalogEntry,
        host_snapshot: &super::HostSkillsSnapshot,
    ) -> std::result::Result<SkillReadResult, String> {
        if entry.authority.kind == SkillSourceKind::Host && entry.host_prompt_kind.is_some() {
            let contents = host_snapshot
                .read_skill_text(entry.main_prompt.as_str())
                .await
                .map_err(|error| error.to_string())?;
            return Ok(SkillReadResult {
                resource: entry.main_prompt.clone(),
                contents,
            });
        }
        let context = match entry.authority.kind {
            SkillSourceKind::Host => SkillReadContext::Host {
                snapshot: Some(host_snapshot),
            },
            SkillSourceKind::Cloud => SkillReadContext::Cloud,
            SkillSourceKind::Executor => {
                let fs = self
                    .executor_roots
                    .get(&entry.authority.id)
                    .ok_or_else(|| {
                        "skill environment is not available for this callback".to_string()
                    })?;
                SkillReadContext::Executor { fs }
            }
            _ => return Err("skill provider is not configured".to_string()),
        };
        self.providers
            .read(SkillReadRequest {
                authority: entry.authority.clone(),
                package: entry.id.clone(),
                resource: entry.main_prompt.clone(),
                context,
            })
            .await
            .map_err(|error| error.message)
    }
}

fn render_skill(entry: &SkillCatalogEntry, contents: &str) -> String {
    let core_host =
        entry.authority.kind == SkillSourceKind::Host && entry.host_prompt_kind.is_some();
    let name = if core_host {
        &entry.name
    } else {
        truncate(&entry.name, super::render::MAX_SKILL_NAME_BYTES)
    };
    let path = if core_host {
        entry.main_prompt.as_str()
    } else {
        truncate(entry.rendered_path(), 1_024)
    };
    let resource_access = (!core_host && !entry.prompt_visible)
        .then(|| SkillToolAuthority::from_authority(&entry.authority))
        .flatten()
        .map(|authority| {
            let metadata = serde_json::json!({
                "authority": authority,
                "package": entry.id.0,
                "main_resource": entry.main_prompt.as_str(),
            });
            format!("\n<resource_access>{metadata}</resource_access>")
        })
        .unwrap_or_default();
    format!(
        "<skill>\n<name>{name}</name>\n<path>{path}</path>{resource_access}\n{contents}\n</skill>"
    )
}

fn truncate(contents: &str, max_bytes: usize) -> &str {
    &contents[..super::floor_char_boundary(contents, max_bytes.min(contents.len()))]
}
