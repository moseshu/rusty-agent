//! Codex contextual fragments stay model-visible without becoming genuine user messages.

use ra_core::{
    item::{ContentBlock, Message, MessageRole},
    session::contextual::{is_contextual_user_message, is_skill_prompt_fragment},
};

#[test]
fn skill_fragment_matching_preserves_upstream_marker_boundaries() {
    for text in [
        "<skill>\n<name>demo</name>\n</skill>",
        " \n<SKILL>body</SKILL>\t ",
        "<skill></skill>",
    ] {
        assert!(is_skill_prompt_fragment(text), "{text:?}");
    }
    for text in [
        "",
        "💡💡",
        "<skill>",
        "</skill>",
        "prefix <skill>body</skill>",
        "<skill>body</skill> suffix",
        "<skillful>body</skill>",
    ] {
        assert!(!is_skill_prompt_fragment(text), "{text:?}");
    }
}

#[test]
fn any_matching_block_hides_a_user_message_but_other_roles_keep_their_identity() {
    for role in [
        MessageRole::User,
        MessageRole::Assistant,
        MessageRole::System,
    ] {
        let message = Message::new(
            role,
            vec![
                ContentBlock::text("other text"),
                ContentBlock::text(" <SKILL>instructions</SKILL> "),
            ],
        );
        assert_eq!(
            is_contextual_user_message(&message),
            role == MessageRole::User
        );
    }
    assert!(!is_contextual_user_message(&Message::user(
        "Mention <skill> without a complete fragment"
    )));
}
