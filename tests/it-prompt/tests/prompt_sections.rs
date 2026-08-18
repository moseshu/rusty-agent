use ra_core::prompt::{
    ContentHash, PromptSection, PromptSectionName, PromptSource, SectionPosition, SectionStability,
};
use ra_prompt::section::{PromptSectionBuilder, compute_content_hash, estimate_tokens};

#[test]
fn test_prompt_section_construction_and_accessors() {
    let section = PromptSectionBuilder::new(PromptSectionName::CORE_BEHAVIOR)
        .purpose("Defines base behavioral constitution")
        .source(PromptSource::Builtin)
        .stable()
        .prefix()
        .content("You are an autonomous engineering assistant.")
        .build()
        .expect("valid stable prefix section");

    assert_eq!(section.name(), &PromptSectionName::CORE_BEHAVIOR);
    assert_eq!(section.purpose(), "Defines base behavioral constitution");
    assert_eq!(section.source(), &PromptSource::Builtin);
    assert!(section.stability().is_stable());
    assert!(section.position().is_prefix());
    assert_eq!(
        section.content(),
        "You are an autonomous engineering assistant."
    );
    assert!(section.token_estimate() > 0);
    assert_eq!(
        section.content_hash().as_str(),
        compute_content_hash("You are an autonomous engineering assistant.").as_str()
    );
}

#[test]
fn test_volatile_prefix_is_strictly_prohibited() {
    let result = PromptSectionBuilder::new(PromptSectionName::new("volatile_test"))
        .purpose("Volatile section mistakenly in prefix")
        .source(PromptSource::Builtin)
        .volatile()
        .prefix()
        .content("Volatile instructions that would destroy cache hit rate")
        .build();

    assert!(
        result.is_err(),
        "volatile section must not be allowed in the prefix position"
    );

    let direct_result = PromptSection::new(
        PromptSectionName::new("volatile_direct"),
        "Direct constructor test",
        PromptSource::Builtin,
        SectionStability::Volatile,
        SectionPosition::Prefix,
        "Direct volatile content",
    );

    assert!(
        direct_result.is_err(),
        "direct constructor must also reject volatile prefix"
    );
}

#[test]
fn test_volatile_tail_message_is_permitted() {
    let section = PromptSectionBuilder::new(PromptSectionName::new("volatile_tail"))
        .purpose("Runtime tail attachment")
        .source(PromptSource::Dynamic("runtime".into()))
        .volatile()
        .tail_message()
        .content("Current state delta update")
        .build()
        .expect("volatile tail message must succeed");

    assert!(section.stability().is_volatile());
    assert!(section.position().is_tail_message());
}

#[test]
fn test_content_hash_deterministic_computation() {
    let text = "Exact byte-for-byte content to test sha256";
    let hash1 = ContentHash::compute(text);
    let hash2 = ContentHash::compute(text);
    assert_eq!(hash1, hash2);
    assert_eq!(hash1.as_str().len(), 64);

    let parsed = ContentHash::from_hex(hash1.as_str()).expect("valid 64 hex string");
    assert_eq!(parsed, hash1);

    let invalid = ContentHash::from_hex("invalid-hash");
    assert!(invalid.is_err());
}

#[test]
fn test_token_estimation_heuristic() {
    assert_eq!(estimate_tokens(""), 0);
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("abcdefgh"), 2);
}
