//! A real skill contribution reaches the model and rollout without becoming a client user item.

use super::*;
use ra_tools::{
    capability::SkillsNsCapability,
    skills_ns::{SkillProvider, SkillProviders, catalog::*, provider::*},
};

const PRIVATE_SKILL_BODY: &str = "PRIVATE_SKILL_INSTRUCTIONS_FOR_MODEL";
struct HostProvider;
impl SkillProvider for HostProvider {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async {
            Ok(SkillCatalog {
                entries: vec![SkillCatalogEntry::new(
                    SkillPackageId("/skills/demo/SKILL.md".into()),
                    SkillAuthority::new(SkillSourceKind::Host, "host"),
                    "demo",
                    "A test skill",
                    SkillResourceId::new("/skills/demo/SKILL.md"),
                )],
                warnings: vec![],
            })
        })
    }
    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            Ok(SkillReadResult {
                resource: request.resource,
                contents: PRIVATE_SKILL_BODY.into(),
            })
        })
    }
}

#[tokio::test]
async fn skill_instructions_are_hidden_in_live_items_and_loaded_and_replayed_thread_reads() {
    let script = Arc::new(Script::default());
    let skills = SkillsNsCapability::host_catalog(
        SkillProviders::new().with_host_provider(Arc::new(HostProvider)),
    )
    .unwrap();
    let factory = Arc::new(Factory::running(
        &script,
        false,
        agent(),
        RunConfig::new().with_capability(Arc::new(skills)),
    ));
    let server = Server::start(factory);
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.answer("Done");
    server
        .run_turn(&mut connection, thread.id(), "Use $demo")
        .await;

    let request = script.last_request();
    assert!(
        request
            .input()
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::Message(message) => Some(message),
                _ => None,
            })
            .flat_map(Message::content)
            .filter_map(|block| block.as_text())
            .any(|text| ra_tools::skills_ns::is_skill_prompt_fragment(text)
                && text.contains(PRIVATE_SKILL_BODY))
    );
    let events = connection.drain().await;
    let user_events: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event["method"].as_str(),
                Some("item/started" | "item/completed")
            ) && event["params"]["item"]["type"] == "userMessage"
        })
        .collect();
    assert_eq!(user_events.len(), 2);
    for event in user_events {
        assert_eq!(event["params"]["item"]["content"][0]["text"], "Use $demo");
    }
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains(PRIVATE_SKILL_BODY)
    );

    let read = server
        .read(
            &mut connection,
            ThreadReadParams::new(thread.id()).including_turns(),
        )
        .await;
    assert_eq!(user_texts(&read.turns()[0]), ["Use $demo"]);
    assert!(
        !serde_json::to_string(&read)
            .unwrap()
            .contains(PRIVATE_SKILL_BODY)
    );

    let (replay, _) = build(
        Options {
            store: Some(Arc::clone(&server.store)),
            factory: None,
            unload_delay: None,
        },
        None,
    );
    let mut replay_connection = replay.connect(2).await;
    let restored = replay
        .read(
            &mut replay_connection,
            ThreadReadParams::new(thread.id()).including_turns(),
        )
        .await;
    assert_eq!(restored.turns().len(), 1);
    assert_eq!(user_texts(&restored.turns()[0]), ["Use $demo"]);
    assert!(
        !serde_json::to_string(&restored)
            .unwrap()
            .contains(PRIVATE_SKILL_BODY)
    );
}
