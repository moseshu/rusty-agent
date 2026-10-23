// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Upstream read pagination and model-facing response contract.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

use super::ExecutorReadSnapshot;
use super::catalog::SkillResourceId;
use super::provider::MAX_SKILL_RESOURCE_CONTENT_BYTES;
use super::provider::SkillReadContext;
use super::provider::SkillReadRequest;
use super::render::build_alias_plan;

use super::FunctionCallError;
use super::MAX_HANDLE_BYTES;
use super::SkillToolAuthority;
use super::SkillToolAuthoritySelector;
use super::SkillToolContext;
use super::pagination_cursor;
use super::parse_args;
use super::parse_pagination_cursor;
use super::serialized_len;
use super::skill_json_output;
use super::validate_handle;
use ra_core::tool::{ToolContext, ToolOutput};

pub(super) const TOOL_NAME: &str = "read";

#[derive(Deserialize, JsonSchema)]
pub(super) struct ReadArgs {
    package: String,
    resource: Option<String>,
    cursor: Option<String>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct ReadResponse {
    resource: String,
    contents: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    skill_root: Option<String>,
    next_cursor: Option<String>,
}

pub(super) const DESCRIPTION: &str = "Read one page from a skill. Pass its provided package directly; root aliases are resolved automatically. Omit resource to read SKILL.md; to read another file, use the same package and pass the file's complete skill:// identifier as resource. For executor-backed skills, skill_root is the skill's absolute directory in the executor filesystem and can be used to locate bundled scripts. If the package is not provided, use skills.list to find it. Pass next_cursor back as cursor to continue the same snapshot while it is cached; omit cursor to read again.";

// Preserve the upstream pagination and recovery order for source-by-source review.
#[allow(clippy::too_many_lines, clippy::single_match_else)]
pub(super) async fn handle(
    context: &SkillToolContext,
    call: &ToolContext<'_>,
) -> Result<ToolOutput, FunctionCallError> {
    let args: ReadArgs = parse_args(call)?;
    let response_byte_budget = context.response_byte_budget();
    validate_handle("package", &args.package, MAX_HANDLE_BYTES)?;
    if let Some(resource) = args.resource.as_deref() {
        validate_handle("resource", resource, MAX_HANDLE_BYTES)?;
    }

    let mut selected_skill = None;
    for selector in [
        super::SkillToolAuthoritySelector::Cloud,
        super::SkillToolAuthoritySelector::Executor,
    ] {
        let catalog = context.catalog(call, selector).await;
        let alias_plan = build_alias_plan(
            &catalog
                .entries
                .iter()
                .filter(|entry| entry.is_model_visible())
                .collect::<Vec<_>>(),
        );
        if let Some(entry) = catalog.entries.into_iter().find(|entry| {
            entry.enabled
                && (entry.id.0 == args.package
                    || alias_plan
                        .as_ref()
                        .and_then(|plan| plan.shorten(&entry.id.0))
                        .is_some_and(|alias| alias == args.package))
                && SkillToolAuthority::from_authority(&entry.authority)
                    .is_some_and(|authority| authority.selector() == selector)
        }) {
            selected_skill = Some((entry, selector));
            break;
        }
    }
    let Some((skill_entry, output_authority)) = selected_skill else {
        return Err(FunctionCallError::RespondToModel(
            "skill package is not available".to_string(),
        ));
    };
    let authority = skill_entry.authority.clone();
    let package = skill_entry.id.clone();
    let main_prompt = skill_entry.main_prompt.clone();
    let requested_resource = match args.resource {
        None => main_prompt.clone(),
        Some(resource) if resource == main_prompt.as_str() => main_prompt.clone(),
        Some(resource) => main_prompt
            .bind_environment_package_resource(&package, resource.clone())
            .unwrap_or_else(|| SkillResourceId::new(resource)),
    };
    let (read_context, access_key) = match output_authority {
        SkillToolAuthoritySelector::Cloud => (SkillReadContext::Cloud, None),
        SkillToolAuthoritySelector::Executor => {
            let fs = requested_resource
                .environment_path()
                .filter(|(id, path)| {
                    *id == super::provider::LOCAL_ENVIRONMENT_ID && path.is_absolute()
                })
                .and_then(|_| context.executor_roots.get(&authority.id))
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "skill environment is not available for this callback".to_string(),
                    )
                })?;
            (
                SkillReadContext::Executor { fs },
                Some(Arc::clone(fs.filesystem())),
            )
        }
    };
    // Reuse the snapshot to avoid reading the whole file for each page. File edits
    // are not detected on cache hits, so a continuation can return old contents.
    // Snapshots have no expiry and are not cleared after the final page.
    let cached = args.cursor.as_deref().and_then(|cursor| {
        let access = access_key.as_ref()?;
        let snapshot = context
            .executor_read_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = snapshot.as_ref().filter(|snapshot| {
            snapshot.authority == authority
                && snapshot.package == package
                && snapshot.result.resource == requested_resource
                && Arc::ptr_eq(&snapshot.access, access)
        })?;
        let start = parse_pagination_cursor(
            Some(cursor),
            snapshot.result.contents.as_str(),
            "skills.read",
        )
        .ok()?;
        Some((Arc::clone(&snapshot.result), start))
    });
    let (result, start) = match cached {
        Some(cached) => cached,
        None => {
            let result = context
                .providers
                .read(SkillReadRequest {
                    authority: authority.clone(),
                    package: package.clone(),
                    resource: requested_resource.clone(),
                    context: read_context,
                })
                .await
                .map_err(|err| {
                    tracing::warn!(
                        error = %err,
                        run_id = %call.run().run_id(),
                        call_id = %call.call_id(),
                        resource = requested_resource.as_str(),
                        "skills.read provider request failed"
                    );
                    FunctionCallError::RespondToModel("failed to read skill resource".to_string())
                })?;
            if result.resource != requested_resource {
                return Err(FunctionCallError::Fatal(
                    "skill provider returned a different resource".to_string(),
                ));
            }
            let start = parse_pagination_cursor(
                args.cursor.as_deref(),
                result.contents.as_str(),
                "skills.read",
            )?;
            (Arc::new(result), start)
        }
    };
    if start > result.contents.len() || !result.contents.is_char_boundary(start) {
        return Err(FunctionCallError::RespondToModel(
            "skills.read cursor is invalid".to_string(),
        ));
    }
    let skill_root = if output_authority == super::SkillToolAuthoritySelector::Executor {
        main_prompt
            .environment_path()
            .and_then(|(_, path)| path.parent())
            .map(|path| path.to_string_lossy().into_owned())
    } else {
        None
    };
    let response = page_response(
        result.resource.as_str(),
        &result.contents,
        skill_root.as_deref(),
        start,
        response_byte_budget,
    )?;
    let output = skill_json_output(&response, output_authority)?;
    if output_authority == super::SkillToolAuthoritySelector::Executor
        && response.next_cursor.is_some()
        && result.contents.len() <= MAX_SKILL_RESOURCE_CONTENT_BYTES
        && let Some(access) = access_key
    {
        *context
            .executor_read_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ExecutorReadSnapshot {
            authority,
            package,
            access,
            result,
        });
    }

    Ok(output)
}

fn page_response(
    resource: &str,
    contents: &str,
    skill_root: Option<&str>,
    start: usize,
    max_response_bytes: usize,
) -> Result<ReadResponse, FunctionCallError> {
    let response = |end, next_cursor| ReadResponse {
        resource: resource.to_string(),
        contents: contents[start..end].to_string(),
        skill_root: skill_root.map(str::to_string),
        next_cursor,
    };
    let complete = response(contents.len(), None);
    if serialized_len(&complete)? <= max_response_bytes {
        return Ok(complete);
    }

    let mut lower = start;
    let mut upper = contents.len();
    let mut best = None;
    while lower < upper {
        // Probe strictly above lower so a multibyte character cannot stall the search.
        let end = super::ceil_char_boundary(contents, lower + (upper - lower) / 2 + 1);
        let candidate = response(end, Some(pagination_cursor(contents, end)));
        if serialized_len(&candidate)? <= max_response_bytes {
            lower = end;
            best = Some(candidate);
        } else {
            upper = super::floor_char_boundary(contents, end.saturating_sub(1));
        }
    }
    best.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "skills.read response budget leaves no room for contents".to_string(),
        )
    })
}
