//! The R8 acceptance criterion for Codex semantic alignment, carried out rather than described.
//!
//! The standard is that `exec_command` + `write_stdin` + `apply_patch` can complete a real
//! multi-file modification task. Each of the three has its own tests in `it-tools`, and those prove
//! the tools work; none of them proves the three *combine*, which is the only thing this criterion
//! is about. So the task here is one no single tool can finish:
//!
//! 1. a version string is duplicated across two files, and both must change **in one patch**;
//! 2. the project's own checker has to agree the edit is consistent, which means running it;
//! 3. the checker is interactive — it prints `READY` and waits on standard input — so the command
//!    outlives its yield and the answer has to be sent to a session that is already running.
//!
//! **The model is not scripted with a fixed list of calls.** It reads the tool output it has been
//! handed and decides from that, because the third step is impossible otherwise: the session
//! identifier is minted at runtime, so a fixed script could not name it. That makes this test assert
//! something worth asserting on its own — the identifier a model needs to continue a session is
//! actually present in the prose the tool gave it, and not only in a field of some record the model
//! never sees.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ra_coding::{CodingHost, build_agent_with_host};
use ra_core::{
    agent::AgentId,
    cancel::CancelScope,
    error::{Error, Result},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    permission::{PermissionDecision, PermissionMode, PermissionRule},
    prompt::PromptRole,
    state::RunId,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunRequest, Runner},
};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The version both files start on, and the one the checker will be told to expect.
const BEFORE: &str = "1.0.0";
const AFTER: &str = "2.0.0";

/// One patch, two files. Splitting it into two calls would sidestep the part of the criterion that
/// says "multi-file".
fn patch() -> String {
    format!(
        "*** Begin Patch\n\
         *** Update File: version.txt\n\
         @@\n\
         -{BEFORE}\n\
         +{AFTER}\n\
         *** Update File: src/app.sh\n\
         @@\n\
         -APP_VERSION=\"{BEFORE}\"\n\
         +APP_VERSION=\"{AFTER}\"\n\
         *** End Patch\n"
    )
}

/// Writes the project the task operates on.
///
/// The checker is deliberately the kind of program that cannot be driven by `exec_command` alone: it
/// asks a question and waits. Plenty of real tooling does — a migration that asks before writing, a
/// release script that wants the version typed back — and that shape is the reason `write_stdin`
/// exists.
fn plant_project(root: &Path) {
    std::fs::write(root.join("version.txt"), format!("{BEFORE}\n")).expect("version file");
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("src/app.sh"),
        format!("#!/bin/sh\nAPP_VERSION=\"{BEFORE}\"\necho \"app ${{APP_VERSION}}\"\n"),
    )
    .expect("source file");

    let checker = "#!/bin/sh\n\
         # Prints READY, then verifies the version given on stdin against both files.\n\
         echo READY\n\
         read -r expected\n\
         file_version=$(cat version.txt)\n\
         code_version=$(sed -n 's/^APP_VERSION=\"\\(.*\\)\"$/\\1/p' src/app.sh)\n\
         if [ \"$file_version\" = \"$expected\" ] && [ \"$code_version\" = \"$expected\" ]; then\n\
         \u{20} echo \"OK $expected\"\n\
         else\n\
         \u{20} echo \"MISMATCH file=$file_version code=$code_version\"\n\
         fi\n";
    let path = root.join("check.sh");
    std::fs::write(&path, checker).expect("checker");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make the checker executable");
    }
}

/// A model that decides its next call from the tool output it has been shown.
///
/// It is the smallest thing that can carry out this task, and no smaller: step three needs an
/// identifier that did not exist when the run started.
struct TaskModel {
    /// The tool each turn asked for, in order, so the test can assert the route actually taken.
    calls: Mutex<Vec<String>>,
    /// The session identifier the model read out of the `exec_command` output.
    session: Mutex<Option<String>>,
    /// What the checker finally said, as the model saw it.
    verdict: Mutex<Option<String>>,
}

impl TaskModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            session: Mutex::new(None),
            verdict: Mutex::new(None),
        })
    }

    fn respond(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let seen: Vec<String> = request
            .input()
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::ToolCallOutput(output) => Some(rendered(output.output())),
                _ => None,
            })
            .collect();

        match seen.len() {
            // Nothing has run yet: change both files at once.
            0 => Ok(self.call("apply_patch", json!({ "patch": patch() }))),
            // The patch landed. Ask the project whether it agrees, with a yield short enough that
            // the wait on standard input is reached rather than sat through.
            //
            // **There is no race here to tune.** The checker blocks on `read`, so it cannot finish
            // before the yield however slow the machine is; and if it has not even printed `READY`
            // by then, the bytes written next sit in the pipe until it does, while the wait below
            // searches the session's whole output history rather than only what arrives after it.
            1 => {
                if !seen[0].contains("2 path(s)") {
                    return Err(Error::caller(format!(
                        "the patch did not report two changed paths: {}",
                        seen[0]
                    )));
                }
                Ok(self.call(
                    "exec_command",
                    json!({
                        "cmd": "./check.sh",
                        "workdir": null,
                        "shell": null,
                        "tty": null,
                        "login": null,
                        "yield_time_ms": 300,
                        "timeout_ms": null,
                    }),
                ))
            }
            // The checker is waiting. The identifier to answer on is in what the tool just said.
            2 => {
                let session = session_id(&seen[1]).ok_or_else(|| {
                    Error::caller(format!(
                        "a yielded command must name its session so it can be continued: {}",
                        seen[1]
                    ))
                })?;
                *self.session.lock().unwrap() = Some(session.clone());
                Ok(self.call(
                    "write_stdin",
                    json!({
                        "session_id": session,
                        "chars": format!("{AFTER}\n"),
                        "until": "match",
                        "match_text": format!("OK {AFTER}"),
                        "control": null,
                        "yield_time_ms": 3_000,
                    }),
                ))
            }
            // The checker answered. Report what it said.
            3 => {
                *self.verdict.lock().unwrap() = Some(seen[2].clone());
                self.calls.lock().unwrap().push("<final>".to_owned());
                Ok(ModelResponse::new(vec![RunItem::new(
                    ItemId::new("final"),
                    RunItemKind::Message(Message::assistant(
                        format!("Bumped both files to {AFTER} and the checker agrees."),
                        OutputPhase::Final,
                    )),
                )]))
            }
            other => Err(Error::caller(format!(
                "the task needed three tool calls; turn {} asked for more",
                other + 1
            ))),
        }
    }

    /// Records and builds one tool call.
    fn call(&self, name: &str, arguments: Value) -> ModelResponse {
        let index = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(name.to_owned());
            calls.len()
        };
        ModelResponse::new(vec![RunItem::new(
            ItemId::new(format!("call-{index}")),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(format!("c{index}")),
                name,
                arguments,
            )),
        )])
    }
}

#[async_trait]
impl Model for TaskModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.respond(&request)
    }
}

/// The text a model is shown for one tool result.
///
/// A tool result travels as structured blocks, and a provider adapter lowers the text ones into the
/// message the model reads. Doing the same here is what keeps the assertions about what a model can
/// actually see: pattern-matching the JSON envelope instead would let this test pass on an
/// identifier that only exists in a field no provider ever renders.
fn rendered(output: &Value) -> String {
    if let Some(blocks) = output.get("blocks").and_then(Value::as_array) {
        return blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
    }
    output
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| output.to_string())
}

/// The session identifier a yielded command reported, which it states between backticks.
fn session_id(text: &str) -> Option<String> {
    let candidate = text.split('`').nth(1)?;
    candidate.starts_with("exec-").then(|| candidate.to_owned())
}

/// Resolves every request to the one model under test.
struct OnlyModel(Arc<TaskModel>);

impl ModelResolver for OnlyModel {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("task-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

#[tokio::test]
async fn the_three_tools_complete_a_multi_file_task_on_one_workspace() {
    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path().canonicalize().expect("canonical root");
    plant_project(&root);

    let host = CodingHost::open(&root).expect("open the coding host");
    let agent = build_agent_with_host(AgentId::new("coder"), "Coder", &PromptRole::Main, &host)
        .await
        .expect("the product assembles its own agent");

    // The criterion names three entries; this is the surface they have to be on, rather than three
    // tools constructed for the occasion.
    let advertised: Vec<String> = agent
        .tools()
        .iter()
        .map(|tool| tool.model_definition().name().to_owned())
        .collect();
    for required in ["apply_patch", "exec_command", "write_stdin"] {
        assert!(
            advertised.iter().any(|name| name == required),
            "`{required}` must be advertised by the product's own assembly, found {advertised:?}"
        );
    }

    let model = TaskModel::new();
    let request = RunRequest::new(
        AgentBinding::direct(Arc::clone(&agent)),
        Arc::new(OnlyModel(Arc::clone(&model))),
        RunId::new("run-coding-task"),
        CancelScope::root(),
        vec![ModelInputItem::Message(Message::user(format!(
            "Bump the project version to {AFTER} everywhere and make sure check.sh agrees."
        )))],
    )
    .with_config(
        host.build_run_config()
            // **The product asks before it edits.** Left at the default mode this run stops on the
            // first `apply_patch` with a pending approval and goes no further, which is the right
            // behaviour and is tested where it belongs -- `it-runtime/tests/permission_engine.rs`
            // for the engine, `it-coding/tests/apply_patch.rs` for the facts an approval is shown.
            // A host carrying out an approved task answers that gate, and this test answers it the
            // way such a host does, so that what it measures is the three tools finishing the work
            // rather than the gate in front of them.
            .with_permission_mode(PermissionMode::AcceptEdits)
            .with_permission_rules([
                PermissionRule::new(PermissionDecision::Allow).with_tool_name("exec_command"),
                PermissionRule::new(PermissionDecision::Allow).with_tool_name("write_stdin"),
            ]),
    )
    .with_services(host.build_tool_services());

    let result = Runner::run(request).await.expect("the task must complete");

    // The route: one patch, one command, one answer written into it, then the final word.
    assert_eq!(
        model.calls.lock().unwrap().as_slice(),
        ["apply_patch", "exec_command", "write_stdin", "<final>"],
        "each step had to happen, and in this order: the checker cannot pass before the patch, and \
         cannot be answered before it is running"
    );

    // The workspace really changed, both files, on disk.
    assert_eq!(
        std::fs::read_to_string(root.join("version.txt")).expect("version file"),
        format!("{AFTER}\n")
    );
    assert!(
        std::fs::read_to_string(root.join("src/app.sh"))
            .expect("source file")
            .contains(&format!("APP_VERSION=\"{AFTER}\"")),
        "the second file in the same patch has to have changed too"
    );

    // And the project's own checker agreed, which is the part no assertion of ours could fake: it
    // read both files itself, inside the workspace, and compared them against what was typed in.
    let verdict = model.verdict.lock().unwrap().clone().expect("a verdict");
    assert!(
        verdict.contains(&format!("OK {AFTER}")),
        "the checker had to confirm the edit through the session it was answered on: {verdict}"
    );
    assert!(
        !verdict.contains("MISMATCH"),
        "the checker must not have found the two files disagreeing: {verdict}"
    );

    // The identifier the model continued the session on was the one the tool told it about.
    let session = model.session.lock().unwrap().clone().expect("a session id");
    assert!(
        session.starts_with("exec-"),
        "the model read the identifier out of the tool's own prose: {session}"
    );
    assert_eq!(
        result.final_text(),
        format!("Bumped both files to {AFTER} and the checker agrees.")
    );
}
