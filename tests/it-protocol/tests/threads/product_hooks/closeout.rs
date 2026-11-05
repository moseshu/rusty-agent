//! Product hook acceptance through the configured coding factory and protocol.
use super::*;
use ra_core::capability::{
    ContextProcessor, ContextProcessorRequest, ContextProcessorResult, ContextSummarizer,
};
use ra_core::hook::CompactTrigger;
use ra_core::item::Compaction;
use std::sync::atomic::{AtomicUsize, Ordering};

fn configured_server(home: &HooksHome, capability: Arc<dyn Capability>) -> Server {
    let root = home.workspace.path().canonicalize().unwrap();
    let host = CodingHost::open(&root).unwrap().with_capability(capability);
    let settings = CodingThreadSettingsAdapter::new(
        &root,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap()
    .with_rules_home(home.home.path());
    let factory = CodingThreadFactory::new(host, Arc::new(Resolver(home.script.clone())), settings)
        .with_product_config(home.config_builder());
    build(
        Options {
            store: Some(home.store.clone()),
            factory: Some(Arc::new(factory)),
            unload_delay: Some(Duration::ZERO),
        },
        None,
    )
    .0
}

struct OnceCompactor {
    fire_on: usize,
    calls: AtomicUsize,
}
impl OnceCompactor {
    fn new(fire_on: usize) -> Arc<Self> {
        Arc::new(Self {
            fire_on,
            calls: AtomicUsize::new(0),
        })
    }
}
#[async_trait]
impl Capability for OnceCompactor {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("acceptance_compaction").unwrap()
    }
    fn context_processor(&self) -> Option<&dyn ContextProcessor> {
        Some(self)
    }
}
#[async_trait]
impl ContextProcessor for OnceCompactor {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        _: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 != self.fire_on {
            return Ok(ContextProcessorResult::new(request.input().to_vec()));
        }
        request
            .notify_pre_compact(CompactTrigger::Automatic)
            .await?;
        let compaction = Compaction::new("accepted compacted summary", vec![]);
        request
            .notify_post_compact(CompactTrigger::Automatic, &compaction)
            .await?;
        Ok(
            ContextProcessorResult::new(request.input().to_vec()).with_generated_items(vec![
                RunItem::new(
                    request.record_id().clone(),
                    RunItemKind::Compaction(compaction),
                ),
            ]),
        )
    }
}
fn command(cmd: &str, escalated: bool) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("command"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("command"),
            "exec_command",
            json!({"cmd":cmd,"justification":escalated.then_some("test approval"),"prefix_rule":null,"sandbox_permissions":escalated.then_some("require_escalated"),"workdir":null,"shell":"/bin/sh","tty":null,"login":false,"yield_time_ms":1000,"timeout_ms":null}),
        )),
    )])
}
fn request_text(script: &Script) -> String {
    serde_json::to_string(script.last_request().input()).unwrap()
}
async fn wait_file(path: &Path) {
    timeout(PATIENCE, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn permission_request_deny_prevents_execution_and_client_approval() {
    let home = HooksHome::new();
    let marker = home.path("never-executed");
    let log = home.path("permission.jsonl");
    home.write_hooks_with_approval("on-request", &hook_section("PermissionRequest", &format!("{}; printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PermissionRequest\",\"decision\":{{\"behavior\":\"deny\",\"message\":\"default factory denied\"}}}}}}'", logging(&log))));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.push(Step::Respond(command(
        &format!("touch '{}'", marker.display()),
        true,
    )));
    home.script.answer("denied");
    server
        .run_turn(&mut connection, &thread, "try command")
        .await;
    assert!(!marker.exists());
    assert_eq!(events_of(&logged(&log)), ["PermissionRequest"]);
    assert!(request_text(&home.script).contains("default factory denied"));
    assert!(
        !connection
            .passed
            .iter()
            .any(|m| m["method"] == "item/commandExecution/requestApproval")
    );
}

#[tokio::test]
async fn post_compact_stop_keeps_records_and_defers_compact_session_start_to_next_turn() {
    let home = HooksHome::new();
    let log = home.path("compaction.jsonl");
    home.write_hooks(
        &[
            hook_section("SessionStart", &logging(&log)),
            hook_section("PreCompact", &logging(&log)),
            hook_section(
                "PostCompact",
                &format!("{}; printf '%s' '{{\"continue\":false}}'", logging(&log)),
            ),
            hook_section("Interrupt", &logging(&log)),
        ]
        .join("\n"),
    );
    let server = configured_server(&home, OnceCompactor::new(1));
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    server
        .turn_start(&mut connection, &thread, "compact me")
        .await;
    let stopped = connection.notification("turn/completed").await;
    assert_eq!(stopped["turn"]["status"], "interrupted");
    assert!(home.script.requests.lock().unwrap().is_empty());
    assert_eq!(
        events_of(&logged(&log)),
        ["SessionStart", "PreCompact", "PostCompact", "Interrupt"]
    );
    assert!(!connection.passed.iter().any(|m| m["method"] == "error"));
    home.script.answer("continued");
    server.run_turn(&mut connection, &thread, "continue").await;
    let inputs = logged(&log);
    assert_eq!(
        events_of(&inputs),
        [
            "SessionStart",
            "PreCompact",
            "PostCompact",
            "Interrupt",
            "SessionStart"
        ]
    );
    assert_eq!(inputs.last().unwrap()["source"], "compact");
    assert!(
        home.script
            .last_request()
            .input()
            .iter()
            .any(|i| matches!(i, ModelInputItem::Compaction(_)))
    );
    let stored = home
        .store
        .read_thread(&ReadThreadParams::new(SessionId::new(&thread)).with_history())
        .await
        .unwrap();
    let serialized = serde_json::to_string(stored.history().unwrap().records()).unwrap();
    assert!(
        serialized.contains("accepted compacted summary"),
        "{serialized}"
    );
    assert!(serialized.contains("compact me"), "{serialized}");
    assert!(!serialized.contains("hook/started") && !serialized.contains("hook/completed"));
    assert!(!serialized.contains("<turn_aborted>"));
}

#[tokio::test]
async fn required_managed_hook_failure_rejects_cold_resume_and_fork() {
    let home = HooksHome::new();
    home.write_hooks("");
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.answer("stored");
    server.run_turn(&mut connection, &thread, "remember").await;
    super::super::unload(&server, &mut connection, &thread).await;
    home.write_requirements(&format!("[hooks]\nmanaged_dir = '{}'\n{}", home.home.path().display(), "[[hooks.PreToolUse]]\nmatcher = '['\n[[hooks.PreToolUse.hooks]]\ntype = 'command'\ncommand = 'true'"));
    let cold = home.server();
    let mut client = cold.connect(2).await;
    for method in ["thread/resume", "thread/fork"] {
        let error = cold
            .call_error(&mut client, method, json!({"threadId":thread}))
            .await;
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("failed to load required managed hooks"),
            "{method}: {error}"
        );
    }
}

fn background(home: &HooksHome) -> (PathBuf, PathBuf) {
    let ready = home.path("background-ready");
    let release = home.path("background-release");
    home.write_hooks(&format!("{}async = true\n", hook_section("UserPromptSubmit", &format!("cat >/dev/null; touch '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; printf '%s' '{{\"systemMessage\":\"accepted background warning\",\"hookSpecificOutput\":{{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"accepted background context\"}}}}'; touch '{}'", ready.display(), release.display(), home.path("background-finished").display()))));
    (ready, release)
}

#[tokio::test]
async fn background_context_enters_active_model_input_without_lifecycle_notifications() {
    let home = HooksHome::new();
    let (ready, release) = background(&home);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let gate = Arc::new(Notify::new());
    home.script.push(Step::After(
        gate.clone(),
        command("echo continue-background", false),
    ));
    home.script.answer("with background");
    server
        .turn_start(&mut connection, &thread, "active prompt")
        .await;
    wait_file(&ready).await;
    home.script.wait_for_calls(1).await;
    std::fs::write(release, "go").unwrap();
    wait_file(&home.path("background-finished")).await;
    gate.notify_one();
    let warning = connection.notification("warning").await;
    assert!(warning.to_string().contains("accepted background warning"));
    connection.notification("turn/completed").await;
    home.script.wait_for_calls(2).await;
    let text = request_text(&home.script);
    assert!(text.contains("accepted background context"), "{text}");
    assert!(
        !connection
            .passed
            .iter()
            .any(|m| m["method"].as_str().is_some_and(|s| s.starts_with("hook/")))
    );
}

#[tokio::test]
async fn idle_background_context_precedes_the_next_prompt() {
    let home = HooksHome::new();
    let (ready, release) = background(&home);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.answer("first finished");
    server
        .run_turn(&mut connection, &thread, "first prompt")
        .await;
    wait_file(&ready).await;
    let before = connection.drain().await;
    assert!(
        !before
            .iter()
            .any(|m| m["method"].as_str().is_some_and(|s| s.starts_with("hook/")))
    );
    std::fs::write(release, "go").unwrap();
    wait_file(&home.path("background-finished")).await;
    // Codex's idle-result case observes a quiet event window: neither warnings nor another
    // model turn may be emitted until a new prompt arrives.
    assert!(
        timeout(Duration::from_millis(150), connection.written.recv())
            .await
            .is_err()
    );
    assert_eq!(home.script.requests.lock().unwrap().len(), 1);
    home.script.answer("next finished");
    server
        .run_turn(&mut connection, &thread, "next prompt")
        .await;
    let input = request_text(&home.script);
    assert!(
        input.find("accepted background context").unwrap() < input.find("next prompt").unwrap(),
        "{input}"
    );
    assert!(
        !connection
            .passed
            .iter()
            .any(|m| m["method"].as_str().is_some_and(|s| s.starts_with("hook/")))
    );
}

#[tokio::test]
async fn unloading_stops_background_process_before_session_end() {
    let home = HooksHome::new();
    let pid = home.path("background-pid");
    let release = home.path("release-after-unload");
    let end = home.path("end.jsonl");
    let async_end = home.path("async-end-must-not-run");
    home.write_hooks(&format!(
        "{}async = true\n{}",
        hook_section(
            "UserPromptSubmit",
            &format!(
                "cat >/dev/null; echo $$ > '{}'; while [ ! -f '{}' ]; do sleep 0.01; done",
                pid.display(),
                release.display()
            )
        ),
        hook_section(
            "SessionEnd",
            &format!(
                "if kill -0 $(cat '{}') 2>/dev/null; then exit 2; fi; {}",
                pid.display(),
                logging(&end)
            )
        )
    ));
    let config_path = home.home.path().join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "\n{}async = true\n",
        hook_section("SessionEnd", &format!("touch '{}'", async_end.display()))
    ));
    std::fs::write(config_path, config).unwrap();
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.answer("finished");
    server
        .run_turn(&mut connection, &thread, "background")
        .await;
    wait_file(&pid).await;
    assert!(connection.passed.iter().any(|m| {
        m.to_string()
            .contains("running async SessionEnd hook synchronously")
    }));
    server
        .call(
            &mut connection,
            "thread/unsubscribe",
            json!({"threadId":thread}),
        )
        .await;
    connection.notification("thread/closed").await;
    let after = connection.drain().await;
    assert_eq!(events_of(&logged(&end)), ["SessionEnd"]);
    assert!(async_end.exists());
    assert!(
        !after
            .iter()
            .any(|m| m["method"] == "hook/started" || m["method"] == "hook/completed")
    );
}

#[tokio::test]
async fn hook_list_trust_and_enablement_writes_refresh_a_loaded_thread() {
    let home = HooksHome::new();
    let log = home.path("trusted.jsonl");
    home.write_hooks(&hook_section("UserPromptSubmit", &logging(&log)));
    let builder = home.config_builder().harness_overrides(ConfigOverrides {
        bypass_hook_trust: Some(false),
        ..Default::default()
    });
    let (server, _) = super::refresh::server_with_plugins_and_builder(&home, builder).await;
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let listed = server
        .call(
            &mut connection,
            "hooks/list",
            json!({"cwds":[home.workspace.path()]}),
        )
        .await;
    let hook = &listed["data"][0]["hooks"][0];
    assert_eq!(hook["trustStatus"], "untrusted", "{listed}");
    let key = hook["key"].as_str().unwrap();
    let state_key = format!("hooks.state.{}", serde_json::to_string(key).unwrap());
    home.script.answer("untrusted");
    server
        .run_turn(&mut connection, &thread, "before trust")
        .await;
    assert!(!log.exists());
    server.call(&mut connection, "config/batchWrite", json!({"reloadUserConfig":true,"edits":[{"keyPath":format!("{state_key}.trusted_hash"),"value":hook["currentHash"],"mergeStrategy":"replace"}]})).await;
    let trusted = server
        .call(
            &mut connection,
            "hooks/list",
            json!({"cwds":[home.workspace.path()]}),
        )
        .await;
    assert_eq!(
        trusted["data"][0]["hooks"][0]["trustStatus"], "trusted",
        "{trusted}"
    );
    home.script.answer("trusted");
    server
        .run_turn(&mut connection, &thread, "after trust")
        .await;
    assert_eq!(logged(&log).len(), 1);
    server.call(&mut connection, "config/batchWrite", json!({"reloadUserConfig":true,"edits":[{"keyPath":format!("{state_key}.enabled"),"value":false,"mergeStrategy":"replace"}]})).await;
    home.script.answer("disabled");
    server
        .run_turn(&mut connection, &thread, "after disable")
        .await;
    assert_eq!(logged(&log).len(), 1);
    let listed = server
        .call(
            &mut connection,
            "hooks/list",
            json!({"cwds":[home.workspace.path()]}),
        )
        .await;
    assert_eq!(listed["data"][0]["hooks"][0]["enabled"], false);
    server.call(&mut connection, "config/batchWrite", json!({"reloadUserConfig":true,"edits":[{"keyPath":format!("{state_key}.enabled"),"value":true,"mergeStrategy":"replace"}]})).await;
    home.script.answer("enabled");
    server
        .run_turn(&mut connection, &thread, "after enable")
        .await;
    assert_eq!(logged(&log).len(), 2);
}

#[tokio::test]
async fn repeated_stop_blocks_keep_ordered_prompts_across_cold_resume() {
    let home = HooksHome::new();
    let count = home.path("stop-count");
    let log = home.path("stop.jsonl");
    home.write_hooks(&hook_section("Stop", &format!("{}; n=$(cat '{}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{}'; if [ $n -le 2 ]; then printf '{{\"decision\":\"block\",\"reason\":\"stop reason %s\"}}' $n; fi", logging(&log), count.display(), count.display())));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    for text in ["first", "second", "third"] {
        home.script.answer(text);
    }
    server
        .run_turn(&mut connection, &thread, "finish after review")
        .await;
    assert_eq!(logged(&log).len(), 3);
    let input = request_text(&home.script);
    assert!(input.find("stop reason 1").unwrap() < input.find("stop reason 2").unwrap());
    super::super::unload(&server, &mut connection, &thread).await;
    let cold = home.server();
    let mut client = cold.connect(2).await;
    cold.call(&mut client, "thread/resume", json!({"threadId":thread}))
        .await;
    home.script.answer("resumed");
    cold.run_turn(&mut client, &thread, "after resume").await;
    let input = request_text(&home.script);
    assert!(
        input.contains("stop reason 1") && input.contains("stop reason 2"),
        "{input}"
    );
}

#[tokio::test]
async fn hook_output_spills_reach_the_model_with_the_original_text_on_disk() {
    for event in ["SessionStart", "PreToolUse", "PostToolUse", "Stop"] {
        let home = HooksHome::new();
        let text = format!("{event} long context ").repeat(2000);
        let payload = home.path("output.json");
        let ran = home.path("ran");
        let value = if event == "Stop" || event == "PostToolUse" {
            json!({"decision":"block","reason":text})
        } else {
            json!({"hookSpecificOutput":{"hookEventName":event,"additionalContext":text}})
        };
        std::fs::write(&payload, value.to_string()).unwrap();
        home.write_hooks(&hook_section(
            event,
            &format!(
                "if [ ! -f '{}' ]; then touch '{}'; cat '{}'; fi",
                ran.display(),
                ran.display(),
                payload.display()
            ),
        ));
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        if event != "SessionStart" {
            home.script
                .push(Step::Respond(command("echo spill", false)));
        }
        home.script.answer("first answer");
        if event == "Stop" {
            home.script.answer("after stop");
        }
        server
            .run_turn(&mut connection, &thread, "large output")
            .await;
        let request = home.script.last_request();
        let texts: Vec<_> = request
            .input()
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::Message(message) => Some(message.text_content()),
                ModelInputItem::ToolCallOutput(output) => Some(
                    output
                        .output()
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| output.output().to_string()),
                ),
                _ => None,
            })
            .collect();
        let visible = texts
            .iter()
            .find(|s| s.contains("Full hook output saved to:"))
            .unwrap_or_else(|| panic!("{event}: {texts:?}"));
        let path = visible
            .split_once("Full hook output saved to: ")
            .unwrap()
            .1
            .split(|c: char| c.is_whitespace() || matches!(c, '\\' | '\"' | '<' | '}' | ']'))
            .next()
            .unwrap();
        let expected = if event == "Stop" { text.trim() } else { &text };
        assert!(
            std::fs::read_to_string(path).unwrap() == expected,
            "{event}: spilled content differs"
        );
    }
}

#[tokio::test]
async fn permission_request_allow_runs_the_command_without_client_review() {
    let home = HooksHome::new();
    let marker = home.path("approved-marker");
    let log = home.path("allow.jsonl");
    std::fs::write(&marker, "remove only after permission").unwrap();
    home.write_hooks_with_approval("on-request", &hook_section("PermissionRequest", &format!("{}; printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PermissionRequest\",\"decision\":{{\"behavior\":\"allow\"}}}}}}'", logging(&log))));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let cmd = format!("rm '{}'", marker.display());
    home.script.push(Step::Respond(command(&cmd, true)));
    home.script.answer("approved");
    server
        .run_turn(&mut connection, &thread, "run approved command")
        .await;
    assert!(!marker.exists());
    let input = logged(&log);
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["tool_input"]["command"], cmd);
    assert!(input[0].get("tool_use_id").is_none());
    assert!(
        !connection
            .passed
            .iter()
            .any(|m| m["method"] == "item/commandExecution/requestApproval")
    );
}

#[tokio::test]
async fn pre_tool_rewrite_changes_the_executed_command() {
    let home = HooksHome::new();
    let log = home.path("rewrite.jsonl");
    home.write_hooks(&hook_section("PreToolUse", &format!("{}; printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\",\"updatedInput\":{{\"command\":\"echo rewritten-shell-value\"}},\"additionalContext\":\"rewrite context\"}}}}'", logging(&log))));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script
        .push(Step::Respond(command("echo original-shell-value", false)));
    home.script.answer("rewritten");
    server.run_turn(&mut connection, &thread, "rewrite").await;
    let input = request_text(&home.script);
    let outputs: Vec<_> = home
        .script
        .last_request()
        .input()
        .iter()
        .filter_map(|i| match i {
            ModelInputItem::ToolCallOutput(out) => Some(
                out.output()
                    .get("blocks")
                    .unwrap_or(out.output())
                    .to_string(),
            ),
            _ => None,
        })
        .collect();
    assert!(
        outputs.iter().any(|s| s.contains("rewritten-shell-value")),
        "{outputs:?}"
    );
    assert!(
        outputs.iter().all(|s| !s.contains("original-shell-value")),
        "{outputs:?}"
    );
    assert!(input.contains("rewrite context"));
    assert_eq!(
        logged(&log)[0]["tool_input"]["command"],
        "echo original-shell-value"
    );
}

#[tokio::test]
async fn post_tool_feedback_replaces_the_output_for_each_supported_stop_form() {
    for response in [
        "printf '%s' '{\"decision\":\"block\",\"reason\":\"review feedback\"}'",
        "printf '%s' '{\"continue\":false,\"stopReason\":\"review feedback\"}'",
        "echo 'review feedback' >&2; exit 2",
    ] {
        let home = HooksHome::new();
        home.write_hooks(&hook_section("PostToolUse", response));
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        home.script
            .push(Step::Respond(command("echo original-tool-result", false)));
        home.script.answer("reviewed");
        server
            .run_turn(&mut connection, &thread, "post feedback")
            .await;
        let outputs: Vec<_> = home
            .script
            .last_request()
            .input()
            .iter()
            .filter_map(|i| match i {
                ModelInputItem::ToolCallOutput(out) => Some(
                    out.output()
                        .get("blocks")
                        .unwrap_or(out.output())
                        .to_string(),
                ),
                _ => None,
            })
            .collect();
        assert!(
            outputs.iter().any(|s| s.contains("review feedback")),
            "{response}: {outputs:?}"
        );
        assert!(
            outputs.iter().all(|s| !s.contains("original-tool-result")),
            "{response}: {outputs:?}"
        );
    }
}

#[tokio::test]
async fn hook_list_reuses_root_repository_hooks_in_a_linked_worktree() {
    let home = HooksHome::new();
    let repo = home.workspace.path().canonicalize().unwrap();
    let linked = tempfile::tempdir().unwrap();
    let worktree = linked
        .path()
        .canonicalize()
        .unwrap()
        .join("linked-worktree");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q", repo.to_str().unwrap()]);
    git(&[
        "-C",
        repo.to_str().unwrap(),
        "-c",
        "user.name=Hook Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--allow-empty",
        "-qm",
        "fixture",
    ]);
    git(&[
        "-C",
        repo.to_str().unwrap(),
        "worktree",
        "add",
        "-q",
        "-b",
        "hook-fixture",
        worktree.to_str().unwrap(),
    ]);
    std::fs::create_dir_all(repo.join(".rusty-agent")).unwrap();
    std::fs::write(
        repo.join(".rusty-agent/config.toml"),
        hook_section("UserPromptSubmit", "echo repository hook"),
    )
    .unwrap();
    std::fs::create_dir_all(worktree.join(".rusty-agent")).unwrap();
    std::fs::write(
        worktree.join(".rusty-agent/config.toml"),
        hook_section("UserPromptSubmit", "echo worktree-local hook"),
    )
    .unwrap();
    home.write_hooks(&format!(
        "[projects.{}]\ntrust_level = 'trusted'",
        serde_json::to_string(repo.to_str().unwrap()).unwrap()
    ));
    let (server, _) = super::refresh::server_with_plugins(&home).await;
    let mut connection = server.connect(1).await;
    let listed = server
        .call(
            &mut connection,
            "hooks/list",
            json!({"cwds":[repo,worktree]}),
        )
        .await;
    let first = &listed["data"][0]["hooks"];
    let second = &listed["data"][1]["hooks"];
    assert_eq!(first.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(second.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(first[0]["command"], "echo repository hook");
    assert_eq!(second[0]["command"], "echo repository hook");
    assert_eq!(first[0]["key"], second[0]["key"]);
    assert_eq!(first[0]["sourcePath"], second[0]["sourcePath"]);
}

#[tokio::test]
async fn pre_compact_stop_aborts_once_without_compaction_or_error() {
    let home = HooksHome::new();
    let log = home.path("pre-compact.jsonl");
    home.write_hooks(
        &[
            hook_section(
                "PreCompact",
                &format!("{}; printf '%s' '{{\"continue\":false}}'", logging(&log)),
            ),
            hook_section("PostCompact", &logging(&log)),
            hook_section("Interrupt", &logging(&log)),
        ]
        .join("\n"),
    );
    let server = configured_server(&home, OnceCompactor::new(1));
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    server
        .turn_start(
            &mut connection,
            &thread,
            "keep this rejected compaction input",
        )
        .await;
    let ended = connection.notification("turn/completed").await;
    assert_eq!(ended["turn"]["status"], "interrupted");
    assert_eq!(events_of(&logged(&log)), ["PreCompact", "Interrupt"]);
    assert!(home.script.requests.lock().unwrap().is_empty());
    assert!(!connection.passed.iter().any(|m| m["method"] == "error"));
    let stored = home
        .store
        .read_thread(&ReadThreadParams::new(SessionId::new(&thread)).with_history())
        .await
        .unwrap();
    let text = serde_json::to_string(stored.history().unwrap().records()).unwrap();
    assert!(text.contains("keep this rejected compaction input"));
    assert!(!text.contains("accepted compacted summary"));
    assert!(!text.contains("<turn_aborted>"));
}

#[tokio::test]
async fn hook_list_uses_each_cwds_feature_and_project_trust() {
    let home = HooksHome::new();
    let root = home.workspace.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join(".rusty-agent")).unwrap();
    std::fs::write(
        root.join(".rusty-agent/config.toml"),
        format!(
            "[features]\ncodex_hooks = true\n{}",
            hook_section("SessionStart", "true")
        ),
    )
    .unwrap();
    home.write_hooks("[features]\ncodex_hooks = false");
    let (server, _) = super::refresh::server_with_plugins(&home).await;
    let mut connection = server.connect(1).await;
    let list = server
        .call(
            &mut connection,
            "hooks/list",
            json!({"cwds":[home.home.path(),root]}),
        )
        .await;
    assert!(list["data"][0]["hooks"].as_array().unwrap().is_empty());
    assert!(list["data"][1]["hooks"].as_array().unwrap().is_empty());
    let trust = format!(
        "[projects.{}]\ntrust_level = 'trusted'",
        serde_json::to_string(root.to_str().unwrap()).unwrap()
    );
    home.write_hooks(&format!("[features]\ncodex_hooks = false\n{trust}"));
    let list = server
        .call(
            &mut connection,
            "hooks/list",
            json!({"cwds":[home.home.path(),root]}),
        )
        .await;
    assert!(list["data"][0]["hooks"].as_array().unwrap().is_empty());
    assert_eq!(list["data"][1]["hooks"].as_array().unwrap().len(), 1);
    std::fs::remove_file(root.join(".rusty-agent/config.toml")).unwrap();
    let log = home.path("trusted-project.jsonl");
    std::fs::write(
        root.join(".rusty-agent/hooks.json"),
        json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":logging(&log)}]}]}})
            .to_string(),
    )
    .unwrap();
    home.write_hooks(&format!("[features]\ncodex_hooks = true\n{trust}"));
    let thread = start_thread(&server, &mut connection).await;
    home.script.answer("trusted project");
    server
        .run_turn(&mut connection, &thread, "without config toml")
        .await;
    assert_eq!(events_of(&logged(&log)), ["SessionStart"]);
}

#[tokio::test]
async fn a_blocked_pre_tool_hook_keeps_its_additional_context() {
    let home = HooksHome::new();
    home.write_hooks(&hook_section("PreToolUse", "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"denied with context\",\"additionalContext\":\"context survives a denied call\"}}'"));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script
        .push(Step::Respond(command("echo must-not-execute", false)));
    home.script.answer("denied");
    server
        .run_turn(&mut connection, &thread, "deny with context")
        .await;
    let request = request_text(&home.script);
    assert!(
        request.contains("context survives a denied call"),
        "{request}"
    );
    let outputs: Vec<_> = home
        .script
        .last_request()
        .input()
        .iter()
        .filter_map(|i| match i {
            ModelInputItem::ToolCallOutput(out) => Some(
                out.output()
                    .get("blocks")
                    .unwrap_or(out.output())
                    .to_string(),
            ),
            _ => None,
        })
        .collect();
    assert!(
        outputs
            .iter()
            .any(|text| text.contains("denied with context")),
        "{outputs:?}"
    );
}

/// Codex's app server shuts every loaded thread down when its transport ends, and each runs its
/// SessionEnd with reason `other` (`app_server_shutdown_runs_session_end_for_all_loaded_threads`).
#[tokio::test]
async fn closing_the_transport_runs_session_end_for_every_loaded_thread() {
    let home = HooksHome::new();
    // The threads end concurrently; each run writes a file of its own.
    let ends = home.path("ends");
    std::fs::create_dir_all(&ends).unwrap();
    home.write_hooks(&hook_section(
        "SessionEnd",
        &format!("cat > '{}/'$$.json", ends.display()),
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let first = start_thread(&server, &mut connection).await;
    let second = start_thread(&server, &mut connection).await;
    let Server { events, _task, .. } = server;
    drop(events);
    timeout(PATIENCE, _task)
        .await
        .expect("the server stops once its transport ends")
        .unwrap();
    let mut ended: Vec<_> = std::fs::read_dir(&ends)
        .unwrap()
        .map(|entry| {
            let input: Value =
                serde_json::from_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap())
                    .unwrap();
            assert_eq!(input["hook_event_name"], "SessionEnd");
            (
                input["session_id"].as_str().unwrap().to_owned(),
                input["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    ended.sort();
    let mut expected = vec![(first, "other".to_owned()), (second, "other".to_owned())];
    expected.sort();
    assert_eq!(ended, expected);
}
