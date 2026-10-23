// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Bounded metadata and warning pages, including the upstream budget table unchanged.
use super::*;

#[tokio::test]
async fn skills_list_only_returns_model_visible_bounded_metadata() {
    const OMITTED: &str = "Some skills were omitted because their metadata is too large.";
    const DISCOVERY: &str = "Some cloud skills could not be discovered because the provider disconnected before returning the complete catalog.";
    let supplementary = "w".repeat(256);
    let opaque = "\\".repeat(1500);
    let mut oversized = entry(
        SkillSourceKind::Cloud,
        "codex_apps",
        &format!("cloud/{opaque}"),
        &format!("skill://cloud/{opaque}/SKILL.md"),
    );
    oversized.description = "x".repeat(1025);
    let [first, mut middle, mut last] = ["p0", "p1", "p2"].map(|name| {
        entry(
            SkillSourceKind::Cloud,
            "codex_apps",
            name,
            &format!("skill://{name}/SKILL.md"),
        )
    });
    middle.description = "d".repeat(110);
    last.description = "d".repeat(50);
    let providers = cloud(
        SkillCatalog {
            entries: vec![
                oversized,
                first,
                middle,
                last,
                entry(
                    SkillSourceKind::Cloud,
                    "codex_apps",
                    "cloud/hidden",
                    "skill://cloud/hidden/SKILL.md",
                )
                .hidden_from_prompt(),
            ],
            warnings: vec![
                DISCOVERY.into(),
                supplementary.clone(),
                supplementary.clone(),
                supplementary.clone(),
            ],
        },
        "# Lint Fix\n\nRun the formatter.",
    );
    let full = tools(providers.clone(), 12000);
    let complete = response(full[0].as_ref(), json!({"authority":{"kind":"cloud"}})).await;
    let skills = complete["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 4);
    assert_eq!(skills[0]["description"], "x".repeat(1021) + "...");
    assert_eq!(complete["warnings"].as_array().unwrap().len(), 4);
    assert_eq!(complete["next_cursor"], Value::Null);
    let mut cursor = None::<String>;
    // Byte budgets and expected offsets are copied from upstream. The Rust host supplies these
    // directly, without applying Codex's serialization multiplier a second time.
    for (index, (budget, indices, warnings, offset)) in [
        (7750, vec![], vec![DISCOVERY], Some("0")),
        (7750, vec![0], vec![], Some("1")),
        (320, vec![1], vec![], Some("2")),
        (220, vec![], vec![OMITTED], Some("2")),
        (220, vec![3], vec![], None),
        (220, vec![], vec![DISCOVERY], Some("0")),
        (220, vec![], vec![OMITTED], Some("0")),
        (182, vec![], vec![OMITTED], Some("0")),
        (182, vec![1], vec![], None),
        (
            2400,
            vec![1, 2, 3],
            vec![
                DISCOVERY,
                &supplementary,
                &supplementary,
                &supplementary,
                OMITTED,
            ],
            None,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let pair = tools(providers.clone(), budget);
        let page = response(
            pair[0].as_ref(),
            json!({"authority":{"kind":"cloud"},"cursor":cursor}),
        )
        .await;
        assert!(serde_json::to_vec(&page).unwrap().len() <= budget);
        let next = page["next_cursor"].as_str().map(str::to_string);
        assert_eq!(
            next.as_deref().and_then(|cursor| cursor.split(':').nth(1)),
            offset,
            "page {index}"
        );
        assert_eq!(
            page,
            json!({"skills":indices.into_iter().map(|i| &skills[i]).collect::<Vec<_>>(),"warnings":warnings,"next_cursor":next}),
            "page {index}"
        );
        if index == 2 {
            let legacy = next.as_ref().unwrap().rsplit_once(':').unwrap().0;
            let page = response(
                full[0].as_ref(),
                json!({"authority":{"kind":"cloud"},"cursor":legacy}),
            )
            .await;
            assert_eq!(
                page,
                json!({"skills": &skills[2..],"warnings":[],"next_cursor":null})
            );
        }
        cursor = next;
    }
    let small = tools(providers.clone(), 64);
    assert_eq!(
        error_message(
            call(small[0].as_ref(), json!({"authority":{"kind":"cloud"}}))
                .await
                .unwrap_err()
        ),
        "skills.list response budget leaves no room for discovery warnings"
    );
    let small = tools(providers, 2000);
    assert_eq!(
        error_message(
            call(
                small[1].as_ref(),
                json!({"package":format!("cloud/{opaque}")})
            )
            .await
            .unwrap_err()
        ),
        "skills.read response budget leaves no room for contents"
    );
}

#[tokio::test]
async fn list_filters_authorities_bounds_unicode_and_paginates_twenty_entries() {
    let mut entries = (0..45)
        .map(|i| {
            entry(
                SkillSourceKind::Cloud,
                "codex_apps",
                &format!("p{i}"),
                &format!("skill://p{i}/SKILL.md"),
            )
        })
        .collect::<Vec<_>>();
    entries[0].name = "技".repeat(100);
    entries[0].description = "界".repeat(1100);
    entries.push(entries[0].clone().disabled());
    entries.push(entry(SkillSourceKind::Host, "host", "host", "host.md"));
    entries.push(entry(SkillSourceKind::Cloud, "other", "other", "other.md"));
    entries.push(entry(
        SkillSourceKind::Cloud,
        "codex_apps",
        &"p".repeat(2049),
        "huge",
    ));
    let pair = tools(
        cloud(
            SkillCatalog {
                entries,
                warnings: (0..8).map(|_| "界".repeat(100)).collect(),
            },
            "",
        ),
        16000,
    );
    let first = response(pair[0].as_ref(), json!({"authority":{"kind":"cloud"}})).await;
    let entries = first["skills"].as_array().unwrap();
    assert_eq!(entries.len(), 20);
    assert_eq!(entries[0]["name"].as_str().unwrap().len(), 255);
    assert_eq!(
        entries[0]["description"].as_str().unwrap().chars().count(),
        1024
    );
    assert_eq!(first["warnings"].as_array().unwrap().len(), 5);
    assert_eq!(first["warnings"][0].as_str().unwrap().len(), 255);
    let second = response(
        pair[0].as_ref(),
        json!({"authority":{"kind":"cloud"},"cursor":first["next_cursor"]}),
    )
    .await;
    assert_eq!(second["skills"].as_array().unwrap().len(), 20);
    assert_eq!(second["warnings"], json!([]));
    let third = response(
        pair[0].as_ref(),
        json!({"authority":{"kind":"cloud"},"cursor":second["next_cursor"]}),
    )
    .await;
    assert_eq!(third["skills"].as_array().unwrap().len(), 5);
    assert!(third["next_cursor"].is_null());
}

#[tokio::test]
async fn selectors_and_cursors_use_the_upstream_errors() {
    let pair = tools(cloud(SkillCatalog::default(), ""), 1000);
    // Keep serde's upstream acceptance of fields on internally tagged unit selectors.
    assert_eq!(
        response(
            pair[0].as_ref(),
            json!({"authority":{"kind":"executor","id":"ignored"}})
        )
        .await["skills"],
        json!([])
    );
    for (args, message) in [
        (json!({}), "missing field `authority`"),
        (
            json!({"authority":{"kind":"executor"},"unknown":1}),
            "unknown field",
        ),
        (json!({"authority":{"kind":"host"}}), "unknown variant"),
        (
            json!({"authority":{"kind":"cloud"},"cursor":"invalid"}),
            "skills.list cursor is invalid",
        ),
        (
            json!({"authority":{"kind":"cloud"},"cursor":"0000000000000000:0"}),
            "skills.list cursor is stale; restart from the first page",
        ),
    ] {
        let error = call(pair[0].as_ref(), args).await.unwrap_err();
        assert!(error_message(error).contains(message));
    }
    assert_eq!(
        response(pair[0].as_ref(), json!({"authority":{"kind":"cloud"}})).await,
        json!({"skills":[],"warnings":[],"next_cursor":null})
    );
}

#[tokio::test]
async fn list_cursors_reject_changed_catalogs_and_invalid_offsets_or_budget_parts() {
    let catalog = SkillCatalog {
        entries: (0..21)
            .map(|i| {
                entry(
                    SkillSourceKind::Cloud,
                    "codex_apps",
                    &format!("p{i}"),
                    &format!("p{i}/SKILL.md"),
                )
            })
            .collect(),
        warnings: vec![],
    };
    let pair = tools(cloud(catalog.clone(), ""), 12000);
    let first = response(pair[0].as_ref(), json!({"authority":{"kind":"cloud"}})).await;
    let cursor = first["next_cursor"].as_str().unwrap();
    let fingerprint = cursor.split_once(':').unwrap().0;
    for invalid in [
        format!("{fingerprint}:9999"),
        format!("{fingerprint}:20:budget"),
        format!("{fingerprint}:20:4:5"),
    ] {
        assert_eq!(
            error_message(
                call(
                    pair[0].as_ref(),
                    json!({"authority":{"kind":"cloud"},"cursor":invalid})
                )
                .await
                .unwrap_err()
            ),
            "skills.list cursor is invalid"
        );
    }
    let mut changed = catalog;
    changed.entries[0].description = "updated metadata".into();
    let changed = tools(cloud(changed, ""), 12000);
    assert_eq!(
        error_message(
            call(
                changed[0].as_ref(),
                json!({"authority":{"kind":"cloud"},"cursor":cursor})
            )
            .await
            .unwrap_err()
        ),
        "skills.list cursor is stale; restart from the first page"
    );
}
