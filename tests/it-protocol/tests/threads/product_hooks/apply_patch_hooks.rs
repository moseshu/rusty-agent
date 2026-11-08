// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Patches through the default coding factory, against Codex's `ApplyPatchHandler` and the
//! `apply_patch` cases of `core/tests/suite/hooks.rs`: a patch that cannot be parsed runs no hook,
//! one that does not verify runs only PreToolUse and shows no item, and one that verifies shows its
//! file change before PermissionRequest or the client is asked about it, ending as the patch did.
use super::*;

const ALLOW: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}'"#;
const DENY: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"blocked by hook"}}}'"#;
const BLOCK: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no patches"}}'"#;
const REWRITE_UNPARSABLE: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"not a patch"}}}'"#;

const ADD: &str = "*** Begin Patch\n*** Add File: added.txt\n+created\n*** End Patch\n";
const MISSING: &str =
    "*** Begin Patch\n*** Update File: missing.txt\n@@\n-nope\n+better\n*** End Patch\n";

/// How the thread asks about patches.
#[derive(Clone, Copy)]
enum Asking {
    /// A read-only sandbox under `on-request`: every patch is asked about.
    Asks,
    /// Full access under `on-request`: a patch runs unasked.
    Unasked,
    /// A read-only sandbox under `never`: every patch is refused by the policy.
    Refuses,
}

fn configure(home: &HooksHome, asking: Asking, hooks: &[String]) {
    let (approval, sandbox) = match asking {
        Asking::Asks => ("on-request", "read-only"),
        Asking::Unasked => ("on-request", "danger-full-access"),
        Asking::Refuses => ("never", "read-only"),
    };
    std::fs::write(
        home.home.path().join("config.toml"),
        format!(
            "approval_policy = \"{approval}\"\nsandbox_mode = \"{sandbox}\"\n\n{}\n",
            hooks.join("\n")
        ),
    )
    .unwrap();
}

/// Logs for each tool hook, and the hook sections that write them.
struct Logs {
    pre: PathBuf,
    permission: PathBuf,
    post: PathBuf,
}

impl Logs {
    fn new(home: &HooksHome) -> Self {
        Self {
            pre: home.path("pre.jsonl"),
            permission: home.path("permission.jsonl"),
            post: home.path("post.jsonl"),
        }
    }

    fn hooks(&self, pre: Option<&str>, permission: Option<&str>) -> Vec<String> {
        let with = |log: &Path, decision: Option<&str>| {
            decision.map_or_else(
                || logging(log),
                |decision| format!("{}; {decision}", logging(log)),
            )
        };
        vec![
            hook_section("PreToolUse", &with(&self.pre, pre)),
            hook_section("PermissionRequest", &with(&self.permission, permission)),
            hook_section("PostToolUse", &logging(&self.post)),
        ]
    }

    fn counts(&self) -> (usize, usize, usize) {
        (
            logged(&self.pre).len(),
            logged(&self.permission).len(),
            logged(&self.post).len(),
        )
    }
}

fn patch_call(id: &str, patch: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(id),
            "apply_patch",
            json!({ "patch": patch }),
        )),
    )
}

/// The file-change notifications for `id`, in order: method and status.
fn file_change_notifications(messages: &[Value], id: &str) -> Vec<(String, String)> {
    messages
        .iter()
        .filter(|message| {
            let method = message["method"].as_str().unwrap_or_default();
            message.get("id").is_none()
                && (method == "item/started" || method == "item/completed")
                && message["params"]["item"]["type"] == "fileChange"
                && message["params"]["item"]["id"] == id
        })
        .map(|message| {
            (
                message["method"].as_str().unwrap().to_owned(),
                message["params"]["item"]["status"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect()
}

fn position(messages: &[Value], matches: impl Fn(&Value) -> bool) -> usize {
    messages
        .iter()
        .position(matches)
        .expect("the message was written")
}

fn model_text(script: &Script, call: &str) -> String {
    script
        .last_request()
        .input()
        .iter()
        .find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == call => {
                Some(output.output().clone())
            }
            _ => None,
        })
        .map(
            |output| match ra_core::tool::ToolOutput::from_stored(&output) {
                Ok(Some(stored)) => stored.as_text().unwrap_or_default().to_owned(),
                _ => output
                    .as_str()
                    .map_or_else(|| output.to_string(), str::to_owned),
            },
        )
        .expect("the model received the call's output")
}

/// The file changes the thread reads back with, by id and status.
async fn file_changes_read_back(
    server: &Server,
    connection: &mut Connection,
    thread_id: &str,
) -> Vec<(String, String)> {
    let thread = server
        .read(
            connection,
            ThreadReadParams::new(thread_id).including_turns(),
        )
        .await;
    thread
        .turns()
        .iter()
        .flat_map(|turn| turn.items().iter())
        .filter_map(|item| match serde_json::to_value(item).unwrap() {
            item if item["type"] == "fileChange" => Some((
                item["id"].as_str().unwrap().to_owned(),
                item["status"].as_str().unwrap().to_owned(),
            )),
            _ => None,
        })
        .collect()
}

/// Runs one turn that makes `patch` as call `id`, and returns what was written to the client.
async fn run_patch(
    home: &HooksHome,
    patch: &str,
    id: &str,
) -> (Server, Connection, String, Vec<Value>) {
    let server = home.server_showing_items();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script
        .push(Step::Respond(ModelResponse::new(vec![patch_call(
            id, patch,
        )])));
    home.script.answer("done");
    server.run_turn(&mut connection, &thread, "patch").await;
    let messages = connection.drain().await;
    (server, connection, thread, messages)
}

/// A patch that cannot be parsed runs no hook at all, as Codex fails it while resolving its
/// environment, and shows no item.
#[tokio::test]
async fn an_unparsable_patch_runs_no_hook_and_shows_no_item() {
    let home = HooksHome::new();
    let logs = Logs::new(&home);
    configure(&home, Asking::Asks, &logs.hooks(None, Some(ALLOW)));

    let (server, mut connection, thread, messages) =
        run_patch(&home, "not a patch", "broken").await;

    assert_eq!(logs.counts(), (0, 0, 0));
    assert!(
        hook_notifications(&messages)
            .iter()
            .all(|(_, event, _)| event != "preToolUse" && event != "permissionRequest"),
        "{messages:#?}"
    );
    assert!(file_change_notifications(&messages, "broken").is_empty());
    assert!(
        model_text(&home.script, "broken").starts_with("apply_patch verification failed: "),
        "{}",
        model_text(&home.script, "broken")
    );
    assert!(
        file_changes_read_back(&server, &mut connection, &thread)
            .await
            .is_empty()
    );
}

/// A patch that parses but does not verify runs PreToolUse and nothing after it, asked about or
/// not: no PermissionRequest, no question, no item, no PostToolUse.
#[tokio::test]
async fn a_patch_that_does_not_verify_runs_only_pre_tool_use() {
    for asking in [Asking::Asks, Asking::Unasked] {
        let home = HooksHome::new();
        let logs = Logs::new(&home);
        configure(&home, asking, &logs.hooks(None, Some(ALLOW)));

        let (server, mut connection, thread, messages) = run_patch(&home, MISSING, "missing").await;

        assert_eq!(logs.counts(), (1, 0, 0));
        assert!(
            !messages
                .iter()
                .any(|message| message["method"] == "item/fileChange/requestApproval")
        );
        assert!(file_change_notifications(&messages, "missing").is_empty());
        let text = model_text(&home.script, "missing");
        assert!(
            text.starts_with("apply_patch verification failed: "),
            "{text}"
        );
        assert!(!home.workspace.path().join("missing.txt").exists());
        assert!(
            file_changes_read_back(&server, &mut connection, &thread)
                .await
                .is_empty()
        );
    }
}

/// A patch PreToolUse blocks, one it rewrites into something unparsable, and one the policy
/// refuses all end before any item.
#[tokio::test]
async fn blocked_rewritten_and_refused_patches_show_no_item() {
    for (asking, pre, expected) in [
        (
            Asking::Asks,
            Some(BLOCK),
            "Command blocked by PreToolUse hook: no patches.",
        ),
        (
            Asking::Asks,
            Some(REWRITE_UNPARSABLE),
            "apply_patch verification failed: ",
        ),
        (
            Asking::Refuses,
            None,
            "apply_patch is denied by the read-only sandbox policy",
        ),
    ] {
        let home = HooksHome::new();
        let logs = Logs::new(&home);
        configure(&home, asking, &logs.hooks(pre, Some(ALLOW)));

        let (_server, _connection, _thread, messages) = run_patch(&home, ADD, "patch").await;

        let (_, permission, post) = logs.counts();
        assert_eq!((permission, post), (0, 0), "{expected}");
        assert!(
            file_change_notifications(&messages, "patch").is_empty(),
            "{expected}"
        );
        let text = model_text(&home.script, "patch");
        assert!(text.starts_with(expected), "{text}");
        assert!(
            !home.workspace.path().join("added.txt").exists(),
            "{expected}"
        );
    }
}

/// Approved by its hook, a patch shows its item before the hook runs, applies, completes once,
/// and runs PostToolUse once.
#[tokio::test]
async fn a_patch_a_permission_hook_approves_completes_after_its_item_started() {
    let home = HooksHome::new();
    let logs = Logs::new(&home);
    configure(&home, Asking::Asks, &logs.hooks(None, Some(ALLOW)));

    let (server, mut connection, thread, messages) = run_patch(&home, ADD, "approved").await;

    assert_eq!(logs.counts(), (1, 1, 1));
    assert_eq!(
        file_change_notifications(&messages, "approved"),
        [
            ("item/started".to_owned(), "inProgress".to_owned()),
            ("item/completed".to_owned(), "completed".to_owned()),
        ]
    );
    let started = position(&messages, |message| {
        message["method"] == "item/started" && message["params"]["item"]["id"] == "approved"
    });
    let asked = position(&messages, |message| {
        message["method"] == "hook/started"
            && message["params"]["run"]["eventName"] == "permissionRequest"
    });
    assert!(started < asked, "the item starts before the hook is asked");
    assert!(
        !messages
            .iter()
            .any(|message| message["method"] == "item/fileChange/requestApproval")
    );
    assert_eq!(
        std::fs::read_to_string(home.workspace.path().join("added.txt")).unwrap(),
        "created\n"
    );
    assert_eq!(
        file_changes_read_back(&server, &mut connection, &thread).await,
        [("approved".to_owned(), "completed".to_owned())]
    );
}

/// Refused by its hook, a patch's item ends declined, nothing is written, PostToolUse does not
/// run, and the model reads the hook's message.
#[tokio::test]
async fn a_patch_a_permission_hook_refuses_ends_declined() {
    let home = HooksHome::new();
    let logs = Logs::new(&home);
    configure(&home, Asking::Asks, &logs.hooks(None, Some(DENY)));

    let (server, mut connection, thread, messages) = run_patch(&home, ADD, "refused").await;

    assert_eq!(logs.counts(), (1, 1, 0));
    assert_eq!(
        file_change_notifications(&messages, "refused"),
        [
            ("item/started".to_owned(), "inProgress".to_owned()),
            ("item/completed".to_owned(), "declined".to_owned()),
        ]
    );
    assert_eq!(model_text(&home.script, "refused"), "blocked by hook");
    assert!(!home.workspace.path().join("added.txt").exists());
    assert_eq!(
        file_changes_read_back(&server, &mut connection, &thread).await,
        [("refused".to_owned(), "declined".to_owned())]
    );
}

/// Undecided by its hook, a patch is asked about after its item started; the client's answer
/// ends it, and the item is not started again.
#[tokio::test]
async fn an_undecided_patch_is_asked_after_its_item_started() {
    for (decision, status, posts) in [("accept", "completed", 1), ("decline", "declined", 0)] {
        let home = HooksHome::new();
        let logs = Logs::new(&home);
        configure(&home, Asking::Asks, &logs.hooks(None, None));
        let server = home.server_showing_items();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        home.script
            .push(Step::Respond(ModelResponse::new(vec![patch_call(
                "asked", ADD,
            )])));
        home.script.answer("done");

        server.turn_start(&mut connection, &thread, "patch").await;
        let request = connection
            .until(|message| {
                message["method"] == "item/fileChange/requestApproval"
                    && message.get("id").is_some()
            })
            .await;
        let before: Vec<Value> = connection.passed.drain(..).collect();
        assert_eq!(
            file_change_notifications(&before, "asked"),
            [("item/started".to_owned(), "inProgress".to_owned())],
            "{decision}: the item starts before the question"
        );
        assert_eq!(logs.counts(), (1, 1, 0), "{decision}");
        server
            .send(
                &connection,
                ra_protocol::frame::JsonRpcResponse::new(
                    serde_json::from_value::<ra_protocol::frame::RequestId>(request["id"].clone())
                        .unwrap(),
                    json!({ "decision": decision }),
                ),
            )
            .await;
        connection.notification("turn/completed").await;
        let messages = connection.drain().await;

        assert_eq!(
            file_change_notifications(&messages, "asked"),
            [("item/completed".to_owned(), status.to_owned())],
            "{decision}: the item is not started again"
        );
        assert_eq!(logs.counts(), (1, 1, posts), "{decision}");
        assert_eq!(
            home.workspace.path().join("added.txt").exists(),
            decision == "accept"
        );
        assert_eq!(
            file_changes_read_back(&server, &mut connection, &thread).await,
            [("asked".to_owned(), status.to_owned())],
            "{decision}"
        );
    }
}

/// A patch that verified and could not be written ends failed, leaves its source as it was, and
/// runs no PostToolUse.
#[tokio::test]
async fn a_patch_that_cannot_be_written_ends_failed() {
    let home = HooksHome::new();
    let logs = Logs::new(&home);
    configure(&home, Asking::Unasked, &logs.hooks(None, None));
    std::fs::write(home.workspace.path().join("source.txt"), "kept\n").unwrap();
    std::fs::create_dir(home.workspace.path().join("destination")).unwrap();
    let patch = "*** Begin Patch\n*** Update File: source.txt\n*** Move to: destination\n@@\n-kept\n+moved\n*** End Patch\n";

    let (server, mut connection, thread, messages) = run_patch(&home, patch, "blocked").await;

    assert_eq!(logs.counts(), (1, 0, 0));
    assert_eq!(
        file_change_notifications(&messages, "blocked"),
        [
            ("item/started".to_owned(), "inProgress".to_owned()),
            ("item/completed".to_owned(), "failed".to_owned()),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(home.workspace.path().join("source.txt")).unwrap(),
        "kept\n"
    );
    assert!(
        model_text(&home.script, "blocked").starts_with("Patch was not applied:"),
        "{}",
        model_text(&home.script, "blocked")
    );
    assert_eq!(
        file_changes_read_back(&server, &mut connection, &thread).await,
        [("blocked".to_owned(), "failed".to_owned())]
    );
}
