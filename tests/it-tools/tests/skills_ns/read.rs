// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Executor references and cached continuations from the core and app-server suites.
use super::*;
use ra_exec::fs::Workspace;
use tempfile::TempDir;

pub(super) struct ExecutorFixture {
    pub directory: TempDir,
    pub workspace: Workspace,
    pub package: String,
    pub resource: String,
    pub main: SkillResourceId,
    pub providers: SkillProviders,
}
impl ExecutorFixture {
    pub fn new(contents: &str) -> Self {
        let directory = TempDir::new().unwrap();
        std::fs::create_dir(directory.path().join("skill")).unwrap();
        std::fs::write(
            directory.path().join("skill/SKILL.md"),
            "Main instructions.",
        )
        .unwrap();
        std::fs::write(directory.path().join("skill/reference.md"), contents).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let package = "skill://selected/skill".to_string();
        let resource = format!("{package}/reference.md");
        let main = SkillResourceId::environment(
            format!("{package}/SKILL.md"),
            LOCAL_ENVIRONMENT_ID,
            workspace.root().join("skill/SKILL.md"),
        );
        let entry = SkillCatalogEntry::new(
            SkillPackageId(package.clone()),
            SkillAuthority::new(SkillSourceKind::Executor, "selected"),
            "reference",
            "Read the reference.",
            main.clone(),
        )
        .with_alias_root("skill://selected");
        let providers = SkillProviders::new().with_executor_provider(Arc::new(
            ExecutorSkillProvider::new(SkillCatalog {
                entries: vec![entry],
                warnings: vec![],
            }),
        ));
        Self {
            directory,
            workspace,
            package,
            resource,
            main,
            providers,
        }
    }
    pub fn pair(&self, budget: usize) -> Vec<Arc<dyn Tool>> {
        skill_tools(
            self.providers.clone(),
            roots("selected", self.workspace.clone()),
            budget,
        )
        .unwrap()
    }
    pub fn reference_args(&self) -> Value {
        json!({"package":self.package,"resource":self.resource})
    }
}

#[tokio::test]
async fn executor_skill_tool_reads_references_within_the_host_grant() {
    let fixture = ExecutorFixture::new("Live executor reference instructions.");
    let pair = fixture.pair(1000);
    let expected_root = fixture.workspace.root().join("skill");
    let page = response(pair[1].as_ref(), fixture.reference_args()).await;
    assert_eq!(
        page,
        json!({"resource":fixture.resource,"contents":"Live executor reference instructions.","skill_root":expected_root,"next_cursor":null})
    );
    for args in [
        json!({"package":fixture.package}),
        json!({"package":"e0/skill","unknown":"accepted"}),
        json!({"package":fixture.package,"resource":fixture.main.as_str()}),
    ] {
        let page = response(pair[1].as_ref(), args).await;
        assert_eq!(page["contents"], "Main instructions.");
        assert_eq!(page["resource"], fixture.main.as_str());
        assert_eq!(page["skill_root"], expected_root.to_string_lossy().as_ref());
    }
    for resource in [
        "skill://selected/skill/../outside",
        "skill://selected/other/ref.md",
        "skill://selected/skill//reference.md",
    ] {
        assert_eq!(
            error_message(
                call(
                    pair[1].as_ref(),
                    json!({"package":fixture.package,"resource":resource})
                )
                .await
                .unwrap_err()
            ),
            "skill environment is not available for this callback"
        );
    }
    let unavailable = skill_tools(
        fixture.providers.clone(),
        roots("other", fixture.workspace.clone()),
        1000,
    )
    .unwrap();
    assert_eq!(
        error_message(
            call(unavailable[1].as_ref(), fixture.reference_args())
                .await
                .unwrap_err()
        ),
        "skill package is not available"
    );
}

#[tokio::test]
async fn executor_continuations_keep_edits_cached_until_a_fresh_read_replaces_the_slot() {
    let original = "reference line\n技\"\\".repeat(800);
    let fixture = ExecutorFixture::new(&original);
    let pair = fixture.pair(1200);
    let mut page = response(pair[1].as_ref(), fixture.reference_args()).await;
    let first_cursor = page["next_cursor"].as_str().unwrap().to_string();
    assert!(!page["contents"].as_str().unwrap().is_empty());
    std::fs::write(
        fixture.directory.path().join("skill/reference.md"),
        "CHANGED_REFERENCE\n".repeat(800),
    )
    .unwrap();
    let mut contents = page["contents"].as_str().unwrap().to_string();
    while let Some(cursor) = page["next_cursor"].as_str() {
        page = response(
            pair[1].as_ref(),
            json!({"package":fixture.package,"resource":fixture.resource,"cursor":cursor}),
        )
        .await;
        assert!(serde_json::to_vec(&page).unwrap().len() <= 1200);
        let part = page["contents"].as_str().unwrap();
        assert!(!part.is_empty());
        contents.push_str(part);
        assert!(original.starts_with(&contents));
    }
    assert_eq!(contents, original);
    // The last page does not clear the snapshot.
    let again = response(
        pair[1].as_ref(),
        json!({"package":fixture.package,"resource":fixture.resource,"cursor":first_cursor}),
    )
    .await;
    assert!(original.contains(again["contents"].as_str().unwrap()));
    let fresh = response(pair[1].as_ref(), fixture.reference_args()).await;
    assert!(
        fresh["contents"]
            .as_str()
            .unwrap()
            .starts_with("CHANGED_REFERENCE")
    );
    assert_eq!(
        error_message(
            call(
                pair[1].as_ref(),
                json!({"package":fixture.package,"resource":fixture.resource,"cursor":first_cursor})
            )
            .await
            .unwrap_err()
        ),
        "skills.read cursor is stale; restart from the first page"
    );
}

#[tokio::test]
async fn cache_eviction_and_new_pairs_revalidate_the_cursor_against_current_contents() {
    let fixture = ExecutorFixture::new(&"original\n".repeat(1000));
    let pair = fixture.pair(1000);
    let first = response(pair[1].as_ref(), fixture.reference_args()).await;
    std::fs::write(
        fixture.directory.path().join("skill/reference.md"),
        "edited\n".repeat(1000),
    )
    .unwrap();
    let fresh_pair = fixture.pair(1000);
    assert_eq!(error_message(call(fresh_pair[1].as_ref(), json!({"package":fixture.package,"resource":fixture.resource,"cursor":first["next_cursor"]})).await.unwrap_err()), "skills.read cursor is stale; restart from the first page");
    std::fs::write(
        fixture.directory.path().join("skill/other.md"),
        "other\n".repeat(1000),
    )
    .unwrap();
    response(
        pair[1].as_ref(),
        json!({"package":fixture.package,"resource":format!("{}/other.md", fixture.package)}),
    )
    .await;
    assert_eq!(error_message(call(pair[1].as_ref(), json!({"package":fixture.package,"resource":fixture.resource,"cursor":first["next_cursor"]})).await.unwrap_err()), "skills.read cursor is stale; restart from the first page");
}

#[tokio::test]
async fn read_validates_utf8_cursor_offsets_and_handle_limits() {
    let fixture = ExecutorFixture::new(&"技".repeat(1000));
    let pair = fixture.pair(1000);
    let page = response(pair[1].as_ref(), fixture.reference_args()).await;
    let cursor = page["next_cursor"].as_str().unwrap();
    let fingerprint = cursor.split_once(':').unwrap().0;
    for cursor in [
        format!("{fingerprint}:1"),
        format!("{fingerprint}:999999"),
        format!("{fingerprint}:not-offset"),
        "no-colon".to_string(),
    ] {
        assert_eq!(
            error_message(
                call(
                    pair[1].as_ref(),
                    json!({"package":fixture.package,"resource":fixture.resource,"cursor":cursor})
                )
                .await
                .unwrap_err()
            ),
            "skills.read cursor is invalid"
        );
    }
    for package in ["".to_string(), "bad\nhandle".to_string(), "x".repeat(2049)] {
        assert_eq!(
            error_message(
                call(pair[1].as_ref(), json!({"package":package}))
                    .await
                    .unwrap_err()
            ),
            "package must be non-empty, contain no control characters, and be at most 2048 bytes"
        );
    }
    assert_eq!(
        error_message(
            call(
                pair[1].as_ref(),
                json!({"package":fixture.package,"resource":""})
            )
            .await
            .unwrap_err()
        ),
        "resource must be non-empty, contain no control characters, and be at most 2048 bytes"
    );
    assert_eq!(
        error_message(call(pair[1].as_ref(), json!({})).await.unwrap_err()),
        "missing field `package`"
    );
    assert_eq!(
        error_message(
            call(pair[1].as_ref(), json!({"package":"absent"}))
                .await
                .unwrap_err()
        ),
        "skill package is not available"
    );
}

#[tokio::test]
async fn hidden_packages_remain_readable_and_cloud_precedes_executor() {
    let mut hidden = entry(
        SkillSourceKind::Cloud,
        "codex_apps",
        "hidden",
        "skill://hidden/SKILL.md",
    )
    .hidden_from_prompt();
    hidden.name = "Hidden".into();
    let pair = tools(
        cloud(
            SkillCatalog {
                entries: vec![hidden],
                warnings: vec![],
            },
            "hidden contents",
        ),
        1000,
    );
    let page = response(pair[1].as_ref(), json!({"package":"hidden"})).await;
    assert_eq!(
        page,
        json!({"resource":"skill://hidden/SKILL.md","contents":"hidden contents","next_cursor":null})
    );
    let disabled = entry(
        SkillSourceKind::Cloud,
        "codex_apps",
        "disabled",
        "disabled.md",
    )
    .disabled();
    let cloud_providers = cloud(
        SkillCatalog {
            entries: vec![
                disabled,
                entry(SkillSourceKind::Cloud, "codex_apps", "shared", "cloud.md"),
            ],
            warnings: vec![],
        },
        "cloud contents",
    );
    let providers = cloud_providers.with_executor_provider(Arc::new(StaticProvider {
        catalog: SkillCatalog {
            entries: vec![entry(
                SkillSourceKind::Executor,
                "root",
                "shared",
                "executor.md",
            )],
            warnings: vec![],
        },
        contents: "executor contents".into(),
    }));
    let pair = tools(providers, 1000);
    assert_eq!(
        response(pair[1].as_ref(), json!({"package":"shared"})).await["contents"],
        "cloud contents"
    );
    assert_eq!(
        error_message(
            call(pair[1].as_ref(), json!({"package":"disabled"}))
                .await
                .unwrap_err()
        ),
        "skill package is not available"
    );
}

struct ChangingProvider {
    catalog: SkillCatalog,
    contents: Arc<std::sync::Mutex<String>>,
}
impl SkillProvider for ChangingProvider {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async { Ok(self.catalog.clone()) })
    }
    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            Ok(SkillReadResult {
                resource: request.resource,
                contents: self.contents.lock().unwrap().clone(),
            })
        })
    }
}

#[tokio::test]
async fn cloud_resources_and_executor_resources_over_the_limit_are_never_snapshotted() {
    for (kind, size) in [
        (SkillSourceKind::Cloud, 3000),
        (
            SkillSourceKind::Executor,
            MAX_SKILL_RESOURCE_CONTENT_BYTES + 1,
        ),
    ] {
        let fixture = ExecutorFixture::new("unused");
        let contents = Arc::new(std::sync::Mutex::new("original".repeat(size / 8 + 1)));
        let entry = if kind == SkillSourceKind::Cloud {
            entry(kind.clone(), "codex_apps", "pkg", "skill://pkg/SKILL.md")
        } else {
            SkillCatalogEntry::new(
                SkillPackageId(fixture.package.clone()),
                SkillAuthority::new(kind.clone(), "selected"),
                "Skill",
                "Read",
                fixture.main.clone(),
            )
        };
        let package = entry.id.0.clone();
        let provider = Arc::new(ChangingProvider {
            catalog: SkillCatalog {
                entries: vec![entry],
                warnings: vec![],
            },
            contents: contents.clone(),
        });
        let providers = SkillProviders::new()
            .with_provider(SkillProviderSource::new(kind, "changing", provider));
        let pair = skill_tools(providers, roots("selected", fixture.workspace), 1000).unwrap();
        let first = response(pair[1].as_ref(), json!({"package":package})).await;
        *contents.lock().unwrap() = "changed".repeat(size / 7 + 1);
        assert_eq!(
            error_message(
                call(
                    pair[1].as_ref(),
                    json!({"package":package,"cursor":first["next_cursor"]})
                )
                .await
                .unwrap_err()
            ),
            "skills.read cursor is stale; restart from the first page"
        );
    }
}

#[tokio::test]
async fn native_paths_preserve_the_upstream_absolute_environment_path_contract() {
    let fixture = ExecutorFixture::new("unused");
    for (environment, path) in [
        ("remote", fixture.workspace.root().join("skill/SKILL.md")),
        (
            LOCAL_ENVIRONMENT_ID,
            std::path::PathBuf::from("skill/SKILL.md"),
        ),
    ] {
        let catalog = SkillCatalog {
            entries: vec![SkillCatalogEntry::new(
                SkillPackageId(fixture.package.clone()),
                SkillAuthority::new(SkillSourceKind::Executor, "selected"),
                "Skill",
                "Read",
                SkillResourceId::environment(fixture.main.as_str(), environment, path),
            )],
            warnings: vec![],
        };
        let pair = skill_tools(
            SkillProviders::new()
                .with_executor_provider(Arc::new(ExecutorSkillProvider::new(catalog))),
            roots("selected", fixture.workspace.clone()),
            1000,
        )
        .unwrap();
        assert_eq!(
            error_message(
                call(pair[1].as_ref(), json!({"package":fixture.package}))
                    .await
                    .unwrap_err()
            ),
            "skill environment is not available for this callback"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn executor_opens_non_utf8_paths_without_reading_a_lossy_replacement() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let directory = TempDir::new().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let actual_root = workspace
        .root()
        .join(OsString::from_vec(b"skill-\xff".to_vec()));
    let native_path_exists = match std::fs::create_dir(&actual_root) {
        Ok(()) => {
            std::fs::write(actual_root.join("SKILL.md"), "actual native path").unwrap();
            true
        }
        // APFS rejects invalid UTF-8 names with EILSEQ. Even there, a failed native lookup must
        // not open the existing Unicode replacement below.
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => false,
        Err(error) => panic!("failed to create native-path fixture: {error}"),
    };
    let replacement = workspace.root().join("skill-\u{fffd}");
    std::fs::create_dir(&replacement).unwrap();
    std::fs::write(replacement.join("SKILL.md"), "wrong lossy replacement").unwrap();
    let main = SkillResourceId::environment(
        "skill://root/pkg/SKILL.md",
        LOCAL_ENVIRONMENT_ID,
        actual_root.join("SKILL.md"),
    );
    let catalog = SkillCatalog {
        entries: vec![SkillCatalogEntry::new(
            SkillPackageId("skill://root/pkg".into()),
            SkillAuthority::new(SkillSourceKind::Executor, "root"),
            "Native",
            "Read",
            main,
        )],
        warnings: vec![],
    };
    let pair = skill_tools(
        SkillProviders::new().with_executor_provider(Arc::new(ExecutorSkillProvider::new(catalog))),
        roots("root", workspace),
        1000,
    )
    .unwrap();
    let args = json!({"package":"skill://root/pkg"});
    if native_path_exists {
        let page = response(pair[1].as_ref(), args).await;
        assert_eq!(page["contents"], "actual native path");
        assert_eq!(page["skill_root"], actual_root.to_string_lossy().as_ref());
    } else {
        assert_eq!(
            error_message(call(pair[1].as_ref(), args).await.unwrap_err()),
            "failed to read skill resource"
        );
    }
}
