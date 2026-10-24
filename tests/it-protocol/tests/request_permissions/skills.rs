// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Executor reference reads under captured thread permissions and real client grants.

use super::*;
use ra_coding::{
    CodingProfile, host_backed_surface,
    permissions::{FileSystemAccessMode as Access, FileSystemPath, FileSystemSandboxEntry},
};
use ra_core::prompt::PromptRole;
use ra_exec::fs::Workspace;
use ra_tools::skills_ns::{ExecutorSkillProvider, SkillProviders, catalog::*};

#[allow(dead_code)]
#[path = "../../../support/configured_coding_host.rs"]
mod configured_host;

struct AccessOnlyProvider(ExecutorSkillProvider);

impl ra_tools::skills_ns::SkillProvider for AccessOnlyProvider {
    fn list(
        &self,
        query: ra_tools::skills_ns::provider::SkillListQuery,
    ) -> ra_tools::skills_ns::provider::SkillProviderFuture<'_, SkillCatalog> {
        self.0.list(query)
    }
    fn read<'a>(
        &'a self,
        request: ra_tools::skills_ns::provider::SkillReadRequest<'a>,
    ) -> ra_tools::skills_ns::provider::SkillProviderFuture<'a, SkillReadResult> {
        use ra_tools::skills_ns::provider::SkillReadContext;
        assert!(matches!(request.context, SkillReadContext::Executor { .. }));
        self.0.read(request)
    }
}

fn policy(entries: &[(&Path, Access)]) -> FileSystemPermissions {
    FileSystemPermissions::new(
        entries
            .iter()
            .map(|(path, access)| {
                FileSystemSandboxEntry::new(
                    FileSystemPath::Path {
                        path: path.to_path_buf(),
                    },
                    *access,
                )
            })
            .collect(),
    )
}

struct Fixture {
    package: String,
    resource: String,
    file: PathBuf,
    main: String,
    agent: Arc<AgentSpec>,
    config: RunConfig,
    adapter: Arc<CodingThreadSettingsAdapter>,
    thread: AgentThread,
    host: CodingHost,
}

impl Fixture {
    async fn load(
        server: &Server,
        connection: &Connection,
        script: &Arc<Script>,
        baseline: FileSystemPermissions,
    ) -> Self {
        Self::load_with_main(server, connection, script, baseline, true).await
    }

    async fn load_with_main(
        server: &Server,
        connection: &Connection,
        script: &Arc<Script>,
        baseline: FileSystemPermissions,
        materialized: bool,
    ) -> Self {
        let root = server.root();
        let directory = root.join("skill");
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join("reference.md");
        std::fs::write(&file, "live reference").unwrap();
        std::fs::write(directory.join("SKILL.md"), "live main").unwrap();
        let package = "skill://selected/skill".to_string();
        let resource = format!("{package}/reference.md");
        let main = if materialized {
            "materialized main prompt"
        } else {
            "live main"
        }
        .to_string();
        let main_resource = if materialized {
            SkillResourceId::environment_with_contents(
                format!("{package}/SKILL.md"),
                "local",
                directory.join("SKILL.md"),
                main.clone(),
            )
        } else {
            SkillResourceId::environment(
                format!("{package}/SKILL.md"),
                "local",
                directory.join("SKILL.md"),
            )
        };
        let entry = SkillCatalogEntry::new(
            SkillPackageId(package.clone()),
            SkillAuthority::new(SkillSourceKind::Executor, "selected"),
            "reference",
            "Read references",
            main_resource,
        );
        let services = Arc::new(configured_host::Services::default());
        let host = CodingHost::open(&root)
            .unwrap()
            .with_web_access(services.clone())
            .with_user_input(services)
            .with_collaboration()
            .with_skill_providers(
                SkillProviders::new().with_executor_provider(Arc::new(AccessOnlyProvider(
                    ExecutorSkillProvider::new(SkillCatalog {
                        entries: vec![entry],
                        warnings: vec![],
                    }),
                ))),
                [("selected".to_string(), Workspace::open(&root).unwrap())].into(),
            );
        let surface = host_backed_surface(&PromptRole::Main, &host, CodingProfile::CodexLike)
            .await
            .unwrap();
        let legacy = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        let adapter = CodingThreadSettingsAdapter::new(&root, legacy.clone())
            .unwrap()
            .with_file_system_read_policy(baseline)
            .unwrap()
            .with_profile("denied", legacy.clone())
            .unwrap()
            .with_file_system_read_profile(
                "denied",
                policy(&[(&root, Access::Read), (&file, Access::Deny)]),
            )
            .unwrap()
            .with_profile("changed", legacy)
            .unwrap()
            .with_file_system_read_profile(
                "changed",
                policy(&[(&root, Access::Read), (&file, Access::Read)]),
            )
            .unwrap();
        let config = adapter
            .prepare(
                &adapter.initialize(surface.configure_run(host.build_run_config())),
                &ThreadPolicyOverrides::new().with_approval_policy(on_request()),
            )
            .unwrap();
        let adapter = adapter
            .with_profile(
                "blocked",
                SandboxPolicy::ReadOnly {
                    network_access: false,
                },
            )
            .unwrap()
            .with_file_system_read_profile("blocked", FileSystemPermissions::default())
            .unwrap();
        let adapter = Arc::new(adapter);
        let mut tools = surface.tool_surface().tools().to_vec();
        tools.push(host.request_permissions_tool().unwrap());
        let agent = AgentSpec::builder()
            .id(AgentId::new("coder"))
            .name("coder")
            .instructions("Read skills")
            .tools(tools)
            .build()
            .unwrap();
        let recorder: Arc<dyn RolloutRecorder> = server
            .store
            .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
                SessionId::new(THREAD),
            )))
            .await
            .unwrap();
        let thread = AgentThread::builder(
            SessionId::new(THREAD),
            AgentBinding::direct(agent.clone()),
            Arc::new(Resolver(script.clone())) as Arc<dyn ModelResolver>,
        )
        .with_config(config.clone())
        .with_services(host.build_tool_services())
        .with_rollout_recorder(recorder)
        .build();
        server
            .loader
            .load_with_adapter(
                thread.clone(),
                &[connection.id],
                Some(adapter.clone() as Arc<dyn ThreadSettingsAdapter>),
            )
            .await;
        Self {
            package,
            resource,
            file,
            main,
            agent,
            config,
            adapter,
            thread,
            host,
        }
    }

    fn read(&self, id: &str, cursor: Option<&str>) -> ModelResponse {
        let mut args = json!({"package": self.package, "resource": self.resource});
        if let Some(cursor) = cursor {
            args["cursor"] = json!(cursor);
        }
        skill_call(id, args)
    }
}

fn skill_call(id: &str, arguments: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("item-{id}")),
        RunItemKind::ToolCall(
            ToolCall::new(CallId::new(id), "read", arguments).with_namespace("skills"),
        ),
    )])
}

#[tokio::test]
async fn executor_references_are_denied_but_discovery_materializations_remain_readable() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let fixture = Fixture::load(
        &server,
        &connection,
        &script,
        FileSystemPermissions::default(),
    )
    .await;
    script.push(skill_call("main", json!({"package": fixture.package})));
    script.push(fixture.read("reference", None));
    script.push(final_message("done"));
    server.turn_start(&mut connection, "$reference").await;
    connection.notification("turn/completed").await;
    assert_eq!(
        script.last_output("main").unwrap()["contents"],
        fixture.main
    );
    assert_eq!(
        script.last_output("reference"),
        Some(json!("failed to read skill resource"))
    );
    assert!(script.inputs.lock().unwrap().iter().flatten().any(|item| {
        matches!(item, ModelInputItem::Message(message) if message.text_content().contains(&fixture.main))
    }));
}

#[tokio::test]
async fn executor_reference_grants_apply_to_later_callbacks_in_the_same_step_and_keep_their_scope()
{
    for scope in ["turn", "session"] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        let fixture = Fixture::load(
            &server,
            &connection,
            &script,
            FileSystemPermissions::default(),
        )
        .await;
        let original = "granted reference line\n".repeat(8000);
        std::fs::write(&fixture.file, &original).unwrap();
        script.push(fixture.read("before", None));
        let args = json!({"permissions": {"file_system": {"read": [fixture.file]}}});
        let mut batch = request_permissions("grant", args).output().to_vec();
        batch.extend(fixture.read("same-step", None).output().iter().cloned());
        script.push(ModelResponse::new(batch));
        script.push(fixture.read("after", None));
        script.push(final_message("done"));
        server
            .turn_start(&mut connection, "read the reference")
            .await;
        let request = connection.permissions_request().await;
        server
            .answer(
                &connection,
                &request,
                json!({"permissions": {"fileSystem": {"read": [fixture.file]}}, "scope": scope}),
            )
            .await;
        connection.notification("turn/completed").await;
        assert_eq!(
            script.last_output("before"),
            Some(json!("failed to read skill resource"))
        );
        let same_step = script.last_output("same-step").unwrap();
        let same_step_text = same_step["contents"].as_str().unwrap();
        assert!(!same_step_text.is_empty());
        assert!(original.starts_with(same_step_text));
        let page = script.last_output("after").unwrap();
        let cursor = page["next_cursor"].as_str().unwrap();
        let prefix = page["contents"].as_str().unwrap();
        assert!(!prefix.is_empty());
        assert!(original.starts_with(prefix));
        std::fs::write(&fixture.file, "changed after grant").unwrap();
        script.push(fixture.read("later-turn", Some(cursor)));
        script.push(final_message("later"));
        server.turn_start(&mut connection, "read again").await;
        connection.notification("turn/completed").await;
        let output = script.last_output("later-turn").unwrap();
        if scope == "session" {
            let part = output["contents"].as_str().unwrap();
            assert!(!part.is_empty());
            assert!(original[prefix.len()..].starts_with(part));
        } else {
            assert_eq!(output, "failed to read skill resource");
        }
    }
}

#[tokio::test]
async fn executor_continuations_preserve_edits_only_under_the_same_permissions() {
    for change in ["unchanged", "edited", "denied", "changed", "network"] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        let fixture = Fixture::load(
            &server,
            &connection,
            &script,
            policy(&[(&server.root(), Access::Read)]),
        )
        .await;
        let original = "reference line\n".repeat(8000);
        std::fs::write(&fixture.file, &original).unwrap();
        script.push(fixture.read("first", None));
        script.push(final_message("first done"));
        server.turn_start(&mut connection, "read first page").await;
        connection.notification("turn/completed").await;
        let first = script.last_output("first").unwrap();
        let cursor = first["next_cursor"].as_str().unwrap();
        let first_length = first["contents"].as_str().unwrap().len();
        if change != "unchanged" {
            std::fs::write(&fixture.file, "edited reference").unwrap();
        }
        if matches!(change, "denied" | "changed") {
            let id = server
                .request(
                    &mut connection,
                    "thread/settings/update",
                    json!({"threadId": THREAD, "permissions": change}),
                )
                .await;
            connection.response(id).await;
        }
        if change == "network" {
            script.push(request_permissions(
                "network",
                json!({"permissions": {"network": {"enabled": true}}}),
            ));
        }
        script.push(fixture.read("continue", Some(cursor)));
        script.push(final_message("continued"));
        server
            .turn_start(&mut connection, "continue reference")
            .await;
        if change == "network" {
            let request = connection.permissions_request().await;
            server
                .answer(
                    &connection,
                    &request,
                    json!({"permissions": {"network": {"enabled": true}}, "scope": "session"}),
                )
                .await;
        }
        connection.notification("turn/completed").await;
        let output = script.last_output("continue").unwrap();
        if change == "denied" {
            assert_eq!(output, "failed to read skill resource");
        } else if matches!(change, "changed" | "network") {
            assert_eq!(
                output,
                "skills.read cursor is stale; restart from the first page"
            );
        } else {
            let part = output["contents"].as_str().unwrap();
            assert!(!part.is_empty());
            assert!(original[first_length..].starts_with(part));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn executor_reads_check_the_opened_symlink_target() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let secret = server.root().join("secret.md");
    std::fs::write(&secret, "private contents").unwrap();
    let fixture = Fixture::load(
        &server,
        &connection,
        &script,
        policy(&[(&server.root(), Access::Read), (&secret, Access::Deny)]),
    )
    .await;
    std::fs::remove_file(&fixture.file).unwrap();
    std::os::unix::fs::symlink("../secret.md", &fixture.file).unwrap();
    script.push(fixture.read("link", None));
    script.push(final_message("done"));
    server.turn_start(&mut connection, "read reference").await;
    connection.notification("turn/completed").await;
    assert_eq!(
        script.last_output("link"),
        Some(json!("failed to read skill resource"))
    );
}

#[tokio::test]
async fn executor_live_main_prompts_use_the_same_permissions_for_injection_and_tool_reads() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let fixture = Fixture::load_with_main(
        &server,
        &connection,
        &script,
        FileSystemPermissions::default(),
        false,
    )
    .await;
    script.push(skill_call("main", json!({"package": fixture.package})));
    script.push(final_message("done"));
    server.turn_start(&mut connection, "$reference").await;
    connection.notification("turn/completed").await;
    assert_eq!(
        script.last_output("main"),
        Some(json!("failed to read skill resource"))
    );
    assert!(!script.inputs.lock().unwrap().iter().flatten().any(|item| {
        matches!(item, ModelInputItem::Message(message) if message.text_content().contains(&fixture.main))
    }));
}

#[tokio::test]
async fn active_turn_settings_keep_the_current_step_baseline_and_restrict_the_next_step() {
    use ra_runtime::thread::{
        SteerSubmission, ThreadSettingsOverrides, TurnInputRequest, TurnSettingsUpdateOutcome,
    };
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let fixture = Fixture::load_with_main(
        &server,
        &connection,
        &script,
        policy(&[(&server.root(), Access::Read)]),
        false,
    )
    .await;
    std::fs::write(&fixture.file, "reference line\n".repeat(8000)).unwrap();
    script.push(fixture.read("first", None));
    let mut batch = request_permissions(
        "barrier",
        json!({"permissions": {"network": {"enabled": true}}}),
    )
    .output()
    .to_vec();
    batch.extend(fixture.read("old-step", None).output().iter().cloned());
    script.push(ModelResponse::new(batch));
    let turn_id = server
        .turn_start(&mut connection, "read the reference")
        .await;
    let request = connection.permissions_request().await;
    let first = script.last_output("first").unwrap();
    let cursor = first["next_cursor"].as_str().unwrap();
    let changed = fixture
        .adapter
        .prepare(
            &fixture.config,
            &ThreadPolicyOverrides::new().with_permissions("blocked"),
        )
        .unwrap();
    assert_eq!(
        fixture.thread.update_turn_settings(
            &turn_id,
            &ThreadSettingsOverrides::new().with_config(changed)
        ),
        TurnSettingsUpdateOutcome::Applied
    );
    assert!(matches!(
        fixture.thread.steer_turn(
            &TurnInputRequest::new(vec![ra_core::item::ContentBlock::text("$reference")]),
            &turn_id
        ),
        SteerSubmission::Steered { .. }
    ));
    script.push(fixture.read("next-step", Some(cursor)));
    script.push(final_message("done"));
    server
        .answer(&connection, &request, nothing_granted())
        .await;
    connection.notification("turn/completed").await;
    assert!(
        script.last_output("old-step").unwrap()["contents"]
            .as_str()
            .unwrap()
            .starts_with("reference line")
    );
    assert_eq!(
        script.last_output("next-step"),
        Some(json!("failed to read skill resource"))
    );
    assert!(!script.inputs.lock().unwrap().iter().flatten().any(|item| {
        matches!(item, ModelInputItem::Message(message) if message.text_content().contains(&fixture.main))
    }));
}

#[tokio::test]
async fn caller_managed_continuations_read_live_grants_without_context_processors() {
    use ra_core::{cancel::CancelScope, state::RunState};
    use ra_runtime::runner::{RunRequest, Runner};
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let fixture = Fixture::load(
        &server,
        &connection,
        &script,
        FileSystemPermissions::default(),
    )
    .await;
    script.push(request_permissions(
        "hold",
        json!({"permissions": {"network": {"enabled": true}}}),
    ));
    script.push(final_message("held done"));
    let turn_id = server.turn_start(&mut connection, "hold the turn").await;
    let hold = connection.permissions_request().await;
    let script = Arc::new(Script::default());
    script.push(fixture.read("before", None));
    script.push(request_permissions(
        "grant",
        json!({"permissions": {"file_system": {"read": [fixture.file]}}}),
    ));
    script.push(fixture.read("after", None));
    script.push(final_message("done"));
    let run_id = RunId::new(turn_id);
    let mut state = RunState::start(run_id.clone());
    state
        .begin_segment(
            fixture.agent.id().clone(),
            vec![ModelInputItem::Message(Message::user("previous input"))],
        )
        .unwrap();
    let request = RunRequest::new(
        AgentBinding::direct(fixture.agent.clone()),
        Arc::new(Resolver(script.clone())),
        run_id,
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user(
            "caller continuation",
        ))],
    )
    .with_state(state)
    .with_config(fixture.config.clone())
    .with_services(fixture.host.build_tool_services());
    let mut running = tokio::spawn(Runner::run(request));
    let approval = tokio::select! {
        approval = connection.permissions_request() => approval,
        outcome = &mut running => panic!("caller continuation finished before requesting permissions (ok={}): {:?}", outcome.is_ok(), script.last_output("grant")),
    };
    server
        .answer(
            &connection,
            &approval,
            json!({"permissions": {"fileSystem": {"read": [fixture.file]}}, "scope": "turn"}),
        )
        .await;
    let outcome = timeout(PATIENCE, running).await.unwrap().unwrap().unwrap();
    assert!(!outcome.state().input_history_is_complete());
    assert_eq!(
        script.last_output("before"),
        Some(json!("failed to read skill resource"))
    );
    assert_eq!(
        script.last_output("after").unwrap()["contents"],
        "live reference"
    );
    server.answer(&connection, &hold, nothing_granted()).await;
    connection.notification("turn/completed").await;
}

#[cfg(unix)]
#[tokio::test]
async fn nested_policy_symlinks_do_not_expand_allow_or_deny_to_the_target() {
    for kind in ["allow", "deny", "deny-glob"] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        let alias = server.root().join("alias");
        let baseline = if kind == "deny-glob" {
            let mut entries = policy(&[(&server.root(), Access::Read)]).entries().to_vec();
            entries.push(FileSystemSandboxEntry::new(
                FileSystemPath::GlobPattern {
                    pattern: format!("{}/**", alias.display()),
                },
                Access::Deny,
            ));
            FileSystemPermissions::new(entries)
        } else if kind == "deny" {
            policy(&[(&server.root(), Access::Read), (&alias, Access::Deny)])
        } else {
            policy(&[(&alias, Access::Read)])
        };
        let fixture = Fixture::load(&server, &connection, &script, baseline).await;
        std::os::unix::fs::symlink("skill", &alias).unwrap();
        script.push(fixture.read("reference", None));
        script.push(final_message("done"));
        server.turn_start(&mut connection, "read reference").await;
        connection.notification("turn/completed").await;
        let output = script.last_output("reference").unwrap();
        if kind != "allow" {
            assert_eq!(output["contents"], "live reference");
        } else {
            assert_eq!(output, "failed to read skill resource");
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_tmp_alias_and_tmpdir_denies_apply_to_canonical_skill_roots() {
    use ra_coding::permissions::FileSystemSpecialPath;
    for case in ["tmp-deny", "tmpdir-deny", "tmp-allow"] {
        let server = if case == "tmpdir-deny" {
            Server::start()
        } else {
            Server::with_workspace(tempfile::tempdir_in("/tmp").unwrap())
        };
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        let raw = server._workspace.path().join("skill");
        let mut entries = if case == "tmp-allow" {
            policy(&[(&raw, Access::Read)]).entries().to_vec()
        } else {
            policy(&[(&server.root(), Access::Read)]).entries().to_vec()
        };
        if case == "tmp-deny" {
            assert_ne!(server._workspace.path(), server.root());
            entries.extend(policy(&[(&raw, Access::Deny)]).entries().iter().cloned());
        } else if case == "tmpdir-deny" {
            assert!(std::env::var_os("TMPDIR").is_some());
            entries.push(FileSystemSandboxEntry::new(
                FileSystemPath::Special {
                    value: FileSystemSpecialPath::Tmpdir,
                },
                Access::Deny,
            ));
        }
        let fixture = Fixture::load(
            &server,
            &connection,
            &script,
            FileSystemPermissions::new(entries),
        )
        .await;
        script.push(fixture.read("reference", None));
        script.push(final_message("done"));
        server.turn_start(&mut connection, "read reference").await;
        connection.notification("turn/completed").await;
        let output = script.last_output("reference").unwrap();
        if case == "tmp-allow" {
            assert_eq!(output["contents"], "live reference");
        } else {
            assert_eq!(output, "failed to read skill resource");
        }
    }
}

#[test]
fn skill_read_profiles_reject_invalid_entries_during_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let legacy = SandboxPolicy::ReadOnly {
        network_access: false,
    };
    for invalid in [
        FileSystemSandboxEntry::new(
            FileSystemPath::Path {
                path: "relative".into(),
            },
            Access::Read,
        ),
        FileSystemSandboxEntry::new(
            FileSystemPath::GlobPattern {
                pattern: "**/*.md".into(),
            },
            Access::Read,
        ),
    ] {
        let invalid = FileSystemPermissions::new(vec![invalid]);
        let adapter = CodingThreadSettingsAdapter::new(directory.path(), legacy.clone()).unwrap();
        assert!(
            adapter
                .clone()
                .with_file_system_read_policy(invalid.clone())
                .is_err()
        );
        assert!(
            adapter
                .with_profile("invalid", legacy.clone())
                .unwrap()
                .with_file_system_read_profile("invalid", invalid)
                .is_err()
        );
    }
}
