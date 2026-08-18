use async_trait::async_trait;
use ra_core::agent::{AgentId, AgentSpec};
use ra_core::context::RunContext;
use ra_core::error::Result;
use ra_core::item::ModelInputItem;
use ra_core::prompt::{DynamicPromptHandler, PromptSource, ResolvedPrompt};
use ra_core::prompt::{PromptSection, PromptSectionName, SectionPosition, SectionStability};
use ra_core::state::RunId;
use ra_prompt::dynamic::{
    resolve_dynamic_prompt, resolved_to_tail_input_items, resolved_to_volatile_sections,
};

struct TestDynamicPrompt {
    counter: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl DynamicPromptHandler for TestDynamicPrompt {
    async fn resolve(&self, context: &RunContext) -> Result<ResolvedPrompt> {
        let count = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let text = format!(
            "Dynamic prompt turn #{count} for run: {}",
            context.run_id().as_str()
        );
        Ok(
            ResolvedPrompt::new(text, PromptSource::Dynamic("test_counter".into()))
                .with_version("v1.0")
                .with_provenance("turn_counter"),
        )
    }
}

#[tokio::test]
async fn test_dynamic_prompt_resolution_and_lowering() {
    let handler = TestDynamicPrompt {
        counter: std::sync::atomic::AtomicUsize::new(1),
    };
    let spec = AgentSpec::builder()
        .id(AgentId::new("agent-1"))
        .name("Agent 1")
        .build()
        .expect("spec");
    let context = RunContext::new(RunId::new("test-run-42"), &spec);

    let resolved = resolve_dynamic_prompt(&handler, &context)
        .await
        .expect("dynamic resolve");

    assert_eq!(
        resolved.text(),
        "Dynamic prompt turn #1 for run: test-run-42"
    );
    assert_eq!(resolved.version(), Some("v1.0"));
    assert_eq!(resolved.provenance(), Some("turn_counter"));
    assert_eq!(
        resolved.source(),
        &PromptSource::Dynamic("test_counter".into())
    );

    let tail_items = resolved_to_tail_input_items(&resolved).expect("tail input items");
    assert_eq!(tail_items.len(), 1);
    assert!(matches!(tail_items[0], ModelInputItem::Message(_)));

    let volatile_sections =
        resolved_to_volatile_sections(&resolved).expect("volatile sections conversion");
    assert_eq!(volatile_sections.len(), 1);
    assert!(volatile_sections[0].stability().is_volatile());
    assert!(volatile_sections[0].position().is_tail_message());
}

/// The helper's name is a promise: it must not hand back a section bound for the stable prefix.
///
/// Returning the generator's sections verbatim would make this function agree with its caller only
/// by accident, and turn preparation applies the same rule from a crate that cannot call it.
#[test]
fn test_prefix_bound_sections_are_rejected_not_passed_through() {
    let prefix_section = PromptSection::new(
        PromptSectionName::CORE_BEHAVIOR,
        "Stable prefix constitution",
        PromptSource::Agent,
        SectionStability::Stable,
        SectionPosition::Prefix,
        "Stable constitution text",
    )
    .expect("prefix section");

    let resolved =
        ResolvedPrompt::new("full text", PromptSource::Agent).with_sections(vec![prefix_section]);

    let err = resolved_to_volatile_sections(&resolved).expect_err("prefix placement must fail");
    assert!(
        err.to_string().contains("core_behavior"),
        "the error must name the offending section: {err}"
    );

    resolved_to_tail_input_items(&resolved).expect_err("lowering must fail for the same reason");
}

/// A stable section aimed at the tail is rejected too: generated text is volatile by definition.
#[test]
fn test_stable_tail_sections_are_rejected() {
    let stable_tail = PromptSection::new(
        PromptSectionName::new("stable_tail"),
        "Stable text in the tail",
        PromptSource::Agent,
        SectionStability::Stable,
        SectionPosition::TailMessage,
        "Stable tail text",
    )
    .expect("stable tail section");

    let resolved =
        ResolvedPrompt::new("full text", PromptSource::Agent).with_sections(vec![stable_tail]);

    let err = resolved_to_volatile_sections(&resolved).expect_err("stable stability must fail");
    assert!(
        err.to_string().contains("stable_tail"),
        "the error must name the offending section: {err}"
    );
}

/// Empty generated text contributes nothing rather than an empty message.
#[test]
fn test_empty_dynamic_text_produces_no_items() {
    let resolved = ResolvedPrompt::new("   \n  ", PromptSource::Dynamic("noop".into()));
    assert!(
        resolved_to_volatile_sections(&resolved)
            .expect("empty text is not an error")
            .is_empty()
    );
    assert!(
        resolved_to_tail_input_items(&resolved)
            .expect("empty text is not an error")
            .is_empty()
    );
}
