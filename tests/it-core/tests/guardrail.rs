//! What a host's input and output guardrails promise before either of them runs.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::{Error, GuardrailStage, Recoverability, Result},
    finish::FinishReason,
    guardrail::{
        GuardrailFinalOutput, GuardrailFunctionOutput, InputGuardrail, InputGuardrailResult,
        OutputGuardrail, OutputGuardrailResult, merge_input_guardrails, merge_output_guardrails,
    },
    item::{Message, ModelInputItem, OutputPhase},
};
use rstest::rstest;
use serde_json::json;

fn id(value: &str) -> String {
    String::from(value)
}

/// Answers with whatever it was built with, and records nothing: these cases are about the values
/// the contract carries, not about running a check.
struct Fixed {
    id: String,
    output: GuardrailFunctionOutput,
}

impl Fixed {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            id: id(name),
            output: GuardrailFunctionOutput::pass(),
        })
    }
}

#[async_trait]
impl InputGuardrail for Fixed {
    fn name(&self) -> &str {
        &self.id
    }

    async fn check(
        &self,
        _context: &RunContext,
        _input: &[ModelInputItem],
    ) -> Result<GuardrailFunctionOutput> {
        Ok(self.output.clone())
    }
}

#[async_trait]
impl OutputGuardrail for Fixed {
    fn name(&self) -> &str {
        &self.id
    }

    async fn check(
        &self,
        _context: &RunContext,
        _output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        Ok(self.output.clone())
    }
}

#[test]
fn a_verdict_records_evidence_whether_or_not_it_stops_the_run() {
    // A pass that produced nothing is indistinguishable afterwards from a check that was never
    // installed, so evidence is available on both verdicts rather than only on a refusal.
    let passed = GuardrailFunctionOutput::pass().with_output_info(json!({"score": 0.02}));
    let tripped = GuardrailFunctionOutput::tripwire().with_output_info(json!({"score": 0.97}));

    assert!(!passed.tripwire_triggered());
    assert!(tripped.tripwire_triggered());
    assert_eq!(passed.output_info(), &json!({"score": 0.02}));
    assert_eq!(tripped.output_info(), &json!({"score": 0.97}));
    assert!(GuardrailFunctionOutput::pass().output_info().is_null());
    assert!(!GuardrailFunctionOutput::default().tripwire_triggered());
}

#[test]
fn a_verdict_survives_the_checkpoint_it_is_recorded_in() {
    let result = InputGuardrailResult::new(
        id("prompt_injection"),
        GuardrailFunctionOutput::pass().with_output_info(json!({"matched": []})),
    );

    let restored: InputGuardrailResult =
        serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
    assert_eq!(restored, result);

    let result = OutputGuardrailResult::new(
        id("pii"),
        GuardrailFunctionOutput::tripwire().with_output_info(json!({"kind": "email"})),
    );
    let restored: OutputGuardrailResult =
        serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
    assert_eq!(restored, result);
}

/// A stored verdict keeps whatever name it was written under, prose and punctuation included.
///
/// The name is a display string mirroring upstream's `get_name()`, so there is no grammar for a
/// checkpoint to re-validate against. An earlier version imposed one; rejecting a restored verdict
/// over its spelling would make a checkpoint unreadable for a reason its own writer accepted.
#[rstest]
#[case::prose("looks like a jailbreak")]
#[case::capitals("PromptInjection")]
#[case::hyphen("prompt-injection")]
fn a_stored_verdict_keeps_the_name_it_was_written_under(#[case] name: &str) {
    let stored = json!({"guardrail": name, "output": {"tripwire_triggered": false}});
    let restored: InputGuardrailResult =
        serde_json::from_value(stored).expect("any name a writer used is a name a reader accepts");
    assert_eq!(restored.guardrail(), name);
}

#[test]
fn a_passing_verdict_has_no_refusal_to_report() {
    let passed = InputGuardrailResult::new(id("prompt_injection"), GuardrailFunctionOutput::pass());
    assert!(passed.tripwire_error().is_none());
    assert!(!passed.tripwire_triggered());

    let passed = OutputGuardrailResult::new(id("pii"), GuardrailFunctionOutput::pass());
    assert!(passed.tripwire_error().is_none());
}

/// The stage and the identity are built beside the guardrail that tripped, never supplied by
/// whoever reports the block: `code()` is what routes every report and metric, and a caller that
/// named the wrong stage would send them to the other end of the run.
#[test]
fn a_tripped_input_verdict_carries_its_own_stage_and_identity() {
    let result = InputGuardrailResult::new(
        id("prompt_injection"),
        GuardrailFunctionOutput::tripwire().with_output_info(json!({"rule": "ignore_previous"})),
    );

    let error = result.tripwire_error().expect("a tripwire is a refusal");
    assert_eq!(error.code(), "guardrail.input");
    assert!(error.to_string().contains("prompt_injection"), "{error}");
    // The evidence travels with the refusal: the run ends with an error and no result, so this is
    // the only place a host can read what the guardrail saw.
    assert!(error.to_string().contains("ignore_previous"), "{error}");
    // A deliberate block, not a fault: nothing about it becomes true on a second attempt.
    assert_eq!(error.recoverability(), Recoverability::Fatal);
    assert!(!error.is_retryable());
    assert!(!error.is_cancelled());
}

#[test]
fn a_tripped_output_verdict_carries_its_own_stage_and_identity() {
    let result = OutputGuardrailResult::new(id("pii"), GuardrailFunctionOutput::tripwire());

    let error = result.tripwire_error().expect("a tripwire is a refusal");
    assert_eq!(error.code(), "guardrail.output");
    assert!(error.to_string().contains("pii"), "{error}");
    assert_eq!(error.recoverability(), Recoverability::Fatal);
}

#[test]
fn the_four_stages_carry_distinct_code_words() {
    let stages = [
        GuardrailStage::Input,
        GuardrailStage::Output,
        GuardrailStage::ToolInput,
        GuardrailStage::ToolOutput,
    ];
    let codes: Vec<&str> = stages.iter().map(|stage| stage.code()).collect();
    assert_eq!(codes, ["input", "output", "tool_input", "tool_output"]);
    for stage in stages {
        // The bare stage names a trace field's value; `Error::code` names the error family. The
        // second is built from the first, and neither may borrow the other's spelling.
        assert!(!stage.code().contains('.'), "{}", stage.code());
        assert_eq!(
            Error::guardrail(stage, "x", "y").code(),
            format!("guardrail.{}", stage.code())
        );
    }
}

#[test]
fn a_delivery_with_no_message_still_says_how_the_run_ended() {
    // A run promoted a tool result to its answer, so there is no assistant message to hand over.
    // The guardrail is still asked, and the finish reason is what lets it tell that case from a
    // model that said nothing.
    let promoted = GuardrailFinalOutput::new(FinishReason::ToolStop);
    assert!(promoted.message().is_none());
    assert!(promoted.text().is_empty());
    assert_eq!(promoted.finish_reason(), FinishReason::ToolStop);

    let message = Message::assistant("done", OutputPhase::Final);
    let delivered = GuardrailFinalOutput::new(FinishReason::Final).with_message(&message);
    assert_eq!(delivered.text(), "done");
    assert_eq!(delivered.message(), Some(&message));
    assert_eq!(delivered.finish_reason(), FinishReason::Final);
}

#[test]
fn a_run_adds_its_own_guardrails_after_the_agent_declares_its_own() {
    let agent: Vec<Arc<dyn InputGuardrail>> = vec![Fixed::new("prompt_injection")];
    let run: Vec<Arc<dyn InputGuardrail>> = vec![Fixed::new("pii"), Fixed::new("topic")];

    let merged = merge_input_guardrails(&agent, &run);
    let names: Vec<&str> = merged.iter().map(|guardrail| guardrail.name()).collect();
    // Declaration order, and the agent's first: the verdicts are recorded in this order, and a
    // reader lining up two runs of the same agent needs it to be the same order both times.
    assert_eq!(names, ["prompt_injection", "pii", "topic"]);
}

/// Two checks under one name both survive the merge, and both will run.
///
/// Upstream concatenates the two lists without inspecting names, and so does this. The name is a
/// display string: refusing a configuration over one would reject a run upstream accepts, and the
/// ambiguity it was meant to prevent is a reason to record more, not to run less.
#[rstest]
#[case::one_from_each_source(1, 1)]
#[case::both_on_the_agent(2, 0)]
#[case::both_on_the_run(0, 2)]
fn one_name_may_stand_for_two_checks(#[case] on_agent: usize, #[case] on_run: usize) {
    let agent: Vec<Arc<dyn InputGuardrail>> = (0..on_agent)
        .map(|_| Fixed::new("pii") as Arc<dyn InputGuardrail>)
        .collect();
    let run: Vec<Arc<dyn InputGuardrail>> = (0..on_run)
        .map(|_| Fixed::new("pii") as Arc<dyn InputGuardrail>)
        .collect();

    let merged = merge_input_guardrails(&agent, &run);
    assert_eq!(
        merged.len(),
        2,
        "both are kept: a name is not an identity, and dropping one would silently skip a check \
         the host installed"
    );

    let agent: Vec<Arc<dyn OutputGuardrail>> = vec![Fixed::new("pii")];
    let run: Vec<Arc<dyn OutputGuardrail>> = vec![Fixed::new("pii")];
    assert_eq!(merge_output_guardrails(&agent, &run).len(), 2);
}

/// An agent may declare two checks under one name, and both are kept.
///
/// The same rule the merge follows, applied where an agent is built: a name is not an identity, so
/// there is nothing here to collide. Refusing would reject a declaration upstream accepts.
#[test]
fn an_agent_may_declare_two_guardrails_under_one_name() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .input_guardrail(Fixed::new("pii"))
        .input_guardrail(Fixed::new("pii"))
        .output_guardrail(Fixed::new("pii"))
        .output_guardrail(Fixed::new("pii"))
        .build()
        .expect("two checks sharing a name is a declaration, not an error");

    assert_eq!(agent.input_guardrails().len(), 2);
    assert_eq!(agent.output_guardrails().len(), 2);
}

/// Both stages are separate lists, so one name may be an input check and an output check at once:
/// a firing is attributed by stage *and* identity, and those two are different checks with
/// different inputs.
#[test]
fn the_two_stages_are_separate_namespaces() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .input_guardrail(Fixed::new("pii"))
        .output_guardrail(Fixed::new("pii"))
        .build()
        .expect("one name may check both ends");

    assert_eq!(agent.input_guardrails().len(), 1);
    assert_eq!(agent.output_guardrails().len(), 1);
}

#[test]
fn a_derived_agent_carries_the_guardrails_it_was_derived_from() {
    let agent = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("Coder")
        .input_guardrail(Fixed::new("prompt_injection"))
        .output_guardrail(Fixed::new("pii"))
        .build()
        .unwrap();

    let derived = agent.to_builder().name("Reviewer").build().unwrap();
    assert_eq!(derived.input_guardrails()[0].name(), "prompt_injection");
    assert_eq!(derived.output_guardrails()[0].name(), "pii");

    // Dropping them is explicit, for the reason clearing tools or instructions is: a variant that
    // silently kept a check the caller meant to replace is the more dangerous default.
    let stripped = agent
        .to_builder()
        .clear_input_guardrails()
        .clear_output_guardrails()
        .build()
        .unwrap();
    assert!(stripped.input_guardrails().is_empty());
    assert!(stripped.output_guardrails().is_empty());
}
