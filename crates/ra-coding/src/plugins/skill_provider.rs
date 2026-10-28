// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The services' effective Host skills as the run's Host skill catalog.
//!
//! The source turn takes its skills from the thread's immutable skills snapshot. Each run here
//! lists the current services snapshot for its cwd, so a configuration or plugin change reaches
//! the next run; explicit `$name` mentions then load main prompts through the existing Host
//! prompt lifecycle. Reads go through the listed snapshot's own filesystem for the skill.

use std::sync::{Arc, Mutex, PoisonError};

use ra_paths::AbsolutePathBuf;
use ra_tools::skills_ns::catalog::{
    HostSkillPromptKind, SkillAuthority, SkillCatalog, SkillCatalogEntry, SkillPackageId,
    SkillProviderError, SkillReadResult, SkillResourceId, SkillSourceKind,
};
use ra_tools::skills_ns::provider::{
    SkillListQuery, SkillProvider, SkillProviderFuture, SkillReadContext, SkillReadRequest,
};

use super::PluginServices;
use crate::skills::HostSkillsSnapshot;

/// A Host skill provider backed by explicitly selected plugin services.
pub struct PluginSkillProvider {
    services: Arc<PluginServices>,
    cwd: AbsolutePathBuf,
    // The snapshot behind the latest listing; reads resolve skills against it.
    listed: Mutex<Option<HostSkillsSnapshot>>,
}

impl PluginSkillProvider {
    /// Lists the effective skills for `cwd` on each run.
    #[must_use]
    pub fn new(services: Arc<PluginServices>, cwd: AbsolutePathBuf) -> Self {
        Self {
            services,
            cwd,
            listed: Mutex::new(None),
        }
    }
}

impl SkillProvider for PluginSkillProvider {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            let snapshot = self
                .services
                .snapshot(self.cwd.clone())
                .await
                .map_err(|error| {
                    SkillProviderError::new(format!("failed to reload config: {error}"))
                })?;
            let skills = snapshot.skills;
            let catalog = catalog_from_outcome(skills.outcome());
            *self.listed.lock().unwrap_or_else(PoisonError::into_inner) = Some(skills);
            Ok(catalog)
        })
    }

    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            let SkillReadContext::Host {
                snapshot: Some(run_snapshot),
            } = request.context
            else {
                return Err(SkillProviderError::new(
                    "host skill provider requires a host skills snapshot",
                ));
            };
            let Some(resource_path) = run_snapshot.resource_path(request.resource.as_str()) else {
                return Err(SkillProviderError::new(format!(
                    "host skill resource is not loaded: {}",
                    request.resource.as_str()
                )));
            };
            let listed = self
                .listed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let skill = listed.as_ref().and_then(|listed| {
                listed
                    .outcome()
                    .skills
                    .iter()
                    .find(|skill| {
                        ra_paths::PathUri::from_host_native_path(resource_path)
                            .is_ok_and(|path| path == skill.path_to_skills_md)
                    })
                    .cloned()
                    .map(|skill| (listed.clone(), skill))
            });
            let Some((listed, skill)) = skill else {
                return Err(SkillProviderError::new(format!(
                    "host skill resource is no longer listed: {}",
                    request.resource.as_str()
                )));
            };
            let contents = listed.read_skill_text(&skill).await.map_err(|error| {
                SkillProviderError::new(format!(
                    "failed to read host skill resource {}: {error}",
                    request.resource.as_str()
                ))
            })?;
            Ok(SkillReadResult {
                resource: request.resource,
                contents,
            })
        })
    }
}

/// Maps one load outcome to the authority-aware catalog, as the source Host provider does.
///
/// Entries also carry the local catalog's prompt kind, so Agent Plugin skills keep their bounded
/// main prompt; the local catalog has no field for the source's tool dependencies.
fn catalog_from_outcome(outcome: &crate::skills::SkillLoadOutcome) -> SkillCatalog {
    let root_order_by_path = outcome
        .skill_roots_in_discovery_order()
        .enumerate()
        .map(|(index, root)| (root.as_path(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let mut catalog = SkillCatalog {
        entries: Vec::new(),
        warnings: outcome
            .errors
            .iter()
            .map(|err| {
                format!(
                    "Failed to load skill at {}: {}",
                    err.path.display(),
                    err.message
                )
            })
            .collect(),
    };

    for (skill, enabled) in outcome.skills_with_enabled() {
        let mut entry = catalog_entry_from_skill(skill, enabled).with_host_prompt_kind(
            if outcome.is_agent_plugin_skill(skill) {
                HostSkillPromptKind::AgentPlugin
            } else {
                HostSkillPromptKind::Core
            },
        );
        if let Some(discovery_path) =
            outcome.skill_discovery_path_for_path(&skill.path_to_skills_md)
        {
            // Keep the catalog's existing forward-slash display on Windows; lookups use PathUri.
            entry = entry.with_display_path(
                discovery_path
                    .inferred_native_path_string()
                    .replace('\\', "/"),
            );
        }
        if let Some(root) = outcome.skill_root_for_path(&skill.path_to_skills_md) {
            // Alias labels share the display path's separators so catalog rendering stays stable.
            entry = entry.with_alias_root(root.to_string_lossy().replace('\\', "/"));
            if let Some(root_order) = root_order_by_path.get(root.as_path()) {
                entry = entry.with_alias_root_order(*root_order);
            }
        }
        catalog.push_entry(entry);
    }

    catalog
}

fn catalog_entry_from_skill(
    skill: &crate::skills::SkillMetadata,
    enabled: bool,
) -> SkillCatalogEntry {
    let skill_path = skill.path_to_skills_md.inferred_native_path_string();
    // Preserve model-visible paths while resource IDs retain the native spelling used by clients.
    let display_path = skill_path.replace('\\', "/");
    let mut entry = SkillCatalogEntry::new(
        SkillPackageId(skill_path.clone()),
        SkillAuthority::new(SkillSourceKind::Host, "host"),
        skill.name.clone(),
        skill.description.clone(),
        SkillResourceId::new(skill_path),
    )
    .with_short_description(skill.short_description.clone())
    .with_display_path(display_path)
    .with_prompt_scope(skill.scope);

    if !enabled {
        entry = entry.disabled();
    }
    if !skill.allows_implicit_invocation() {
        entry = entry.hidden_from_prompt();
    }

    entry
}
