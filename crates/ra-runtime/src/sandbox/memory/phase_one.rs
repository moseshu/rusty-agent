//! Phase one: extracting one rollout into a raw memory and a rollout summary.
//!
//! A port of the reference's `sandbox/memory/phase_one.py` and `sandbox/memory/interface.py`: the
//! structured output the extraction model returns, the prompt it is given, the checks on what it
//! returns, and the extraction run itself.

use ra_core::{
    error::{Error, Result},
    output::OutputSchema,
    sandbox::{
        MemoryGenerateConfig,
        token_truncation::{TruncationPolicy, truncate_text},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::agents::{MemoryRunConfig, PhaseAgent};
use super::json::{dumps_indented_tight, python_repr, python_strip};
use super::prompts::{render_rollout_extraction_prompt, render_rollout_extraction_user_prompt};

/// The extraction agent's name.
pub const PHASE_ONE_AGENT_NAME: &str = "sandbox-memory-phase-one";

/// The turn cap an extraction runs under: the reference leaves it at its runner's default.
pub const PHASE_ONE_MAX_TURNS: u32 = 10;

/// The most tokens of a rollout phase one's prompt carries.
pub const PHASE_ONE_ROLLOUT_TOKEN_LIMIT: i64 = 150_000;

/// The name phase one's structured output is requested under.
pub const ROLLOUT_EXTRACTION_ARTIFACTS_NAME: &str = "sandbox_memory_rollout_extraction_artifacts";

/// What phase one returns for one rollout.
///
/// The reference's `RolloutExtractionArtifacts`. All three empty means there was nothing worth
/// remembering; see [`validate_rollout_artifacts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutExtractionArtifacts {
    rollout_slug: String,
    rollout_summary: String,
    raw_memory: String,
}

impl RolloutExtractionArtifacts {
    /// What one extraction returned.
    #[must_use]
    pub fn new(
        rollout_slug: impl Into<String>,
        rollout_summary: impl Into<String>,
        raw_memory: impl Into<String>,
    ) -> Self {
        Self {
            rollout_slug: rollout_slug.into(),
            rollout_summary: rollout_summary.into(),
            raw_memory: raw_memory.into(),
        }
    }

    /// A short file-name-safe label for the rollout.
    #[must_use]
    pub fn rollout_slug(&self) -> &str {
        &self.rollout_slug
    }

    /// The rollout's summary.
    #[must_use]
    pub fn rollout_summary(&self) -> &str {
        &self.rollout_summary
    }

    /// The raw memory extracted from the rollout.
    #[must_use]
    pub fn raw_memory(&self) -> &str {
        &self.raw_memory
    }
}

/// The reference's `ROLLOUT_EXTRACTION_ARTIFACTS_JSON_SCHEMA`, strict: three required strings and
/// nothing else.
#[must_use]
pub fn rollout_extraction_artifacts_json_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "rollout_slug": {"type": "string"},
            "rollout_summary": {"type": "string"},
            "raw_memory": {"type": "string"},
        },
        "required": ["rollout_slug", "rollout_summary", "raw_memory"],
    })
}

/// A rollout slug with surrounding whitespace and a trailing `.md` removed, having checked that it
/// is a lowercase letter or digit followed by up to 79 lowercase letters, digits, `_` or `-`.
///
/// # Errors
///
/// Returns a configuration error, worded as the reference's, for anything else.
pub fn normalize_rollout_slug(value: &str) -> Result<String> {
    let trimmed = python_strip(value);
    let slug = trimmed.strip_suffix(".md").unwrap_or(trimmed);
    let mut characters = slug.chars();
    let valid = characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && slug.len() <= 80
        && characters
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if !valid {
        return Err(Error::config(format!(
            "Invalid rollout_slug: {}",
            python_repr(value)
        )));
    }
    Ok(slug.to_owned())
}

/// The rollout id a rollout file is named for: its file name without the extension.
///
/// # Errors
///
/// Returns a configuration error, worded as the reference's, when that is not a file-safe id.
pub fn rollout_id_from_rollout_path(value: &str) -> Result<String> {
    let name = value.rsplit('/').next().unwrap_or(value);
    let name = python_strip(name);
    let stem = match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => &name[..index],
        _ => name,
    };
    let mut characters = stem.chars();
    let valid = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && stem.len() <= 128
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Err(Error::config(format!(
            "Invalid rollout id for memory: {}",
            python_repr(value)
        )));
    }
    Ok(stem.to_owned())
}

/// The segments of a rollout file, one per non-blank line.
///
/// Lines are split on `\n`, which is the only line break a rollout file holds: every writer, the
/// reference's included, escapes the others inside strings.
///
/// # Errors
///
/// Returns a configuration error for a line that is not JSON.
pub fn parse_rollout_segments(rollout_contents: &str) -> Result<Vec<Value>> {
    rollout_contents
        .lines()
        .filter(|line| !python_strip(line).is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| Error::config(format!("invalid rollout JSONL record: {error}")))
        })
        .collect()
}

fn terminal_metadata_of(segment: &Value) -> Value {
    segment
        .get("terminal_metadata")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// Phase one's input for a rollout: how it ended, and its segments, truncated to
/// [`PHASE_ONE_ROLLOUT_TOKEN_LIMIT`] tokens with a notice saying so when they do not fit.
///
/// A rollout of one segment reports that segment's terminal metadata; one of several reports how
/// many there were, the last one's metadata, and each one's terminal state.
///
/// # Errors
///
/// Returns a configuration error for a line that is not JSON, and, worded as the reference's, for
/// a rollout with no segments.
pub fn render_phase_one_prompt(rollout_contents: &str) -> Result<String> {
    let segments = parse_rollout_segments(rollout_contents)?;
    let Some(last) = segments.last() else {
        return Err(Error::config(
            "rollout_contents must contain at least one JSONL record",
        ));
    };
    let terminal_metadata = if segments.len() == 1 {
        terminal_metadata_of(last)
    } else {
        json!({
            "segment_count": segments.len(),
            "final_terminal_metadata": terminal_metadata_of(last),
            "terminal_states": segments
                .iter()
                .filter(|segment| segment.is_object())
                .map(|segment| {
                    terminal_metadata_of(segment)
                        .get("terminal_state")
                        .cloned()
                        .unwrap_or_else(|| Value::from("unknown"))
                })
                .collect::<Vec<_>>(),
        })
    };
    let terminal_metadata_json = dumps_indented_tight(&terminal_metadata).map_err(|error| {
        Error::config("failed to serialize terminal metadata").with_source(error)
    })?;

    let mut rendered = truncate_text(
        rollout_contents,
        TruncationPolicy::tokens(PHASE_ONE_ROLLOUT_TOKEN_LIMIT),
    );
    if rendered != rollout_contents {
        let marker = format!(
            "\n\n[rollout content omitted: this phase-one memory prompt contains a truncated view \
             of the saved rollout. original_chars={}; rendered_chars={}. Do not assume the \
             rendered rollout below is complete.]\n\n",
            rollout_contents.chars().count(),
            rendered.chars().count()
        );
        rendered = marker + &rendered;
    }
    Ok(render_rollout_extraction_user_prompt(
        &terminal_metadata_json,
        &rendered,
    ))
}

/// Whether phase one's artifacts are worth writing: `false` when all three are blank, which is
/// the extraction declining.
///
/// # Errors
///
/// Returns a configuration error, worded as the reference's, when only some are blank.
pub fn validate_rollout_artifacts(artifacts: &RolloutExtractionArtifacts) -> Result<bool> {
    let blank = [
        artifacts.rollout_slug(),
        artifacts.rollout_summary(),
        artifacts.raw_memory(),
    ]
    .map(|text| python_strip(text).is_empty());
    if blank.iter().all(|blank| *blank) {
        return Ok(false);
    }
    if blank.iter().any(|blank| *blank) {
        return Err(Error::config(
            "Phase 1 returned partially-empty memory artifacts.",
        ));
    }
    Ok(true)
}

/// Runs the extraction agent on `prompt` and reads its structured output.
///
/// The reference's `run_phase_one`: the agent's instructions are the extraction prompt with the
/// developer's guidance, and its output is [`RolloutExtractionArtifacts`] under the strict schema.
/// Only a run that concluded on its own is read; an error handler's closeout is not the agent's
/// answer.
///
/// # Errors
///
/// Returns the extraction run's failure, a configuration error naming how it stopped when it did
/// not conclude, and one when it delivered no answer or one that is not the artifacts.
pub async fn run_phase_one(
    config: &MemoryGenerateConfig,
    prompt: String,
    run: &MemoryRunConfig,
) -> Result<RolloutExtractionArtifacts> {
    let result = run
        .run(
            PhaseAgent {
                name: PHASE_ONE_AGENT_NAME,
                instructions: Some(render_rollout_extraction_prompt(config.extra_prompt())),
                model: config.phase_one_model(),
                model_settings: config.phase_one_model_settings(),
                output_schema: Some(OutputSchema::json_schema(
                    ROLLOUT_EXTRACTION_ARTIFACTS_NAME,
                    rollout_extraction_artifacts_json_schema(),
                )),
                max_turns: Some(PHASE_ONE_MAX_TURNS),
            },
            prompt,
        )
        .await?;
    let Some(message) = result.final_message() else {
        return Err(Error::config(
            "Phase 1 did not return rollout extraction artifacts.",
        ));
    };
    serde_json::from_str(&message.text_content()).map_err(|error| {
        Error::config("Phase 1 returned output that is not rollout extraction artifacts.")
            .with_source(error)
    })
}
