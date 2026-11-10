//! Tool calls whose argument text is not JSON, through the default coding factory: the product
//! answers them with the tool's own argument error without asking anyone, as Codex's handlers
//! parse their arguments before any approval. PreToolUse sees such a call only where Codex builds
//! a hook payload from the raw text: ordinary function tools and MCP tools receive the text as a
//! string, `exec_command` and `apply_patch` run no hook. PostToolUse never runs.
use super::*;

/// The parser's diagnostic for `text`, as the tools quote it.
fn diagnostic(text: &str) -> String {
    serde_json::from_str::<Value>(text).unwrap_err().to_string()
}

pub(super) fn unparsed_call(id: &str, name: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::unparsed(CallId::new(id), name, text)),
    )
}

/// The text the model read for call `id`, and whether it was marked as an error.
pub(super) fn model_answer(script: &Script, id: &str) -> (String, bool) {
    script
        .last_request()
        .input()
        .iter()
        .find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == id => Some((
                match ra_core::tool::ToolOutput::from_stored(output.output()) {
                    Ok(Some(stored)) => stored.as_text().unwrap_or_default().to_owned(),
                    _ => output.output().to_string(),
                },
                output.is_error(),
            )),
            _ => None,
        })
        .expect("the model received the call's answer")
}

/// Logs for the three tool hooks, written by the hook sections `hooks` returns.
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

    fn hooks(&self) -> String {
        [
            hook_section("PreToolUse", &logging(&self.pre)),
            hook_section("PermissionRequest", &logging(&self.permission)),
            hook_section("PostToolUse", &logging(&self.post)),
        ]
        .join("\n")
    }

    fn counts(&self) -> (usize, usize, usize) {
        (
            logged(&self.pre).len(),
            logged(&self.permission).len(),
            logged(&self.post).len(),
        )
    }
}

/// Runs one turn whose model makes `call`, under a policy that asks about commands and patches,
/// and returns every message written to the client.
async fn run_unparsed(home: &HooksHome, call: RunItem) -> Vec<Value> {
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script
        .push(Step::Respond(ModelResponse::new(vec![call])));
    home.script.answer("done");
    server.run_turn(&mut connection, &thread, "run").await;
    connection.drain().await
}

/// No approval request reached the client.
pub(super) fn asked_nothing(messages: &[Value]) -> bool {
    !messages.iter().any(|message| {
        message.get("id").is_some()
            && message["method"]
                .as_str()
                .is_some_and(|method| method.contains("requestApproval"))
    })
}

/// Codex's exec handler builds its hook payload only from arguments it parsed, and answers such a
/// call before asking about it: no hook runs, nothing is asked, nothing runs.
#[tokio::test]
async fn an_unparsed_command_runs_no_hook_asks_nothing_and_reports_its_own_argument_error() {
    let home = HooksHome::new();
    let logs = Logs::new(&home);
    home.write_hooks_with_approval(
        "on-request",
        &format!("[features]\ncodex_hooks = true\n{}", logs.hooks()),
    );
    let text = r#"{"cmd": "touch ran.txt""#;

    let messages = run_unparsed(&home, unparsed_call("exec", "exec_command", text)).await;

    assert_eq!(logs.counts(), (0, 0, 0));
    assert!(asked_nothing(&messages), "{messages:#?}");
    assert!(!home.workspace.path().join("ran.txt").exists());
    let (answer, is_error) = model_answer(&home.script, "exec");
    assert!(is_error);
    assert!(
        answer.contains(&format!(
            "Invalid arguments: arguments are not valid JSON: {}.",
            diagnostic(text)
        )),
        "{answer}"
    );
}

/// Argument text that reads as a patch is still not a patch: Codex gives its hooks a patch only
/// from freeform input, and answers the call before asking about it.
#[tokio::test]
async fn an_unparsed_patch_runs_no_hook_even_when_its_text_is_a_patch() {
    let home = HooksHome::new();
    let logs = Logs::new(&home);
    home.write_hooks_with_approval(
        "on-request",
        &format!("[features]\ncodex_hooks = true\n{}", logs.hooks()),
    );
    let text = "*** Begin Patch\n*** Add File: added.txt\n+created\n*** End Patch\n";

    let messages = run_unparsed(&home, unparsed_call("patch", "apply_patch", text)).await;

    assert_eq!(logs.counts(), (0, 0, 0));
    assert!(asked_nothing(&messages), "{messages:#?}");
    assert!(!home.workspace.path().join("added.txt").exists());
    let (answer, is_error) = model_answer(&home.script, "patch");
    assert!(is_error);
    assert_eq!(
        answer,
        format!(
            "apply_patch verification failed: invalid arguments: arguments are not valid JSON: {}",
            diagnostic(text)
        )
    );
}

/// An ordinary function tool's PreToolUse receives the argument text as a string, as Codex's
/// registry gives it; the tool then answers with its own argument error and PostToolUse never
/// runs.
#[tokio::test]
async fn an_unparsed_function_call_gives_pre_tool_use_its_text_and_reports_the_tool_error() {
    let home = HooksHome::new();
    let logs = Logs::new(&home);
    home.write_hooks_with_approval(
        "on-request",
        &format!("[features]\ncodex_hooks = true\n{}", logs.hooks()),
    );
    let text = r#"{"path": "notes.txt""#;

    let messages = run_unparsed(&home, unparsed_call("read", "read_file", text)).await;

    assert_eq!(logs.counts(), (1, 0, 0));
    let pre = &logged(&logs.pre)[0];
    assert_eq!(pre["tool_name"], "read_file");
    assert_eq!(pre["tool_use_id"], "read");
    assert_eq!(pre["tool_input"], json!(text));
    assert!(asked_nothing(&messages), "{messages:#?}");
    let (answer, is_error) = model_answer(&home.script, "read");
    assert!(is_error);
    assert!(
        answer.contains(&format!(
            "Invalid arguments: arguments are not valid JSON: {}.",
            diagnostic(text)
        )),
        "{answer}"
    );
}
