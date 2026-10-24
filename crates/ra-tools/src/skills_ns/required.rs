// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Source-scoped requirements checked against discovery before inference.

use super::catalog::SkillCatalog;

/// Shared product-configuration merge; replacement keeps position and an empty list removes it.
#[doc(hidden)]
pub fn set_required_skills(
    requirements: &mut Vec<(String, Vec<String>)>,
    environment_id: String,
    names: Vec<String>,
) {
    if names.is_empty() {
        requirements.retain(|(id, _)| id != &environment_id);
    } else if let Some((_, existing)) = requirements
        .iter_mut()
        .find(|(id, _)| id == &environment_id)
    {
        *existing = names;
    } else {
        requirements.push((environment_id, names));
    }
}

/// Names that must be supplied by a selected environment before inference.
pub struct EnvironmentSkillRequirements<'a> {
    /// Environment identity from the main resource's binding, not its authority id.
    pub environment_id: &'a str,
    /// Exact, case-sensitive names required from this environment.
    pub skill_names: &'a [String],
}

/// Checks enabled Executor and Cloud entries, including prompt-hidden skills.
///
/// `catalogs` is absent when the skills integration is unavailable. Host catalogs never satisfy
/// environment requirements. Requirements check discovery only; they neither select nor read a skill.
/// Returns the first unavailable requirement using the upstream diagnostic.
///
/// # Errors
/// Returns the upstream unavailable-extension or missing-skill message.
pub fn validate_required_skills<'a>(
    catalogs: Option<(&SkillCatalog, Option<&SkillCatalog>)>,
    environments: impl IntoIterator<Item = EnvironmentSkillRequirements<'a>>,
) -> Result<(), String> {
    let mut environments = environments.into_iter().peekable();
    if environments.peek().is_none() {
        return Ok(());
    }
    let (executor, cloud) = catalogs.ok_or_else(|| {
        "Required skills cannot be validated because the skills extension is unavailable"
            .to_string()
    })?;
    for EnvironmentSkillRequirements {
        environment_id,
        skill_names,
    } in environments
    {
        for name in skill_names {
            let available = executor
                .entries
                .iter()
                .chain(cloud.into_iter().flat_map(|catalog| &catalog.entries))
                .any(|entry| {
                    entry.enabled
                        && entry.name == *name
                        && entry
                            .main_prompt
                            .environment_path()
                            .is_some_and(|(id, _)| id == environment_id)
                });
            if !available {
                return Err(format!(
                    "Required skill {name:?} from environment {environment_id:?} is unavailable"
                ));
            }
        }
    }
    Ok(())
}
