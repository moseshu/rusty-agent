// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    unreachable_pub,
    reason = "Output internals are exposed only through the explicit test facade."
)]
#![allow(
    missing_docs,
    reason = "Preserve the source hook discovery and execution contract."
)]
use crate::hooks::HookPromptFragment;
use ra_core::sandbox::token_truncation::TruncationPolicy;
use ra_core::sandbox::token_truncation::approx_token_count;
use ra_core::sandbox::token_truncation::formatted_truncate_text;
use ra_core::session::SessionId as ThreadId;
use ra_paths::AbsolutePathBuf;
use tokio::fs;
use tracing::warn;
use uuid::Uuid;

const HOOK_OUTPUTS_DIR: &str = "hook_outputs";
pub const DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT: usize = 2_500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdditionalContextLimit {
    token_limit: usize,
}

impl AdditionalContextLimit {
    #[must_use]
    pub fn from_config(value: Option<usize>) -> Self {
        Self {
            token_limit: value.unwrap_or(DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT),
        }
    }
}

impl Default for AdditionalContextLimit {
    fn default() -> Self {
        Self::from_config(/*value*/ None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditionalContext {
    pub text: String,
    pub limit: AdditionalContextLimit,
}

#[derive(Clone)]
pub struct HookOutputSpiller {
    output_dir: AbsolutePathBuf,
}

impl HookOutputSpiller {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "Retain the source signature; callers hand over ownership."
    )]
    #[must_use]
    pub fn new(thread_id: ThreadId) -> Self {
        Self {
            output_dir: AbsolutePathBuf::resolve_path_against_base(std::env::temp_dir(), "/")
                .join(HOOK_OUTPUTS_DIR)
                .join(session_spill_directory(&thread_id)),
        }
    }

    /// Keeps hook text within the model-visible hook-output budget.
    ///
    /// Oversized text is written in full under the OS temp directory at
    /// `<temp_dir>/hook_outputs/<thread_id>/`
    /// and replaced with the same head/tail preview style used for other truncated
    /// output, plus a path back to the preserved full text.
    pub async fn maybe_spill_text(&self, text: String) -> String {
        self.maybe_spill_text_with_limit(text, AdditionalContextLimit::default())
            .await
    }

    async fn maybe_spill_text_with_limit(
        &self,
        text: String,
        limit: AdditionalContextLimit,
    ) -> String {
        let token_limit = limit.token_limit;
        if token_limit == 0 || approx_token_count(&text) <= token_limit {
            return text;
        }

        let path = self.output_dir.join(format!("{}.txt", Uuid::new_v4()));
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent.as_ref()).await
        {
            warn!(
                "failed to create hook output directory {}: {err}",
                parent.display()
            );
            return formatted_truncate_text(
                &text,
                TruncationPolicy::tokens(i64::try_from(token_limit).unwrap_or(i64::MAX)),
            );
        }

        if let Err(err) = fs::write(path.as_ref(), &text).await {
            warn!("failed to write hook output {}: {err}", path.display());
            return formatted_truncate_text(
                &text,
                TruncationPolicy::tokens(i64::try_from(token_limit).unwrap_or(i64::MAX)),
            );
        }

        spilled_hook_output_preview(&text, &path, token_limit)
    }

    pub async fn maybe_spill_additional_contexts(
        &self,
        contexts: Vec<AdditionalContext>,
    ) -> Vec<String> {
        let mut spilled = Vec::with_capacity(contexts.len());
        for context in contexts {
            spilled.push(
                self.maybe_spill_text_with_limit(context.text, context.limit)
                    .await,
            );
        }
        spilled
    }

    pub async fn maybe_spill_prompt_fragments(
        &self,
        fragments: Vec<HookPromptFragment>,
    ) -> Vec<HookPromptFragment> {
        let mut spilled = Vec::with_capacity(fragments.len());
        for fragment in fragments {
            spilled.push(HookPromptFragment {
                text: self.maybe_spill_text(fragment.text).await,
                hook_run_id: fragment.hook_run_id,
            });
        }
        spilled
    }
}

/// Builds the model-visible replacement for a spilled hook output.
///
/// The path footer is budgeted before truncation so adding the recovery path
/// does not consume the configured preview budget.
fn spilled_hook_output_preview(text: &str, path: &AbsolutePathBuf, token_limit: usize) -> String {
    let footer = format!("\n\nFull hook output saved to: {}", path.display());
    let preview_policy = TruncationPolicy::tokens(
        i64::try_from(token_limit.saturating_sub(approx_token_count(&footer))).unwrap_or(i64::MAX),
    );
    format!("{}{footer}", formatted_truncate_text(text, preview_policy))
}

// Framework session identifiers are opaque strings, so use a stable single path segment.
fn session_spill_directory(session: &ThreadId) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(session.as_str().as_bytes()))
}
