//! Provider boundaries and the exact executor read errors.
use super::read::ExecutorFixture;
use super::*;

#[test]
fn packages_and_catalogs_preserve_upstream_boundaries_and_first_seen_entries() {
    let package = SkillPackageId("skill://root/pkg".into());
    for suffix in ["", "/", "/.", "/..", "/a/../b", "/a//b", "/a/./b"] {
        assert_eq!(
            package.relative_resource_path(&format!("{}{suffix}", package.0)),
            None
        );
    }
    assert_eq!(
        package.relative_resource_path("skill://root/pkg/ref/file.md"),
        Some("ref/file.md")
    );
    assert_eq!(
        package.relative_resource_path("skill://root/pkg-other/file.md"),
        None
    );
    let first = entry(SkillSourceKind::Executor, "root", "same", "first");
    let mut replacement = first.clone();
    replacement.name = "replacement".into();
    let mut catalog = SkillCatalog::default();
    catalog.push_entry(first.clone());
    catalog.extend(SkillCatalog {
        entries: vec![
            replacement,
            entry(SkillSourceKind::Cloud, "codex_apps", "same", "second"),
        ],
        warnings: vec!["warning".into()],
    });
    assert_eq!(catalog.entries.len(), 2);
    assert_eq!(catalog.entries[0], first);
    assert_eq!(catalog.warnings, ["warning"]);
}

#[tokio::test]
async fn executor_provider_bounds_streams_checks_utf8_and_preserves_preloaded_contents() {
    let fixture = ExecutorFixture::new("reference");
    let provider = ExecutorSkillProvider::new(SkillCatalog::default());
    let resource = fixture
        .main
        .bind_environment_package_resource(
            &SkillPackageId(fixture.package.clone()),
            fixture.resource.clone(),
        )
        .unwrap();
    let access = ra_tools::skills_ns::SkillExecutorAccess::new(fixture.workspace.clone(), None);
    let base = SkillReadRequest {
        authority: SkillAuthority::new(SkillSourceKind::Executor, "selected"),
        package: SkillPackageId(fixture.package.clone()),
        resource,
        context: SkillReadContext::Executor { access: &access },
    };
    let path = fixture.directory.path().join("skill/reference.md");
    std::fs::write(&path, vec![b'x'; MAX_SKILL_RESOURCE_CONTENT_BYTES]).unwrap();
    assert_eq!(
        provider.read(base.clone()).await.unwrap().contents.len(),
        MAX_SKILL_RESOURCE_CONTENT_BYTES
    );
    std::fs::write(&path, vec![b'x'; MAX_SKILL_RESOURCE_CONTENT_BYTES + 1]).unwrap();
    assert_eq!(
        provider.read(base.clone()).await.unwrap_err().message,
        format!(
            "executor skill resource {} exceeds 1048576 bytes",
            fixture.resource
        )
    );
    std::fs::write(&path, [0xff, 0xfe]).unwrap();
    assert_eq!(
        provider.read(base.clone()).await.unwrap_err().message,
        format!(
            "executor skill resource {} is not valid UTF-8",
            fixture.resource
        )
    );
    let mut wrong = base.clone();
    wrong.authority.kind = SkillSourceKind::Cloud;
    assert_eq!(
        provider.read(wrong).await.unwrap_err().message,
        "executor skill provider cannot read cloud resources"
    );
    let mut wrong = base.clone();
    wrong.authority.id = "other".into();
    assert_eq!(
        provider.read(wrong).await.unwrap_err().message,
        "executor skill resource does not match its package"
    );
    let mut wrong = base.clone();
    wrong.context = SkillReadContext::Cloud;
    assert_eq!(
        provider.read(wrong).await.unwrap_err().message,
        "executor skill reads require filesystem access"
    );
    let mut wrong = base.clone();
    wrong.resource = SkillResourceId::new(&fixture.resource);
    assert_eq!(
        provider.read(wrong).await.unwrap_err().message,
        "executor skill resource is not bound to an environment"
    );
    let preloaded = SkillResourceId::environment_with_contents(
        fixture.main.as_str(),
        LOCAL_ENVIRONMENT_ID,
        fixture.workspace.root().join("skill/SKILL.md"),
        "discovered contents".into(),
    );
    std::fs::remove_file(fixture.directory.path().join("skill/SKILL.md")).unwrap();
    assert_eq!(
        provider
            .read(SkillReadRequest {
                resource: preloaded,
                ..base
            })
            .await
            .unwrap()
            .contents,
        "discovered contents"
    );
}

#[tokio::test]
async fn host_root_and_symlink_escapes_are_refused_and_failure_text_is_bounded() {
    let fixture = ExecutorFixture::new("contents");
    let access = ra_tools::skills_ns::SkillExecutorAccess::new(fixture.workspace.clone(), None);
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret"), "secret contents").unwrap();
    let pair = fixture.pair(1000);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            fixture.directory.path().join("skill/escape.md"),
        )
        .unwrap();
        let args =
            json!({"package":fixture.package,"resource":format!("{}/escape.md",fixture.package)});
        let error = call(pair[1].as_ref(), args.clone()).await.unwrap_err();
        assert_eq!(error_message(error), "failed to read skill resource");
        let error = call(pair[1].as_ref(), args.clone()).await.unwrap_err();
        let run = run();
        let id = CallId::new("failure");
        let context = ToolContext::new(&run, pair[1].as_ref(), &id, &args);
        let output = pair[1]
            .handle_failure(&context, &error)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.as_text(), Some("failed to read skill resource"));
    }
    let provider = ExecutorSkillProvider::new(SkillCatalog::default());
    let resource = SkillResourceId::environment(
        format!("{}/secret", fixture.package),
        LOCAL_ENVIRONMENT_ID,
        outside.path().join("secret"),
    );
    let error = provider
        .read(SkillReadRequest {
            authority: SkillAuthority::new(SkillSourceKind::Executor, "selected"),
            package: SkillPackageId(fixture.package),
            resource,
            context: SkillReadContext::Executor { access: &access },
        })
        .await
        .unwrap_err();
    assert!(
        error
            .message
            .starts_with("failed to read executor skill resource")
    );
    assert!(!error.message.contains("secret contents"));
}

struct BrokenProvider {
    catalog: SkillCatalog,
    wrong_resource: bool,
}
impl SkillProvider for BrokenProvider {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async { Ok(self.catalog.clone()) })
    }
    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            if self.wrong_resource {
                Ok(SkillReadResult {
                    resource: SkillResourceId::new("wrong"),
                    contents: "wrong".into(),
                })
            } else {
                Err(SkillProviderError::new(format!(
                    "private source failure for {}",
                    request.resource.as_str()
                )))
            }
        })
    }
}

#[tokio::test]
async fn providers_fall_back_by_kind_but_resource_mismatches_are_fatal() {
    let catalog = SkillCatalog {
        entries: vec![entry(
            SkillSourceKind::Cloud,
            "codex_apps",
            "pkg",
            "skill://pkg/SKILL.md",
        )],
        warnings: vec![],
    };
    let providers = SkillProviders::new()
        .with_cloud_provider(Arc::new(BrokenProvider {
            catalog: catalog.clone(),
            wrong_resource: false,
        }))
        .with_cloud_provider(Arc::new(StaticProvider {
            catalog: catalog.clone(),
            contents: "fallback".into(),
        }));
    let pair = tools(providers, 1000);
    assert_eq!(
        response(pair[1].as_ref(), json!({"package":"pkg"})).await["contents"],
        "fallback"
    );
    let pair = tools(
        SkillProviders::new().with_cloud_provider(Arc::new(BrokenProvider {
            catalog,
            wrong_resource: true,
        })),
        1000,
    );
    let args = json!({"package":"pkg"});
    let error = call(pair[1].as_ref(), args.clone()).await.unwrap_err();
    assert_eq!(
        error_message(call(pair[1].as_ref(), args.clone()).await.unwrap_err()),
        "skill provider returned a different resource"
    );
    let run = run();
    let id = CallId::new("fatal");
    assert!(
        pair[1]
            .handle_failure(
                &ToolContext::new(&run, pair[1].as_ref(), &id, &args),
                &error
            )
            .await
            .unwrap()
            .is_none()
    );
}

struct UnavailableProvider(std::sync::atomic::AtomicUsize);
impl SkillProvider for UnavailableProvider {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(SkillProviderError::new("disconnected"))
        })
    }
    fn read<'a>(&'a self, _: SkillReadRequest<'a>) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async { Err(SkillProviderError::new("last provider error")) })
    }
}

#[tokio::test]
async fn executor_discovery_failures_are_cached_and_warnings_are_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(UnavailableProvider(std::sync::atomic::AtomicUsize::new(0)));
    let pair = skill_tools(
        SkillProviders::new().with_executor_provider(provider.clone()),
        roots(
            "root",
            ra_exec::fs::Workspace::open(directory.path()).unwrap(),
        ),
        1000,
    )
    .unwrap();
    let args = json!({"authority":{"kind":"executor"}});
    let (left, right) = tokio::join!(
        response(pair[0].as_ref(), args.clone()),
        response(pair[0].as_ref(), args)
    );
    assert_eq!(
        left,
        json!({"skills":[],"warnings":["executor skills unavailable: disconnected"],"next_cursor":null})
    );
    assert_eq!(left, right);
    assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

struct MutableCatalogProvider {
    outcome: std::sync::Mutex<SkillProviderResult<SkillCatalog>>,
    queries: std::sync::Mutex<Vec<String>>,
}
impl SkillProvider for MutableCatalogProvider {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            self.queries.lock().unwrap().push(query.turn_id);
            self.outcome.lock().unwrap().clone()
        })
    }
    fn read<'a>(&'a self, _: SkillReadRequest<'a>) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async { Err(SkillProviderError::new("unused read")) })
    }
}

#[tokio::test]
async fn executor_catalog_refreshes_entries_warnings_and_query_identity_when_the_run_changes() {
    let provider = Arc::new(MutableCatalogProvider {
        outcome: std::sync::Mutex::new(Err(SkillProviderError::new("temporary failure"))),
        queries: Default::default(),
    });
    let directory = tempfile::tempdir().unwrap();
    let pair = skill_tools(
        SkillProviders::new().with_executor_provider(provider.clone()),
        roots(
            "root",
            ra_exec::fs::Workspace::open(directory.path()).unwrap(),
        ),
        8000,
    )
    .unwrap();
    let args = json!({"authority":{"kind":"executor"}});
    let first = call_in_run(pair[0].as_ref(), args.clone(), "run-a")
        .await
        .unwrap();
    let first: Value = serde_json::from_str(first.as_text().unwrap()).unwrap();
    assert_eq!(
        first["warnings"],
        json!(["executor skills unavailable: temporary failure"])
    );
    *provider.outcome.lock().unwrap() = Ok(SkillCatalog {
        entries: vec![entry(
            SkillSourceKind::Executor,
            "root",
            "skill://root/new",
            "skill://root/new/SKILL.md",
        )],
        warnings: vec!["new discovery warning".into()],
    });
    let same = call_in_run(pair[0].as_ref(), args.clone(), "run-a")
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(same.as_text().unwrap()).unwrap(),
        first
    );
    let (left, right) = tokio::join!(
        call_in_run(pair[0].as_ref(), args.clone(), "run-b"),
        call_in_run(pair[0].as_ref(), args.clone(), "run-b")
    );
    let refreshed: Value = serde_json::from_str(left.unwrap().as_text().unwrap()).unwrap();
    assert_eq!(refreshed["skills"][0]["package"], "skill://root/new");
    assert_eq!(refreshed["warnings"], json!(["new discovery warning"]));
    assert_eq!(
        serde_json::from_str::<Value>(right.unwrap().as_text().unwrap()).unwrap(),
        refreshed
    );
    assert_eq!(*provider.queries.lock().unwrap(), ["run-a", "run-b"]);
    *provider.outcome.lock().unwrap() = Ok(SkillCatalog::default());
    for run_id in ["run-c", "run-a"] {
        let page = call_in_run(pair[0].as_ref(), args.clone(), run_id)
            .await
            .unwrap();
        let page: Value = serde_json::from_str(page.as_text().unwrap()).unwrap();
        assert_eq!(page, json!({"skills":[],"warnings":[],"next_cursor":null}));
    }
    assert_eq!(
        *provider.queries.lock().unwrap(),
        ["run-a", "run-b", "run-c", "run-a"]
    );
}

struct DelayedCatalogProvider {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    queries: std::sync::Mutex<Vec<String>>,
}
impl SkillProvider for DelayedCatalogProvider {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            self.queries.lock().unwrap().push(query.turn_id.clone());
            if query.turn_id == "run-a" {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(SkillCatalog {
                entries: vec![],
                warnings: vec![query.turn_id],
            })
        })
    }
    fn read<'a>(&'a self, _: SkillReadRequest<'a>) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async { Err(SkillProviderError::new("unused read")) })
    }
}

#[tokio::test]
async fn an_in_flight_old_run_cannot_replace_the_new_runs_catalog() {
    let provider = Arc::new(DelayedCatalogProvider {
        started: Default::default(),
        release: Default::default(),
        queries: Default::default(),
    });
    let directory = tempfile::tempdir().unwrap();
    let pair = skill_tools(
        SkillProviders::new().with_executor_provider(provider.clone()),
        roots(
            "root",
            ra_exec::fs::Workspace::open(directory.path()).unwrap(),
        ),
        1000,
    )
    .unwrap();
    let first_tool = pair[0].clone();
    let first = tokio::spawn(async move {
        call_in_run(
            first_tool.as_ref(),
            json!({"authority":{"kind":"executor"}}),
            "run-a",
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.started.notified(),
    )
    .await
    .unwrap();
    let args = json!({"authority":{"kind":"executor"}});
    let second = call_in_run(pair[0].as_ref(), args.clone(), "run-b")
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(second.as_text().unwrap()).unwrap()["warnings"],
        json!(["run-b"])
    );
    provider.release.notify_one();
    let first = first.await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(first.as_text().unwrap()).unwrap()["warnings"],
        json!(["run-a"])
    );
    let repeated = call_in_run(pair[0].as_ref(), args, "run-b").await.unwrap();
    assert_eq!(repeated, second);
    assert_eq!(*provider.queries.lock().unwrap(), ["run-a", "run-b"]);
}

#[test]
fn unconfigured_or_unsupported_sources_do_not_register_skills_tools() {
    use ra_core::capability::Capability;
    use ra_tools::capability::SkillsNsCapability;
    let directory = tempfile::tempdir().unwrap();
    let workspace = ra_exec::fs::Workspace::open(directory.path()).unwrap();
    for providers in [
        SkillProviders::new(),
        SkillProviders::new().with_provider(SkillProviderSource::new(
            SkillSourceKind::Host,
            "host",
            Arc::new(StaticProvider {
                catalog: Default::default(),
                contents: String::new(),
            }),
        )),
        SkillProviders::new().with_provider(SkillProviderSource::new(
            SkillSourceKind::custom("private"),
            "private",
            Arc::new(StaticProvider {
                catalog: Default::default(),
                contents: String::new(),
            }),
        )),
    ] {
        assert!(!providers.has_executor_provider());
        assert!(!providers.has_cloud_provider());
        for selected_roots in [BTreeMap::new(), roots("root", workspace.clone())] {
            assert!(
                skill_tools(providers.clone(), selected_roots.clone(), 1000)
                    .unwrap()
                    .is_empty()
            );
            let capability =
                SkillsNsCapability::new(providers.clone(), selected_roots, 1000).unwrap();
            assert!(capability.tools().is_empty());
            assert!(capability.list().is_none());
            assert!(capability.read().is_none());
        }
    }
}

#[test]
fn executor_provider_without_roots_does_not_register_skills_tools() {
    use ra_core::capability::Capability;
    use ra_tools::capability::SkillsNsCapability;
    let providers = executor_source();
    assert!(providers.has_executor_provider());
    assert!(tools(providers.clone(), 1000).is_empty());
    let capability = SkillsNsCapability::new(providers, BTreeMap::new(), 1000).unwrap();
    assert!(capability.tools().is_empty());
    assert!(capability.list().is_none());
    assert!(capability.read().is_none());
}

#[tokio::test]
async fn cloud_without_executor_roots_does_not_query_the_executor_provider() {
    let executor = Arc::new(UnavailableProvider(std::sync::atomic::AtomicUsize::new(0)));
    let providers = cloud(
        SkillCatalog {
            entries: vec![entry(
                SkillSourceKind::Cloud,
                CLOUD_AUTHORITY_ID,
                "pkg",
                "pkg.md",
            )],
            warnings: vec![],
        },
        "cloud instructions",
    )
    .with_executor_provider(executor.clone());
    let pair = tools(providers, 1000);
    assert_eq!(pair.len(), 2);
    let listed = response(pair[0].as_ref(), json!({"authority":{"kind":"cloud"}})).await;
    assert_eq!(listed["skills"][0]["package"], "pkg");
    assert_eq!(
        response(pair[1].as_ref(), json!({"package":"pkg"})).await["contents"],
        "cloud instructions"
    );
    assert_eq!(
        response(pair[0].as_ref(), json!({"authority":{"kind":"executor"}})).await,
        json!({"skills":[],"warnings":[],"next_cursor":null})
    );
    assert_eq!(
        error_message(
            call(pair[1].as_ref(), json!({"package":"skill://root/pkg"}))
                .await
                .unwrap_err()
        ),
        "skill package is not available"
    );
    assert_eq!(executor.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cloud_providers_use_the_documented_authority_identity() {
    let providers = cloud(
        SkillCatalog {
            entries: vec![
                entry(
                    SkillSourceKind::Cloud,
                    CLOUD_AUTHORITY_ID,
                    "available",
                    "available.md",
                ),
                entry(
                    SkillSourceKind::Cloud,
                    "other-cloud",
                    "unavailable",
                    "unavailable.md",
                ),
            ],
            warnings: vec![],
        },
        "cloud contents",
    );
    assert!(providers.has_cloud_provider());
    let pair = tools(providers, 1000);
    let listed = response(pair[0].as_ref(), json!({"authority":{"kind":"cloud"}})).await;
    assert_eq!(listed["skills"].as_array().unwrap().len(), 1);
    assert_eq!(listed["skills"][0]["package"], "available");
    assert_eq!(
        response(pair[1].as_ref(), json!({"package":"available"})).await["contents"],
        "cloud contents"
    );
    assert_eq!(
        error_message(
            call(pair[1].as_ref(), json!({"package":"unavailable"}))
                .await
                .unwrap_err()
        ),
        "skill package is not available"
    );
}

struct RootQueryProvider {
    queries: std::sync::Mutex<Vec<SkillListQuery>>,
}
impl SkillProvider for RootQueryProvider {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        self.queries.lock().unwrap().push(query);
        Box::pin(async {
            Ok(SkillCatalog {
                entries: ["selected", "unselected"]
                    .into_iter()
                    .map(|authority| {
                        entry(
                            SkillSourceKind::Executor,
                            authority,
                            &format!("skill://{authority}/review"),
                            "resource",
                        )
                    })
                    .collect(),
                warnings: vec![],
            })
        })
    }
    fn read<'a>(&'a self, _: SkillReadRequest<'a>) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async { Err(SkillProviderError::new("not needed")) })
    }
}

#[tokio::test]
async fn executor_queries_and_all_discovery_consumers_are_limited_to_selected_roots() {
    use ra_core::capability::Capability;
    let directory = tempfile::tempdir().unwrap();
    let workspace = ra_exec::fs::Workspace::open(directory.path()).unwrap();
    let provider = Arc::new(RootQueryProvider {
        queries: Default::default(),
    });
    let capability = ra_tools::capability::SkillsNsCapability::new(
        SkillProviders::new().with_executor_provider(provider.clone()),
        roots("selected", workspace.clone()),
        8000,
    )
    .unwrap();
    let bound = capability.bind(&run()).unwrap().unwrap();
    let prompt = bound.instructions().await.unwrap().unwrap();
    assert!(prompt.content().contains("skill://selected/review"));
    assert!(!prompt.content().contains("unselected"));
    let list = response(
        capability.list().unwrap().as_ref(),
        json!({"authority":{"kind":"executor"}}),
    )
    .await;
    assert_eq!(list["skills"].as_array().unwrap().len(), 1);
    assert_eq!(list["skills"][0]["package"], "skill://selected/review");
    let denied = call(
        capability.read().unwrap().as_ref(),
        json!({"package":"skill://unselected/review"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error_message(denied), "skill package is not available");
    let queries = provider.queries.lock().unwrap();
    assert_eq!(queries.len(), 1);
    assert_eq!(
        queries[0].executor_roots,
        [("selected".into(), workspace.root().to_path_buf())].into()
    );
}
