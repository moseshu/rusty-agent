//! Product hooks on the default path: a thread the coding factory builds from a product
//! configuration runs the hooks that configuration declares, as Codex's session does, and its
//! clients hear of each run as `hook/started` and `hook/completed`, in order with the turn's own
//! notifications.
//!
//! Every case writes its own product home and workspace and reads no user-level configuration.
//! Hooks are trusted through the harness override Codex's `--dangerously-bypass-hook-trust` sets,
//! as trust itself is covered by the engine's suite.

use std::path::{Path, PathBuf};

use super::*;
use ra_coding::config::{ConfigBuilder, ConfigOverrides};
use ra_config::LoaderOverrides;

struct HooksHome {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    store: Arc<dyn ThreadStore>,
    script: Arc<Script>,
}

impl HooksHome {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            workspace: tempfile::tempdir().unwrap(),
            store: Arc::new(InMemoryThreadStore::new()),
            script: Arc::new(Script::default()),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().canonicalize().unwrap().join(name)
    }

    /// The configuration of `hooks`, with approval `never` and a read-only sandbox.
    fn write_hooks(&self, hooks: &str) {
        self.write_hooks_with_approval("never", hooks);
    }

    fn write_hooks_with_approval(&self, approval_policy: &str, hooks: &str) {
        std::fs::write(
            self.home.path().join("config.toml"),
            format!(
                "approval_policy = \"{approval_policy}\"\nsandbox_mode = \"read-only\"\n\n{hooks}\n"
            ),
        )
        .unwrap();
    }

    fn write_requirements(&self, contents: &str) {
        std::fs::write(self.home.path().join("requirements.toml"), contents).unwrap();
    }

    fn config_builder(&self) -> ConfigBuilder {
        let home = self.home.path().canonicalize().unwrap();
        let loader_overrides = LoaderOverrides {
            managed_config_path: Some(home.join("managed_config.toml")),
            system_config_path: Some(home.join("system_config.toml")),
            system_requirements_path: Some(home.join("requirements.toml")),
            ..LoaderOverrides::without_managed_config_for_tests()
        };
        ConfigBuilder::default()
            .codex_home(home)
            .loader_overrides(loader_overrides)
            .harness_overrides(ConfigOverrides {
                bypass_hook_trust: Some(true),
                ..ConfigOverrides::default()
            })
    }

    /// A server whose threads the coding factory builds from this home, unloaded as soon as the
    /// last connection lets go of them.
    fn server(&self) -> Server {
        let root = self.workspace.path().canonicalize().unwrap();
        let host = CodingHost::open(&root).unwrap();
        let settings = CodingThreadSettingsAdapter::new(
            &root,
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        )
        .unwrap()
        .with_rules_home(self.home.path());
        let factory =
            CodingThreadFactory::new(host, Arc::new(Resolver(Arc::clone(&self.script))), settings)
                .with_product_config(self.config_builder());
        build(
            Options {
                store: Some(Arc::clone(&self.store)),
                factory: Some(Arc::new(factory)),
                unload_delay: Some(Duration::ZERO),
            },
            None,
        )
        .0
    }
}

/// A command that appends the hook's input to `log`, one line per run.
fn logging(log: &Path) -> String {
    format!("cat >> '{0}'; echo >> '{0}'", log.display())
}

/// The hook inputs `log` holds, in the order the hooks ran.
fn logged(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn events_of(inputs: &[Value]) -> Vec<String> {
    inputs
        .iter()
        .map(|input| input["hook_event_name"].as_str().unwrap().to_owned())
        .collect()
}

fn hook_section(event: &str, command: &str) -> String {
    format!(
        "[[hooks.{event}]]\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = '''{command}'''\n"
    )
}

/// The hook notifications among `messages`: method, event name and status, in order.
fn hook_notifications(messages: &[Value]) -> Vec<(String, String, String)> {
    messages
        .iter()
        .filter(|message| {
            message["method"]
                .as_str()
                .is_some_and(|method| method.starts_with("hook/"))
        })
        .map(|message| {
            (
                message["method"].as_str().unwrap().to_owned(),
                message["params"]["run"]["eventName"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                message["params"]["run"]["status"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect()
}

async fn start_thread(server: &Server, connection: &mut Connection) -> String {
    let started = server.call(connection, "thread/start", json!({})).await;
    started["thread"]["id"].as_str().unwrap().to_owned()
}

/// Codex's session runs SessionStart and UserPromptSubmit before the prompt, Stop when the turn
/// concludes and SessionEnd when the thread ends; each run is told to the thread's clients as
/// started, then completed, under the turn it belongs to and before that turn completes. A
/// UserPromptSubmit hook's context reaches the model with the prompt.
#[tokio::test]
async fn the_default_factory_runs_lifecycle_hooks_and_reports_them_with_the_turn() {
    let home = HooksHome::new();
    let log = home.path("hooks.log");
    let log_hook = logging(&log);
    home.write_hooks(&[
        hook_section("SessionStart", &log_hook),
        hook_section(
            "UserPromptSubmit",
            &format!(
                "{log_hook}; printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"prompt noted\"}}}}'"
            ),
        ),
        hook_section("Stop", &log_hook),
        hook_section("SessionEnd", &log_hook),
    ]
    .join("\n"));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    home.script.answer("Done");

    let turn_id = server
        .turn_start(&mut connection, &thread_id, "hello")
        .await;
    connection.notification("turn/completed").await;
    let before: Vec<Value> = connection.passed.drain(..).collect();

    let inputs = logged(&log);
    assert_eq!(
        events_of(&inputs),
        ["SessionStart", "UserPromptSubmit", "Stop"]
    );
    assert_eq!(inputs[0]["source"], "startup");
    for input in &inputs {
        assert_eq!(input["session_id"], thread_id.as_str());
    }
    assert_eq!(inputs[1]["turn_id"], turn_id.as_str());
    assert_eq!(inputs[1]["prompt"], "hello");
    assert_eq!(inputs[2]["turn_id"], turn_id.as_str());
    assert_eq!(
        hook_notifications(&before),
        [
            ("hook/started", "sessionStart", "running"),
            ("hook/completed", "sessionStart", "completed"),
            ("hook/started", "userPromptSubmit", "running"),
            ("hook/completed", "userPromptSubmit", "completed"),
            ("hook/started", "stop", "running"),
            ("hook/completed", "stop", "completed"),
        ]
        .map(|(method, event, status)| (
            method.to_owned(),
            event.to_owned(),
            status.to_owned()
        ))
    );
    for message in before.iter().filter(|message| {
        message["method"]
            .as_str()
            .is_some_and(|m| m.starts_with("hook/"))
    }) {
        assert_eq!(message["params"]["threadId"], thread_id.as_str());
        assert_eq!(message["params"]["turnId"], turn_id.as_str());
    }
    let request = serde_json::to_string(home.script.last_request().input()).unwrap();
    assert!(request.contains("prompt noted"), "{request}");

    super::unload(&server, &mut connection, &thread_id).await;
    assert_eq!(
        events_of(&logged(&log)),
        ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"]
    );
}

/// The client hears a hook started while it is still running.
#[tokio::test]
async fn a_slow_hook_is_reported_as_started_before_it_completes() {
    let home = HooksHome::new();
    let gate = home.path("release");
    home.write_hooks(&hook_section(
        "UserPromptSubmit",
        &format!("while [ ! -f '{}' ]; do sleep 0.01; done", gate.display()),
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    home.script.answer("Done");

    server
        .turn_start(&mut connection, &thread_id, "hello")
        .await;
    let started = connection.notification("hook/started").await;
    assert_eq!(started["run"]["eventName"], "userPromptSubmit");
    let meanwhile = connection.drain().await;
    assert!(
        !meanwhile
            .iter()
            .any(|message| message["method"] == "hook/completed"),
        "{meanwhile:?}"
    );
    connection.passed.extend(meanwhile);
    std::fs::write(&gate, "go").unwrap();
    let completed = connection.notification("hook/completed").await;
    assert_eq!(completed["run"]["eventName"], "userPromptSubmit");
    connection.notification("turn/completed").await;
}

/// A PreToolUse hook that blocks a command keeps it from running, and the model is told why.
#[tokio::test]
async fn a_pre_tool_use_hook_blocks_a_command_on_the_default_path() {
    let home = HooksHome::new();
    let log = home.path("hooks.log");
    let marker = home.path("ran");
    home.write_hooks(&format!(
        "[[hooks.PreToolUse]]\nmatcher = \"Bash\"\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = '''{}; echo 'no commands' >&2; exit 2'''\n\n{}",
        logging(&log),
        hook_section("PostToolUse", &logging(&log)),
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    let call_id = "blocked-command";
    home.script
        .push(Step::Respond(ModelResponse::new(vec![RunItem::new(
            ItemId::new(call_id),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(call_id),
                "exec_command",
                json!({"cmd": format!("touch '{}'", marker.display()), "justification": null,
                   "prefix_rule": null, "sandbox_permissions": null, "workdir": null,
                   "shell": "/bin/sh", "tty": null, "login": false, "yield_time_ms": 1000,
                   "timeout_ms": null}),
            )),
        )])));
    home.script.answer("Understood");

    server
        .run_turn(&mut connection, &thread_id, "touch it")
        .await;

    assert!(!marker.exists(), "the blocked command ran");
    assert_eq!(events_of(&logged(&log)), ["PreToolUse"]);
    let request = serde_json::to_string(home.script.last_request().input()).unwrap();
    assert!(
        request.contains("Command blocked by PreToolUse hook: no commands"),
        "{request}"
    );
}

/// Codex's `Hooks::new` refuses a required managed hook that cannot load, and the session is not
/// created: `thread/start` fails with that error.
#[tokio::test]
async fn a_required_managed_hook_that_cannot_load_fails_thread_start() {
    let home = HooksHome::new();
    home.write_hooks("");
    home.write_requirements(&format!(
        "[hooks]\nmanaged_dir = '{}'\n\n[[hooks.PreToolUse]]\nmatcher = \"[\"\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"echo managed\"\n",
        home.home.path().display()
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;

    let error = server
        .call_error(&mut connection, "thread/start", json!({}))
        .await;

    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("failed to load required managed hooks"),
        "{message}"
    );
}

/// Warnings from loading the hooks reach the thread's client among its first events.
#[tokio::test]
async fn hook_startup_warnings_reach_the_client() {
    let home = HooksHome::new();
    home.write_hooks(
        "[[hooks.PreToolUse]]\nmatcher = \"[\"\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"echo user\"\n",
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;

    let warning = connection.notification("warning").await;
    assert_eq!(warning["threadId"], thread_id.as_str());
    assert!(
        warning["message"]
            .as_str()
            .unwrap()
            .contains("invalid matcher"),
        "{warning}"
    );
}

/// Interrupting a turn runs its Interrupt hooks once, told to the client before the turn's end,
/// and the thread's end runs SessionEnd after them.
#[tokio::test]
async fn an_interrupt_runs_its_hooks_once_before_session_end() {
    let home = HooksHome::new();
    let log = home.path("hooks.log");
    home.write_hooks(
        &[
            hook_section("Interrupt", &logging(&log)),
            hook_section("SessionEnd", &logging(&log)),
        ]
        .join("\n"),
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    home.script
        .push(Step::After(Arc::new(Notify::new()), final_message("never")));

    let turn_id = server.turn_start(&mut connection, &thread_id, "wait").await;
    home.script.wait_for_calls(1).await;
    server
        .call(
            &mut connection,
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
        .await;
    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "interrupted");
    let before: Vec<Value> = connection.passed.drain(..).collect();
    assert_eq!(
        hook_notifications(&before)
            .into_iter()
            .map(|(method, event, _)| (method, event))
            .collect::<Vec<_>>(),
        [
            ("hook/started".to_owned(), "interrupt".to_owned()),
            ("hook/completed".to_owned(), "interrupt".to_owned()),
        ]
    );

    super::unload(&server, &mut connection, &thread_id).await;
    let inputs = logged(&log);
    assert_eq!(events_of(&inputs), ["Interrupt", "SessionEnd"]);
    assert_eq!(inputs[0]["turn_id"], turn_id.as_str());
}

/// Without a product configuration a thread has no configuration layers to read hooks from, so
/// the factory builds none, as before.
#[tokio::test]
async fn a_factory_without_product_configuration_builds_no_hooks() {
    let home = HooksHome::new();
    let log = home.path("hooks.log");
    home.write_hooks(&hook_section("UserPromptSubmit", &logging(&log)));
    let root = home.workspace.path().canonicalize().unwrap();
    let factory = CodingThreadFactory::new(
        CodingHost::open(&root).unwrap(),
        Arc::new(Resolver(Arc::clone(&home.script))),
        CodingThreadSettingsAdapter::new(
            &root,
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        )
        .unwrap(),
    );
    let server = Server::start(Arc::new(factory));
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    home.script.answer("Done");

    server.run_turn(&mut connection, &thread_id, "hello").await;

    assert!(logged(&log).is_empty());
}

/// Codex runs the legacy `notify` program once a turn concludes without a Stop hook stopping it,
/// handing it the turn as its last argument.
#[tokio::test]
async fn the_legacy_notify_program_hears_the_completed_turn() {
    let home = HooksHome::new();
    let out = home.path("notified.json");
    home.write_hooks(&format!(
        "notify = ['sh', '-c', 'printf \"%s\" \"$1\" > {}', 'notify']\n",
        out.display()
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    home.script.answer("Done");

    let turn_id = server
        .turn_start(&mut connection, &thread_id, "hello")
        .await;
    connection.notification("turn/completed").await;
    let mut notified = String::new();
    for _ in 0..500 {
        notified = std::fs::read_to_string(&out).unwrap_or_default();
        if !notified.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let notified: Value = serde_json::from_str(&notified).unwrap();
    assert_eq!(notified["type"], "agent-turn-complete");
    assert_eq!(notified["thread-id"], thread_id.as_str());
    assert_eq!(notified["turn-id"], turn_id.as_str());
    assert_eq!(notified["input-messages"], json!(["hello"]));
    assert_eq!(notified["last-assistant-message"], "Done");
}

/// Interrupts the running turn and returns the hook notifications sent before it completed,
/// after checking it completed interrupted.
async fn interrupt_turn(
    server: &Server,
    connection: &mut Connection,
    thread_id: &str,
    turn_id: &str,
) -> Vec<(String, String)> {
    server
        .call(
            connection,
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
        .await;
    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "interrupted");
    let before: Vec<Value> = connection.passed.drain(..).collect();
    hook_notifications(&before)
        .into_iter()
        .map(|(method, event, _)| (method, event))
        .collect()
}

/// Codex runs Interrupt hooks for an interrupted turn whichever stage it reached: here the turn is
/// still in its prompt's UserPromptSubmit hook, before any model call.
#[tokio::test]
async fn an_interrupt_during_the_prompt_hooks_runs_interrupt_hooks() {
    let home = HooksHome::new();
    let log = home.path("hooks.log");
    let gate = home.path("release");
    home.write_hooks(
        &[
            hook_section(
                "UserPromptSubmit",
                &format!("while [ ! -f '{}' ]; do sleep 0.01; done", gate.display()),
            ),
            hook_section("Interrupt", &logging(&log)),
        ]
        .join("\n"),
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;

    let turn_id = server
        .turn_start(&mut connection, &thread_id, "hello")
        .await;
    connection.notification("hook/started").await;
    let notices = interrupt_turn(&server, &mut connection, &thread_id, &turn_id).await;
    std::fs::write(&gate, "go").unwrap();

    assert!(home.script.requests.lock().unwrap().is_empty());
    let inputs = logged(&log);
    assert_eq!(events_of(&inputs), ["Interrupt"]);
    assert_eq!(inputs[0]["turn_id"], turn_id.as_str());
    assert!(
        notices.windows(2).any(|pair| pair[0]
            == ("hook/started".to_owned(), "interrupt".to_owned())
            && pair[1] == ("hook/completed".to_owned(), "interrupt".to_owned())),
        "{notices:?}"
    );
}

/// The same holds for a turn waiting for the client to approve a command: its run has already
/// stopped for the approval when the interrupt arrives.
#[tokio::test]
async fn an_interrupt_while_waiting_for_approval_runs_interrupt_hooks_once() {
    let home = HooksHome::new();
    let log = home.path("hooks.log");
    home.write_hooks_with_approval("on-request", &hook_section("Interrupt", &logging(&log)));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread_id = start_thread(&server, &mut connection).await;
    let call_id = "escalated-command";
    home.script
        .push(Step::Respond(ModelResponse::new(vec![RunItem::new(
            ItemId::new(call_id),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(call_id),
                "exec_command",
                json!({"cmd": "touch made.txt", "justification": "needs the real file system",
                   "prefix_rule": null, "sandbox_permissions": "require_escalated",
                   "workdir": null, "shell": "/bin/sh", "tty": null, "login": false,
                   "yield_time_ms": 1000, "timeout_ms": null}),
            )),
        )])));

    let turn_id = server
        .turn_start(&mut connection, &thread_id, "make it")
        .await;
    let request = connection
        .until(|message| {
            message["method"] == "item/commandExecution/requestApproval"
                && message.get("id").is_some()
        })
        .await;
    assert_eq!(request["params"]["turnId"], turn_id.as_str());
    let notices = interrupt_turn(&server, &mut connection, &thread_id, &turn_id).await;

    assert_eq!(events_of(&logged(&log)), ["Interrupt"]);
    assert_eq!(
        notices,
        [
            ("hook/started".to_owned(), "interrupt".to_owned()),
            ("hook/completed".to_owned(), "interrupt".to_owned()),
        ]
    );
}

/// Codex's turn-complete notification lists the user messages of the input the turn was sampled
/// with: a resumed thread's notification includes the prompts of the history it resumed.
#[tokio::test]
async fn the_legacy_notify_program_hears_a_resumed_threads_history() {
    let home = HooksHome::new();
    let out = home.path("notified.json");
    home.write_hooks(&format!(
        "notify = ['sh', '-c', 'printf \"%s\" \"$1\" > {}', 'notify']\n",
        out.display()
    ));
    let thread_id = {
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread_id = start_thread(&server, &mut connection).await;
        home.script.answer("First");
        server
            .run_turn(&mut connection, &thread_id, "first prompt")
            .await;
        assert_eq!(
            notified_turn(&out).await["input-messages"],
            json!(["first prompt"])
        );
        super::unload(&server, &mut connection, &thread_id).await;
        thread_id
    };
    std::fs::remove_file(&out).unwrap();
    let server = home.server();
    let mut connection = server.connect(1).await;
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread_id}),
        )
        .await;
    home.script.answer("Second");

    server
        .run_turn(&mut connection, &thread_id, "second prompt")
        .await;

    let notified = notified_turn(&out).await;
    assert_eq!(
        notified["input-messages"],
        json!(["first prompt", "second prompt"])
    );
}

async fn notified_turn(out: &Path) -> Value {
    for _ in 0..500 {
        let notified = std::fs::read_to_string(out).unwrap_or_default();
        if !notified.is_empty() {
            return serde_json::from_str(&notified).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the notify program never ran");
}

#[path = "product_hooks/executor.rs"]
mod executor;

#[path = "product_hooks/fields.rs"]
mod fields;
