//! Phase two: consolidating the selected raw memories into the memories directory.
//!
//! A port of the reference's `sandbox/memory/phase_two.py`.

use ra_core::{error::Result, sandbox::MemoryGenerateConfig};

use super::agents::{MemoryRunConfig, PhaseAgent};
use super::prompts::render_memory_consolidation_prompt;
use super::storage::PhaseTwoInputSelection;

/// The most turns a consolidation may take, as on the reference.
pub const PHASE_TWO_MAX_TURNS: u32 = 500;

/// The consolidation agent's name.
pub const PHASE_TWO_AGENT_NAME: &str = "sandbox-memory-phase-two";

/// Runs the consolidation agent over `selection`, rooted at `memory_root`.
///
/// The agent has no instructions of its own beyond the sandbox's; the consolidation prompt is its
/// input, and it edits the memory files with the tools its capabilities give it.
///
/// Succeeds only when the run concluded on its own. A consolidation that stopped at its turn cap,
/// for approval, on a cancellation or an exhausted budget is a failure, as the reference's raised
/// `MaxTurnsExceeded` is, so the manager leaves the last successful selection as it was.
///
/// # Errors
///
/// Returns the consolidation run's failure, and a configuration error naming how it stopped when
/// it did not conclude.
pub async fn run_phase_two(
    config: &MemoryGenerateConfig,
    memory_root: &str,
    selection: &PhaseTwoInputSelection,
    run: &MemoryRunConfig,
) -> Result<()> {
    let prompt = render_memory_consolidation_prompt(memory_root, selection, config.extra_prompt());
    // The run's own result is not read, but it has been checked: `run` refuses one that did not
    // conclude.
    run.run(
        PhaseAgent {
            name: PHASE_TWO_AGENT_NAME,
            instructions: None,
            model: config.phase_two_model(),
            model_settings: config.phase_two_model_settings(),
            output_schema: None,
            max_turns: Some(PHASE_TWO_MAX_TURNS),
        },
        prompt,
    )
    .await?;
    Ok(())
}
