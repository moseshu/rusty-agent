use ra_core::agent::{AgentId, AgentInstructions, AgentSpec, ResolvedInstructions};
use ra_core::context::RunContext;
use ra_core::prompt::{
    CachePlan, ContentHash, MIN_CACHEABLE_PREFIX_TOKENS, PromptSection, PromptSectionName,
    PromptSource, ResolvedPrompt, SectionPosition, SectionStability, estimate_tokens,
};
use ra_core::state::RunId;

/// Static instructions resolve to the prefix placement, and cannot be mistaken for tail content.
///
/// Resolution used to hand back a bare [`ResolvedPrompt`] for both sources. That type lowers to
/// volatile tail messages, so resolving a static agent and lowering the result put the entire
/// system constitution into a user message — no error, and nothing in the value recording that it
/// had ever been prefix text. The placement now travels with the value.
#[tokio::test]
async fn test_static_instructions_resolve_to_the_prefix_placement() {
    let instructions = AgentInstructions::static_text("You are a helpful assistant.");
    assert_eq!(
        instructions.as_static(),
        Some("You are a helpful assistant.")
    );
    assert!(!instructions.is_dynamic());

    let spec = AgentSpec::builder()
        .id(AgentId::new("agent-1"))
        .name("Agent 1")
        .build()
        .expect("spec");
    let context = RunContext::new(RunId::new("run-1"), &spec);
    let resolved = instructions
        .resolve(&context)
        .await
        .expect("resolve static");

    match resolved {
        ResolvedInstructions::Prefix(text) => {
            assert_eq!(text, "You are a helpful assistant.");
        }
        ResolvedInstructions::Generated(prompt) => panic!(
            "static instructions must not resolve to tail-bound generator output: {prompt:?}"
        ),
    }
}

#[tokio::test]
async fn test_agent_instructions_dynamic_resolution() {
    let instructions = AgentInstructions::dynamic_fn(|ctx| {
        let run_id = ctx.run_id().as_str().to_string();
        async move {
            Ok(ResolvedPrompt::new(
                format!("Dynamic prompt for run {run_id}"),
                PromptSource::Dynamic("session_hook".into()),
            ))
        }
    });

    assert_eq!(instructions.as_static(), None);
    assert!(instructions.is_dynamic());

    let spec = AgentSpec::builder()
        .id(AgentId::new("agent-1"))
        .name("Agent 1")
        .build()
        .expect("spec");
    let context = RunContext::new(RunId::new("run-dynamic-99"), &spec);
    let resolved = instructions
        .resolve(&context)
        .await
        .expect("resolve dynamic");

    let ResolvedInstructions::Generated(prompt) = resolved else {
        panic!("a dynamic source must resolve to generator output");
    };
    assert_eq!(prompt.text(), "Dynamic prompt for run run-dynamic-99");
    assert_eq!(
        prompt.source(),
        &PromptSource::Dynamic("session_hook".into())
    );
}

#[test]
fn test_agent_instructions_debug_masking() {
    let static_inst = AgentInstructions::static_text("Sensitive secret prompt content");
    let static_debug = format!("{static_inst:?}");
    assert!(
        !static_debug.contains("Sensitive secret prompt content"),
        "raw prompt content must be masked in Debug"
    );
    assert!(static_debug.contains("static"));

    let dynamic_inst = AgentInstructions::dynamic_fn(|_| async {
        Ok(ResolvedPrompt::new("test", PromptSource::Builtin))
    });
    let dynamic_debug = format!("{dynamic_inst:?}");
    assert!(dynamic_debug.contains("dynamic"));
}

#[tokio::test]
async fn test_agent_spec_builder_dynamic_instructions() {
    let spec = AgentSpec::builder()
        .id(AgentId::new("dynamic-agent"))
        .name("Dynamic Agent")
        .dynamic_instructions_fn(|ctx| {
            let id = ctx.agent().id().as_str().to_string();
            async move {
                Ok(ResolvedPrompt::new(
                    format!("Instructions for agent {id}"),
                    PromptSource::Agent,
                ))
            }
        })
        .build()
        .expect("build dynamic spec");

    let context = RunContext::new(RunId::new("run-1"), &spec);
    let resolved = spec
        .instructions()
        .expect("has instructions")
        .resolve(&context)
        .await
        .expect("resolved");

    let ResolvedInstructions::Generated(prompt) = resolved else {
        panic!("a dynamic source must resolve to generator output");
    };
    assert_eq!(prompt.text(), "Instructions for agent dynamic-agent");
}

/// The volatile-cannot-be-prefix rule survives a round trip through serde.
///
/// The rule is only worth as much as the narrowest path into the type. A derived `Deserialize` is
/// a second constructor that skips the check, and prompt sections are persisted and replayed —
/// so that path is the one an invalid section would actually arrive through.
#[test]
fn test_deserialize_rejects_a_volatile_prefix_section() {
    let json = r#"{
        "name": "sneaky",
        "purpose": "smuggled in through the wire format",
        "source": {"type": "Builtin"},
        "stability": "volatile",
        "position": "prefix",
        "content_hash": "0000000000000000000000000000000000000000000000000000000000000000",
        "token_estimate": 0,
        "content": "per-turn text pretending to be cacheable"
    }"#;

    let err = serde_json::from_str::<PromptSection>(json)
        .expect_err("a volatile prefix section must not deserialize");
    assert!(
        err.to_string().contains("volatile"),
        "the error must explain the violated invariant: {err}"
    );
}

/// A stored hash that disagrees with the stored content is rejected rather than quietly refreshed.
///
/// Every cache decision is keyed on this hash. Recomputing it on load would make the two agree
/// again by overwriting the evidence that they had diverged.
#[test]
fn test_deserialize_rejects_a_hash_that_does_not_match_its_content() {
    let json = r#"{
        "name": "core_behavior",
        "purpose": "core",
        "source": {"type": "Builtin"},
        "stability": "stable",
        "position": "prefix",
        "content_hash": "0000000000000000000000000000000000000000000000000000000000000000",
        "token_estimate": 4,
        "content": "some content"
    }"#;

    let err = serde_json::from_str::<PromptSection>(json)
        .expect_err("a mismatched content hash must not deserialize");
    assert!(
        err.to_string().contains("content hash"),
        "the error must point at the hash: {err}"
    );
}

/// A valid section round trips unchanged, including an externally supplied token count.
#[test]
fn test_prompt_section_round_trips_with_an_external_token_estimate() {
    let section = PromptSection::new(
        PromptSectionName::CORE_BEHAVIOR,
        "core",
        PromptSource::Builtin,
        SectionStability::Stable,
        SectionPosition::Prefix,
        "You are an autonomous engineering assistant.",
    )
    .expect("valid section")
    .with_token_estimate(9_999)
    .with_token_budget(12_000);

    let json = serde_json::to_string(&section).expect("serialize");
    let restored: PromptSection = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored, section);
    assert_eq!(
        restored.token_estimate(),
        9_999,
        "an exact count from a real tokenizer must not be recomputed away"
    );
    assert_eq!(
        restored.token_budget(),
        Some(12_000),
        "the declared allowance travels with the section that declared it"
    );
}

/// A section recorded before allowances existed loads, and reports that it declares none.
///
/// Every other field here is required, because a section missing one is a section that never
/// validated. An allowance is different in kind: an older writer could not have carried it, and
/// `None` is the honest reading of its absence rather than a value invented on the way in. The
/// same default is what keeps `prompt dump --baseline` able to read a dump recorded last month.
#[test]
fn test_a_section_recorded_without_an_allowance_declares_none() {
    let section = PromptSection::new(
        PromptSectionName::CORE_BEHAVIOR,
        "core",
        PromptSource::Builtin,
        SectionStability::Stable,
        SectionPosition::Prefix,
        "You are an autonomous engineering assistant.",
    )
    .expect("valid section");

    let mut wire: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&section).expect("serialize"))
            .expect("a section is JSON");
    assert!(
        wire.as_object_mut()
            .expect("a section is a JSON object")
            .remove("token_budget")
            .is_some(),
        "the field has to be present for its removal to be testing anything"
    );

    let restored: PromptSection =
        serde_json::from_str(&wire.to_string()).expect("an older section must still load");
    assert_eq!(restored.token_budget(), None);
}

/// A generated prompt may only occupy volatile tail messages.
#[test]
fn test_resolved_prompt_lowering_is_tail_only() {
    let tail = PromptSection::new(
        PromptSectionName::new("delta"),
        "delta",
        PromptSource::Dynamic("gen".into()),
        SectionStability::Volatile,
        SectionPosition::TailMessage,
        "delta text",
    )
    .expect("tail section");
    let prefix = PromptSection::new(
        PromptSectionName::CORE_BEHAVIOR,
        "core",
        PromptSource::Agent,
        SectionStability::Stable,
        SectionPosition::Prefix,
        "prefix text",
    )
    .expect("prefix section");

    let tail_only =
        ResolvedPrompt::new("full", PromptSource::Agent).with_sections(vec![tail.clone()]);
    assert_eq!(
        tail_only
            .lower_to_tail_items()
            .expect("tail-only lowers")
            .len(),
        1
    );

    let with_prefix = ResolvedPrompt::new("full", PromptSource::Agent).with_sections(vec![prefix]);
    with_prefix
        .lower_to_tail_items()
        .expect_err("a prefix-bound section must be rejected, not dropped");
}

/// The record and the items come out of one derivation, so they cannot describe different text.
#[test]
fn test_lowering_and_its_record_are_produced_together() {
    let resolved = ResolvedPrompt::new("generated text", PromptSource::Dynamic("hook".into()));

    let (items, record) = resolved.lower().expect("valid dynamic prompt");
    assert_eq!(items.len(), 1);
    assert_eq!(
        record.content_hash(),
        &ContentHash::compute("generated text")
    );

    // The two single-purpose entries stay consistent with the combined one; they are the same
    // computation, not a second opinion about it.
    assert_eq!(
        resolved.lower_to_tail_items().expect("items").len(),
        items.len()
    );
    assert_eq!(
        resolved.provenance_record().expect("record").content_hash(),
        record.content_hash()
    );
}

/// The provenance record carries source, hash, version, and provenance for the text actually sent.
#[test]
fn test_provenance_record_captures_source_hash_version_and_provenance() {
    let resolved = ResolvedPrompt::new("generated text", PromptSource::Dynamic("hook".into()))
        .with_version("v3")
        .with_provenance("session_bootstrap");

    let record = resolved.provenance_record().expect("valid dynamic prompt");
    assert_eq!(record.source(), &PromptSource::Dynamic("hook".into()));
    assert_eq!(
        record.content_hash(),
        &ContentHash::compute("generated text")
    );
    assert_eq!(record.version(), Some("v3"));
    assert_eq!(record.provenance(), Some("session_bootstrap"));
}

/// Structured prompts must record the text that lowering sends, not an unused full-text field.
#[test]
fn test_structured_prompt_provenance_hashes_lowered_tail_text() {
    let first = PromptSection::new(
        PromptSectionName::new("first"),
        "first delta",
        PromptSource::Dynamic("hook".into()),
        SectionStability::Volatile,
        SectionPosition::TailMessage,
        "first update",
    )
    .expect("valid section");
    let second = PromptSection::new(
        PromptSectionName::new("second"),
        "second delta",
        PromptSource::Dynamic("hook".into()),
        SectionStability::Volatile,
        SectionPosition::TailMessage,
        "second update",
    )
    .expect("valid section");
    let resolved = ResolvedPrompt::new("unused full representation", PromptSource::Agent)
        .with_sections(vec![first, second]);

    let record = resolved
        .provenance_record()
        .expect("valid structured prompt");
    assert_eq!(
        record.content_hash(),
        &ContentHash::compute("first update\n\nsecond update")
    );
    assert_ne!(record.content_hash(), resolved.content_hash());
}

/// The plan states intent only: which bytes are stable, and which calls should share an entry.
///
/// It used to be derived from `ApiProtocol` and to carry a vendor strategy, breakpoints whose
/// `is_ephemeral` is Anthropic's `cache_control` shape, and OpenAI's `prompt_cache_key`. It also
/// used to decide, from the instructions alone, whether caching was worthwhile — a judgement the
/// cached prefix does not support, since the tool table shares that prefix and hosted tools do not
/// exist until an adapter has merged them.
#[test]
fn test_cache_plan_carries_intent_rather_than_provider_wire_form() {
    let prefix = cacheable_prefix();

    let plan = CachePlan::for_prefix(&prefix, Some("run-7"));
    assert_eq!(plan.prefix_hash(), &ContentHash::compute(&prefix));
    assert_eq!(plan.cache_scope(), Some("run-7"));
}

/// Short instructions still get a plan, because this layer cannot see the rest of the prefix.
///
/// Refusing one here would deny caching to a request whose real cached span — a modest instruction
/// block in front of a dozen tool schemas — runs well past the floor. The verdict belongs to the
/// adapter, which is the only place the whole wire prefix exists.
#[test]
fn test_short_instructions_still_receive_a_plan() {
    let short = "You are a helpful assistant.";
    assert!(
        estimate_tokens(short) < MIN_CACHEABLE_PREFIX_TOKENS,
        "the fixture must be below the floor for this test to mean anything"
    );

    let plan = CachePlan::for_prefix(short, Some("run-7"));
    assert_eq!(plan.prefix_hash(), &ContentHash::compute(short));
}

/// The cache scope is optional; omitting it still yields a plan naming the prefix.
#[test]
fn test_cache_plan_without_a_scope_still_names_the_prefix() {
    let prefix = cacheable_prefix();
    let plan = CachePlan::for_prefix(&prefix, None);
    assert_eq!(plan.cache_scope(), None);
    assert_eq!(plan.prefix_hash(), &ContentHash::compute(&prefix));
}

fn cacheable_prefix() -> String {
    "You are an autonomous engineering assistant. ".repeat(150)
}
