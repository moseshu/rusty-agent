// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Run-bound catalog contribution over the same executor cache as the namespace tools.

use std::num::NonZeroUsize;

use ra_core::state::RunId;

use super::{
    SkillToolAuthority, SkillToolAuthoritySelector, SkillToolContext,
    render::{SkillMetadataBudget, render_available_skills, skill_metadata_budget},
    warnings::bounded_warnings,
};

#[derive(Clone)]
pub(crate) struct SkillCatalogPrompt {
    context: SkillToolContext,
    run_id: Option<RunId>,
    context_window: Option<i64>,
    max_context_tokens: Option<NonZeroUsize>,
    include_skills_usage_instructions: bool,
}

impl SkillCatalogPrompt {
    pub(super) fn new(context: SkillToolContext) -> Self {
        Self {
            context,
            run_id: None,
            context_window: None,
            max_context_tokens: None,
            include_skills_usage_instructions: false,
        }
    }

    pub(crate) fn set_context_window(&mut self, context_window: Option<i64>) {
        self.context_window = context_window;
    }

    pub(crate) fn set_max_context_tokens(&mut self, max_context_tokens: NonZeroUsize) {
        self.max_context_tokens = Some(max_context_tokens);
    }

    pub(crate) fn include_usage_instructions(&mut self) {
        self.include_skills_usage_instructions = true;
    }

    fn metadata_budget(&self) -> SkillMetadataBudget {
        skill_metadata_budget(self.context_window, self.max_context_tokens)
    }

    pub(crate) fn token_budget(&self) -> usize {
        super::catalog_prompt::fixed_prompt_token_budget(self.include_skills_usage_instructions)
            .saturating_add(self.metadata_budget().token_limit())
    }

    pub(crate) fn bind(&mut self, run_id: &RunId) {
        self.run_id = Some(run_id.clone());
    }

    pub(crate) async fn render(&self) -> Option<String> {
        let run_id = self.run_id.as_ref()?;
        let mut catalog = super::catalog::SkillCatalog::default();
        for selector in [
            SkillToolAuthoritySelector::Executor,
            SkillToolAuthoritySelector::Cloud,
        ] {
            let mut source = self.context.catalog_for_run(run_id, selector).await;
            source.entries.retain(|entry| {
                SkillToolAuthority::from_authority(&entry.authority)
                    .is_some_and(|authority| authority.selector() == selector)
            });
            catalog.extend(source);
        }
        for warning in bounded_warnings(&catalog.warnings) {
            tracing::warn!(%run_id, "{warning}");
        }
        let rendered = render_available_skills(
            &catalog,
            self.metadata_budget(),
            self.include_skills_usage_instructions,
        )?;
        let report = &rendered.report;
        tracing::debug!(
            total_count = report.total_count,
            included_count = report.included_count,
            omitted_count = report.omitted_count,
            truncated_description_chars = report.truncated_description_chars,
            truncated_description_count = report.truncated_description_count,
            metadata_cost = rendered.metadata_cost(
                self.metadata_budget(),
                self.include_skills_usage_instructions
            ),
            "rendered skill catalog"
        );
        if let Some(warning) = report.warning_message() {
            tracing::warn!(%run_id, "{warning}");
        }
        rendered
            .into_fragment(self.include_skills_usage_instructions)
            .map(|fragment| fragment.render())
    }
}
