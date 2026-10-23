// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Cloud-first catalog allocation for a run with no previous world-state allocation.

use super::{
    catalog::SkillCatalog,
    render::{
        PreparedSkillCatalog, RenderedSkillCatalogs, SkillCatalogRenderPolicy, SkillMetadataBudget,
        render_available_skills, render_prepared_skill_catalogs,
    },
};

// Reserve a quarter for filesystem skills before their inventory is known.
const FILESYSTEM_BUDGET_DIVISOR: usize = 4;

struct CatalogContributions<'a> {
    executor: &'a SkillCatalog,
    cloud: &'a SkillCatalog,
    host: &'a SkillCatalog,
}

struct CatalogContext {
    metadata_budget: SkillMetadataBudget,
    include_usage: bool,
}

/// Uses the upstream first-step allocation. The run-bound prompt has no previous world-state
/// snapshot, so there is no persisted cap or fingerprint to recover between sampling steps.
pub(super) fn render_catalogs(
    executor: &SkillCatalog,
    cloud: &SkillCatalog,
    host: &SkillCatalog,
    metadata_budget: SkillMetadataBudget,
    include_usage: bool,
) -> RenderedSkillCatalogs {
    CatalogContext {
        metadata_budget,
        include_usage,
    }
    .render_catalogs(&CatalogContributions {
        executor,
        cloud,
        host,
    })
}

impl CatalogContext {
    fn render_catalogs(&self, catalogs: &CatalogContributions<'_>) -> RenderedSkillCatalogs {
        let total_limit = self.metadata_budget.limit();
        let mut cloud_limit = total_limit - total_limit / FILESYSTEM_BUDGET_DIVISOR;
        let mut executor = PreparedSkillCatalog::new(
            catalogs.executor,
            SkillCatalogRenderPolicy::ExtensionCompatible,
        );
        executor.prefer_cloud_skills(catalogs.cloud);
        let (mut rendered, _) = self.render_with_cloud_limit(catalogs, &executor, cloud_limit);
        if let Some(limit) =
            self.rebalance_to_retain_all_skills(catalogs, &executor, &rendered, cloud_limit)
        {
            cloud_limit = limit;
            (rendered, _) = self.render_with_cloud_limit(catalogs, &executor, cloud_limit);
        }
        rendered
    }

    // Lower the Cloud cap only if omitted filesystem entries can fit without losing Cloud entries.
    fn rebalance_to_retain_all_skills(
        &self,
        catalogs: &CatalogContributions<'_>,
        executor: &PreparedSkillCatalog<'_>,
        rendered: &RenderedSkillCatalogs,
        current_cloud_limit: usize,
    ) -> Option<usize> {
        if ![&rendered.executor, &rendered.host]
            .into_iter()
            .flatten()
            .any(|catalog| catalog.report.omitted_count > 0)
        {
            return None;
        }

        let shared = render_prepared_skill_catalogs(
            executor,
            &PreparedSkillCatalog::new(
                catalogs.cloud,
                SkillCatalogRenderPolicy::ExtensionCompatible,
            ),
            &PreparedSkillCatalog::new(catalogs.host, SkillCatalogRenderPolicy::CoreCompatible),
            self.metadata_budget,
            self.include_usage,
        );
        if [&shared.executor, &shared.cloud, &shared.host]
            .into_iter()
            .flatten()
            .any(|catalog| catalog.report.omitted_count > 0)
        {
            return None;
        }

        let cloud_limit = shared.cloud.as_ref().map_or(0, |cloud| {
            cloud.metadata_cost(self.metadata_budget, self.include_usage)
        });
        (cloud_limit < current_cloud_limit).then_some(cloud_limit)
    }

    // Re-render through the same buckets after accepting a smaller Cloud cap.
    fn render_with_cloud_limit(
        &self,
        catalogs: &CatalogContributions<'_>,
        executor: &PreparedSkillCatalog<'_>,
        cloud_limit: usize,
    ) -> (RenderedSkillCatalogs, SkillMetadataBudget) {
        let cloud_budget = self.metadata_budget.with_limit(cloud_limit);
        let cloud = render_available_skills(
            catalogs.cloud,
            SkillCatalogRenderPolicy::ExtensionCompatible,
            cloud_budget,
            self.include_usage,
        );
        // Empty or disabled cloud catalogs leave the full allowance to filesystem skills.
        let cloud_cost = cloud.as_ref().map_or(0, |rendered| {
            rendered.metadata_cost(cloud_budget, self.include_usage)
        });
        let filesystem_budget = self
            .metadata_budget
            .with_limit(self.metadata_budget.limit().saturating_sub(cloud_cost));
        let mut rendered = render_prepared_skill_catalogs(
            executor,
            &PreparedSkillCatalog::new(
                &SkillCatalog::default(),
                SkillCatalogRenderPolicy::ExtensionCompatible,
            ),
            &PreparedSkillCatalog::new(catalogs.host, SkillCatalogRenderPolicy::CoreCompatible),
            filesystem_budget,
            self.include_usage,
        );
        rendered.cloud = cloud;

        (rendered, filesystem_budget)
    }
}
