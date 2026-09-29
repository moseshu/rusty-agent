//! `ra-tools::sandbox::shell`: the shell capability and its two tools against a scripted session.
//!
//! Ported from the reference's `tests/sandbox/capabilities/test_shell_capability.py`, one test per
//! upstream test, in the upstream order. The session is the Rust counterpart of the reference's
//! `scripted_sandbox_session`: it records every call and answers from a script, so the tests read
//! exactly what the tool asked the session for.
//!
//! The reference pins the chunk id and the wall time by replacing `uuid4` and `perf_counter`. Here
//! both are read from the response and checked for their shape — six hex digits, four decimals —
//! and everything after them is compared exactly.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    capability::{Capability, CapabilityFamily, SandboxBinding},
    context::RunContext,
    error::{Error, ToolErrorKind},
    item::{AgentId, CallId},
    sandbox::{
        AsUser, ErrorCode, ExecRequest, ExecResult, FileEntry, Manifest, PtyExecUpdate,
        PtyProcessId, PtyStartRequest, PtyWriteRequest, SandboxError, SandboxPathGrant,
        SandboxResult, SandboxSession, SandboxSessionState, SandboxWorkspaceScope, SessionPath,
        SessionResources, ShellInvocation, Snapshot, User,
    },
    state::RunId,
    tool::{Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext},
};
use ra_tools::sandbox::shell::{SHELL_INSTRUCTIONS, Shell, ShellToolSet};
use ra_tools::sandbox::shell_tool::{
    ExecCommandArgs, ExecCommandTool, WriteStdinArgs, WriteStdinTool, resolve_shell,
};
use ra_tools::sandbox::{ApprovalCheck, NeedsApproval};
use serde_json::{Value, json};

// ---- the scripted session ------------------------------------------------------------------

/// One recorded call.
#[derive(Debug, Clone)]
enum Call {
    Exec(ExecRequest),
    PtyStart(PtyStartRequest),
    PtyWrite(PtyWriteRequest),
}

/// One scripted answer.
enum Answer {
    Exec(SandboxResult<ExecResult>),
    /// Echoes the command back as both streams, exit code 7 — the reference's default responder.
    ExecEcho,
    PtyStart(SandboxResult<PtyExecUpdate>),
    PtyWrite(SandboxResult<PtyExecUpdate>),
}

struct ScriptedSession {
    state: SandboxSessionState,
    resources: SessionResources,
    pty: bool,
    script: Mutex<VecDeque<Answer>>,
    calls: Mutex<Vec<Call>>,
}

impl ScriptedSession {
    fn new(pty: bool, script: Vec<Answer>) -> Arc<Self> {
        Self::with_manifest(pty, script, Manifest::new().with_root("/workspace"))
    }

    fn with_manifest(pty: bool, script: Vec<Answer>, manifest: Manifest) -> Arc<Self> {
        Arc::new(Self {
            state: SandboxSessionState::new("scripted", Snapshot::noop(), manifest),
            resources: SessionResources::new(),
            pty,
            script: Mutex::new(script.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn next(&self, call: Call) -> Answer {
        self.calls.lock().unwrap().push(call);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("the script has an answer for this call")
    }

    fn assert_complete(&self) {
        assert!(
            self.script.lock().unwrap().is_empty(),
            "unused script steps"
        );
    }
}

fn not_scripted() -> SandboxError {
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        ra_core::sandbox::OpName::Exec,
        "not scripted",
    )
}

#[async_trait]
impl SandboxSession for ScriptedSession {
    fn backend_id(&self) -> &str {
        "scripted"
    }

    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    fn supports_pty(&self) -> bool {
        self.pty
    }

    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        let rendered = request.command().join(" ");
        match self.next(Call::Exec(request)) {
            Answer::Exec(result) => result,
            Answer::ExecEcho => Ok(ExecResult::new(
                format!("stdout: {rendered}").into_bytes(),
                format!("stderr: {rendered}").into_bytes(),
                7,
            )),
            _ => panic!("scripted a different call than exec"),
        }
    }

    async fn pty_start(&self, request: PtyStartRequest) -> SandboxResult<PtyExecUpdate> {
        match self.next(Call::PtyStart(request)) {
            Answer::PtyStart(result) => result,
            _ => panic!("scripted a different call than pty_start"),
        }
    }

    async fn pty_write(&self, request: PtyWriteRequest) -> SandboxResult<PtyExecUpdate> {
        match self.next(Call::PtyWrite(request)) {
            Answer::PtyWrite(result) => result,
            _ => panic!("scripted a different call than pty_write"),
        }
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(true)
    }

    async fn ls(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        Err(not_scripted())
    }

    async fn rm(
        &self,
        _path: SessionPath<'_>,
        _recursive: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Err(not_scripted())
    }

    async fn mkdir(
        &self,
        _path: SessionPath<'_>,
        _parents: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Err(not_scripted())
    }

    async fn read(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<u8>> {
        Err(not_scripted())
    }

    async fn write(
        &self,
        _path: SessionPath<'_>,
        _data: Vec<u8>,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Err(not_scripted())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Err(not_scripted())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Err(not_scripted())
    }
}

/// The reference's `_shell_session`: no terminals, one exec answered with the echo responder.
fn shell_session() -> Arc<ScriptedSession> {
    ScriptedSession::new(false, vec![Answer::ExecEcho])
}

fn exec_session(result: SandboxResult<ExecResult>) -> Arc<ScriptedSession> {
    ScriptedSession::new(false, vec![Answer::Exec(result)])
}

fn running(process_id: i64) -> PtyExecUpdate {
    PtyExecUpdate::running(PtyProcessId::new(process_id), Vec::new())
}

fn transport_error(retry_safe: Option<bool>, tty: bool) -> SandboxError {
    let mut error = SandboxError::exec_transport(
        vec!["pwd".to_owned()],
        Some("connection closed while reading HTTP status line"),
    )
    .with_context("stage", "open_pipe");
    if let Some(retry_safe) = retry_safe {
        error = error.with_context("retry_safe", retry_safe);
    }
    if tty {
        error = error.with_context("tty", true);
    }
    error
}

// ---- binding, calling, reading responses ---------------------------------------------------

fn session(session: &Arc<ScriptedSession>) -> Arc<dyn SandboxSession> {
    Arc::clone(session) as Arc<dyn SandboxSession>
}

fn bound(shell: &Shell, scripted: &Arc<ScriptedSession>) -> Shell {
    shell.bound(session(scripted), None, SandboxWorkspaceScope::root())
}

fn scope(cwd: &str) -> SandboxWorkspaceScope {
    SandboxWorkspaceScope::from_cwd(Some(cwd)).expect("scope")
}

fn run_context() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("shell-runner"))
        .name("Shell runner")
        .build()
        .expect("agent");
    RunContext::new(RunId::new("run-sandbox-shell"), &agent)
}

async fn invoke(tool: &dyn Tool, arguments: &Value) -> Result<String, Error> {
    let run = run_context();
    let call_id = CallId::new("call");
    let output = tool
        .call(ToolContext::new(&run, tool, &call_id, arguments))
        .await?;
    Ok(output.as_text().expect("text").to_owned())
}

fn exec_args(args: &ExecCommandArgs) -> Value {
    serde_json::to_value(args).expect("arguments")
}

fn stdin_args(args: &WriteStdinArgs) -> Value {
    serde_json::to_value(args).expect("arguments")
}

/// Checks the two lines that change on every call and returns the rest of the response.
fn after_stamp(output: &str) -> String {
    let mut lines = output.splitn(3, '\n');
    let chunk = lines.next().expect("chunk line");
    let chunk_id = chunk.strip_prefix("Chunk ID: ").expect("chunk id");
    assert_eq!(chunk_id.len(), 6, "{chunk}");
    assert!(
        chunk_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    let wall = lines.next().expect("wall line");
    let seconds = wall
        .strip_prefix("Wall time: ")
        .and_then(|rest| rest.strip_suffix(" seconds"))
        .expect("wall time");
    let (_, decimals) = seconds.split_once('.').expect("decimal point");
    assert_eq!(decimals.len(), 4, "{wall}");
    seconds.parse::<f64>().expect("seconds");
    lines.next().unwrap_or_default().to_owned()
}

fn exec_calls(scripted: &ScriptedSession) -> Vec<ExecRequest> {
    scripted
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Exec(request) => Some(request),
            _ => None,
        })
        .collect()
}

fn pty_starts(scripted: &ScriptedSession) -> Vec<PtyStartRequest> {
    scripted
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::PtyStart(request) => Some(request),
            _ => None,
        })
        .collect()
}

fn names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool.origin().name().to_owned())
        .collect()
}

// ---- TestShellCapability --------------------------------------------------------------------

// `test_resolve_shell_uses_plain_sh_when_login_is_false`
#[test]
fn without_login_the_default_shell_is_plain_sh() {
    assert_eq!(
        resolve_shell(None, false),
        ShellInvocation::Prefix(vec!["sh".to_owned(), "-c".to_owned()])
    );
}

// `test_tools_requires_bound_session`
#[test]
fn an_unbound_shell_refuses_to_build_its_tools() {
    let error = Shell::new().try_tools().err().expect("unbound");
    assert!(
        error
            .to_string()
            .contains("Shell capability is not bound to a SandboxSession"),
        "{error}"
    );
    // The trait method cannot fail, so it contributes nothing instead.
    assert!(Capability::tools(&Shell::new()).is_empty());
}

// `test_tools_exposes_exec_command_function_tool_after_bind`
#[test]
fn a_bound_shell_without_terminals_offers_exec_command_only() {
    let tools = bound(&Shell::new(), &shell_session())
        .try_tools()
        .expect("tools");
    assert_eq!(names(&tools), ["exec_command"]);
}

// `test_tools_exposes_write_stdin_for_pty_sessions`
#[test]
fn a_bound_shell_with_terminals_also_offers_write_stdin() {
    let scripted = ScriptedSession::new(true, Vec::new());
    let tools = bound(&Shell::new(), &scripted).try_tools().expect("tools");
    assert_eq!(names(&tools), ["exec_command", "write_stdin"]);
}

// `test_tools_keep_both_pty_session_methods_callable`: in Python the check is that the session
// still has both methods after binding; in Rust both are trait methods, so what is left to check is
// that the two tools reach the same session.
#[test]
fn both_tools_reach_the_session_they_were_bound_to() {
    let scripted = ScriptedSession::new(true, Vec::new());
    let toolset = bound(&Shell::new(), &scripted).toolset().expect("toolset");
    let expected = session(&scripted);
    assert!(Arc::ptr_eq(toolset.exec_command().session(), &expected));
    assert!(Arc::ptr_eq(
        toolset.write_stdin().expect("write_stdin").session(),
        &expected
    ));
}

// `test_configure_tools_can_customize_shell_approvals_after_clone`
#[tokio::test]
async fn a_configurator_sets_per_call_approval_on_both_tools() {
    let shell = Shell::new().with_configure_tools(|toolset: &mut ShellToolSet| {
        toolset
            .exec_command_mut()
            .set_needs_approval(NeedsApproval::check(|context: &ToolContext<'_>| {
                Ok(context.arguments()["cmd"]
                    .as_str()
                    .is_some_and(|cmd| cmd.starts_with("rm ")))
            }));
        toolset
            .write_stdin_mut()
            .expect("write_stdin")
            .set_needs_approval(NeedsApproval::check(|context: &ToolContext<'_>| {
                Ok(context.arguments()["chars"] == "\u{3}")
            }));
    });
    // The reference clones before binding; the Rust capability is cloned the same way.
    let scripted = ScriptedSession::new(true, Vec::new());
    let tools = bound(&shell.clone(), &scripted).try_tools().expect("tools");

    let run = run_context();
    let call_id = CallId::new("call");
    let asks = |tool: &Arc<dyn Tool>, arguments: Value| {
        let tool = Arc::clone(tool);
        let run = &run;
        let call_id = &call_id;
        async move {
            assert_eq!(tool.options().approval(), ToolApprovalPolicy::Dynamic);
            tool.needs_approval(&ToolContext::new(run, tool.as_ref(), call_id, &arguments))
                .await
                .expect("decision")
        }
    };
    assert!(asks(&tools[0], json!({"cmd": "rm -rf build"})).await);
    assert!(!asks(&tools[0], json!({"cmd": "ls"})).await);
    assert!(asks(&tools[1], json!({"session_id": 1, "chars": "\u{3}"})).await);
    assert!(!asks(&tools[1], json!({"session_id": 1, "chars": "q"})).await);
}

// `test_configure_tools_can_observe_missing_write_stdin_on_non_pty_session`
#[test]
fn a_configurator_sees_that_write_stdin_is_absent_without_terminals() {
    let saw_missing = Arc::new(Mutex::new(false));
    let seen = Arc::clone(&saw_missing);
    let shell = Shell::new().with_configure_tools(move |toolset: &mut ShellToolSet| {
        *seen.lock().unwrap() = toolset.write_stdin().is_none();
    });

    let tools = bound(&shell, &shell_session()).try_tools().expect("tools");

    assert!(*saw_missing.lock().unwrap());
    assert_eq!(names(&tools), ["exec_command"]);
}

// `test_configure_tools_can_replace_exec_command_tool`
#[test]
fn a_configurator_can_replace_exec_command() {
    let shell = Shell::new().with_configure_tools(|toolset: &mut ShellToolSet| {
        let replacement = ExecCommandTool::new(Arc::clone(toolset.exec_command().session()))
            .expect("tool")
            .with_needs_approval(true);
        toolset.set_exec_command(replacement);
    });

    let toolset = bound(&shell, &shell_session()).toolset().expect("toolset");
    let tools = bound(&shell, &shell_session()).try_tools().expect("tools");

    assert!(matches!(
        toolset.exec_command().needs_approval_policy(),
        NeedsApproval::Always
    ));
    // The replacement was built without the bound scope — it is the replacement that is handed out.
    assert_eq!(
        toolset.exec_command().workspace_scope(),
        &SandboxWorkspaceScope::root()
    );
    assert_eq!(tools[0].options().approval(), ToolApprovalPolicy::Always);
}

// `test_configure_tools_receives_workspace_scope`
#[test]
fn a_configurator_sees_the_bound_workspace_scope() {
    let observed = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&observed);
    let shell = Shell::new().with_configure_tools(move |toolset: &mut ShellToolSet| {
        *seen.lock().unwrap() = Some(toolset.workspace_scope().clone());
    });

    let toolset = shell
        .bound(session(&shell_session()), None, scope("tasks/a"))
        .toolset()
        .expect("toolset");

    assert_eq!(observed.lock().unwrap().as_ref(), Some(&scope("tasks/a")));
    assert_eq!(toolset.exec_command().workspace_scope(), &scope("tasks/a"));
}

// `test_instructions_match_sandbox_shell_guidance`
#[tokio::test]
async fn the_instructions_are_the_references_shell_guidance() {
    let section = Shell::new()
        .instructions()
        .await
        .expect("instructions")
        .expect("a section");

    assert_eq!(
        section.content(),
        "When using the shell:\n\
         - Use `exec_command` for shell execution.\n\
         - If available, use `write_stdin` to interact with or poll running sessions.\n\
         - To interrupt a long-running process via `write_stdin`, start it with `tty=true` and \
         send Ctrl-C (`\\u0003`).\n\
         - Prefer `rg` and `rg --files` for text/file discovery when available.\n\
         - Avoid using Python scripts just to print large file chunks."
    );
    assert_eq!(section.content(), SHELL_INSTRUCTIONS);
}

// `test_exec_command_tool_runs_commands_with_source_output_format`
#[tokio::test]
async fn exec_command_reports_in_the_references_format() {
    let scripted = shell_session();
    let tools = bound(&Shell::new(), &scripted).try_tools().expect("tools");

    let output = invoke(
        tools[0].as_ref(),
        &exec_args(&ExecCommandArgs::new("pwd").with_yield_time_ms(1500)),
    )
    .await
    .expect("output");

    let calls = exec_calls(&scripted);
    assert_eq!(calls[0].command(), ["pwd"]);
    assert_eq!(calls[0].timeout_s(), Some(1.5));
    assert_eq!(calls[0].shell(), &ShellInvocation::Login);
    assert_eq!(
        after_stamp(&output),
        "Process exited with code 7\nOutput:\nstdout: pwd\nstderr: pwd"
    );
}

// `test_exec_command_tool_runs_as_bound_user`
#[tokio::test]
async fn exec_command_runs_as_the_bound_user_from_the_bound_directory() {
    let scripted = exec_session(Ok(ExecResult::new(Vec::new(), Vec::new(), 0)));
    let tools = Shell::new()
        .bound(
            session(&scripted),
            Some(User::new("sandbox-user")),
            scope("tasks/a"),
        )
        .try_tools()
        .expect("tools");

    invoke(tools[0].as_ref(), &exec_args(&ExecCommandArgs::new("pwd")))
        .await
        .expect("output");

    let calls = exec_calls(&scripted);
    assert_eq!(calls[0].command(), ["cd /workspace/tasks/a && pwd"]);
    assert_eq!(calls[0].user().cloned(), Some(User::new("sandbox-user")));
    scripted.assert_complete();
}

// `test_exec_command_tool_includes_original_token_count_when_truncating`
#[tokio::test]
async fn exec_command_reports_the_original_token_count_when_it_cuts() {
    let scripted = shell_session();
    let tools = bound(&Shell::new(), &scripted).try_tools().expect("tools");

    let output = invoke(
        tools[0].as_ref(),
        &exec_args(
            &ExecCommandArgs::new("pwd")
                .with_yield_time_ms(1500)
                .with_max_output_tokens(2),
        ),
    )
    .await
    .expect("output");

    assert_eq!(
        after_stamp(&output),
        "Process exited with code 7\nOriginal token count: 6\nOutput:\n…6 tok"
    );
}

// `test_exec_command_tool_wraps_workdir_and_uses_custom_shell`
#[tokio::test]
async fn exec_command_changes_into_the_workdir_and_uses_the_named_shell() {
    let scripted = shell_session();
    let tools = bound(&Shell::new(), &scripted).try_tools().expect("tools");

    let output = invoke(
        tools[0].as_ref(),
        &exec_args(
            &ExecCommandArgs::new("pwd")
                .with_workdir("src/project")
                .with_shell("/bin/bash")
                .with_login(false),
        ),
    )
    .await
    .expect("output");

    let calls = exec_calls(&scripted);
    assert_eq!(calls[0].command(), ["cd /workspace/src/project && pwd"]);
    assert_eq!(calls[0].timeout_s(), Some(10.0));
    assert_eq!(
        calls[0].shell(),
        &ShellInvocation::Prefix(vec!["/bin/bash".to_owned(), "-c".to_owned()])
    );
    assert_eq!(
        after_stamp(&output),
        "Process exited with code 7\nOutput:\n\
         stdout: cd /workspace/src/project && pwd\n\
         stderr: cd /workspace/src/project && pwd"
    );
}

// `test_exec_command_tool_defaults_to_workspace_scope_cwd`, all three parameters.
#[tokio::test]
async fn a_blank_workdir_means_the_turns_working_directory() {
    for workdir in [None, Some(""), Some("   ")] {
        let scripted = shell_session();
        let tools = Shell::new()
            .bound(session(&scripted), None, scope("tasks/a"))
            .try_tools()
            .expect("tools");
        let mut args = ExecCommandArgs::new("pwd");
        if let Some(workdir) = workdir {
            args = args.with_workdir(workdir);
        }

        invoke(tools[0].as_ref(), &exec_args(&args))
            .await
            .expect("output");

        assert_eq!(
            exec_calls(&scripted)[0].command(),
            ["cd /workspace/tasks/a && pwd"],
            "{workdir:?}"
        );
    }
}

// `test_exec_command_tool_resolves_relative_workdir_from_workspace_scope`
#[tokio::test]
async fn a_relative_workdir_is_measured_from_the_turns_working_directory() {
    let scripted = shell_session();
    let tools = Shell::new()
        .bound(session(&scripted), None, scope("tasks/a"))
        .try_tools()
        .expect("tools");

    invoke(
        tools[0].as_ref(),
        &exec_args(&ExecCommandArgs::new("pwd").with_workdir("src/project")),
    )
    .await
    .expect("output");

    assert_eq!(
        exec_calls(&scripted)[0].command(),
        ["cd /workspace/tasks/a/src/project && pwd"]
    );
}

// `test_exec_command_tool_normalizes_raw_backslashes_before_workspace_scope`
#[tokio::test]
async fn backslashes_in_a_workdir_are_read_as_separators() {
    let scripted = shell_session();
    let tools = Shell::new()
        .bound(session(&scripted), None, scope("tasks/a"))
        .try_tools()
        .expect("tools");

    invoke(
        tools[0].as_ref(),
        &exec_args(&ExecCommandArgs::new("pwd").with_workdir("src\\project")),
    )
    .await
    .expect("output");

    assert_eq!(
        exec_calls(&scripted)[0].command(),
        ["cd /workspace/tasks/a/src/project && pwd"]
    );
}

// `test_exec_command_tool_allows_split_path_grant_workdir`
#[tokio::test]
async fn a_workdir_under_a_path_grant_is_allowed() {
    let grant = SandboxPathGrant::new("/mnt/shared-data")
        .expect("grant")
        .with_host_path("/native/shared-data")
        .expect("host path")
        .read_only(true);
    let scripted = ScriptedSession::with_manifest(
        false,
        vec![Answer::ExecEcho],
        Manifest::new()
            .with_root("/workspace")
            .with_path_grant(grant),
    );
    let tools = Shell::new()
        .bound(session(&scripted), None, scope("tasks/a"))
        .try_tools()
        .expect("tools");

    let output = invoke(
        tools[0].as_ref(),
        &exec_args(
            &ExecCommandArgs::new("pwd")
                .with_workdir("/mnt/shared-data")
                .with_shell("/bin/bash")
                .with_login(false),
        ),
    )
    .await
    .expect("output");

    let calls = exec_calls(&scripted);
    assert_eq!(calls[0].command(), ["cd /mnt/shared-data && pwd"]);
    assert_eq!(calls[0].timeout_s(), Some(10.0));
    assert_eq!(
        calls[0].shell(),
        &ShellInvocation::Prefix(vec!["/bin/bash".to_owned(), "-c".to_owned()])
    );
    assert_eq!(
        after_stamp(&output),
        "Process exited with code 7\nOutput:\n\
         stdout: cd /mnt/shared-data && pwd\n\
         stderr: cd /mnt/shared-data && pwd"
    );
}

// `test_exec_command_tool_uses_pty_when_supported`
#[tokio::test]
async fn exec_command_starts_interactively_where_the_session_offers_terminals() {
    let scripted = ScriptedSession::new(true, vec![Answer::PtyStart(Ok(running(1337)))]);
    let tools = Shell::new()
        .bound(session(&scripted), None, scope("tasks/a"))
        .try_tools()
        .expect("tools");

    let output = invoke(
        tools[0].as_ref(),
        &exec_args(
            &ExecCommandArgs::new("pwd")
                .with_yield_time_ms(0)
                .with_tty(true),
        ),
    )
    .await
    .expect("output");

    let starts = pty_starts(&scripted);
    assert_eq!(starts[0].command(), ["cd /workspace/tasks/a && pwd"]);
    assert_eq!(starts[0].yield_time_s(), Some(0.0));
    assert!(starts[0].tty());
    assert_eq!(
        after_stamp(&output),
        "Process running with session ID 1337\nOutput:\n"
    );
}

// `test_exec_command_tool_starts_pty_as_bound_user`
#[tokio::test]
async fn an_interactive_start_runs_as_the_bound_user() {
    let scripted = ScriptedSession::new(true, vec![Answer::PtyStart(Ok(running(1337)))]);
    let tools = Shell::new()
        .bound(
            session(&scripted),
            Some(User::new("sandbox-user")),
            SandboxWorkspaceScope::root(),
        )
        .try_tools()
        .expect("tools");

    invoke(
        tools[0].as_ref(),
        &exec_args(
            &ExecCommandArgs::new("pwd")
                .with_yield_time_ms(0)
                .with_tty(true),
        ),
    )
    .await
    .expect("output");

    assert_eq!(
        pty_starts(&scripted)[0].user().cloned(),
        Some(User::new("sandbox-user"))
    );
}

// `test_exec_command_tool_formats_timeout_without_exit_code`
#[tokio::test]
async fn a_timeout_is_a_response_without_an_exit_code() {
    let scripted = exec_session(Err(SandboxError::exec_timeout(
        vec!["sleep 30".to_owned()],
        Some(0.005),
    )));
    let tool = ExecCommandTool::new(session(&scripted)).expect("tool");

    let output = invoke(
        &tool,
        &exec_args(&ExecCommandArgs::new("sleep 30").with_yield_time_ms(5)),
    )
    .await
    .expect("output");

    assert_eq!(
        after_stamp(&output),
        "Output:\nCommand timed out after 0.005 seconds."
    );
}

// `test_exec_command_tool_falls_back_to_one_shot_exec_after_startup_transport_error`
#[tokio::test]
async fn a_retry_safe_transport_failure_falls_back_to_a_one_shot_command() {
    let scripted = ScriptedSession::new(
        true,
        vec![
            Answer::PtyStart(Err(transport_error(Some(true), false))),
            Answer::Exec(Ok(ExecResult::new(b"fallback ok".to_vec(), Vec::new(), 0))),
        ],
    );
    let tool = ExecCommandTool::new(session(&scripted))
        .expect("tool")
        .with_workspace_scope(scope("tasks/a"));

    let output = invoke(&tool, &exec_args(&ExecCommandArgs::new("pwd")))
        .await
        .expect("output");

    assert!(output.contains("PTY transport failed before the interactive session opened"));
    assert!(output.contains("Process exited with code 0"));
    assert!(!output.contains("Process running with session ID"));
    assert!(output.contains("fallback ok"));
    assert_eq!(
        after_stamp(&output),
        "Process exited with code 0\nOutput:\n\
         PTY transport failed before the interactive session opened; fell back to one-shot exec.\n\
         fallback ok"
    );
    let calls = scripted.calls();
    assert!(
        matches!(&calls[0], Call::PtyStart(request) if request.command() == ["cd /workspace/tasks/a && pwd"])
    );
    assert!(
        matches!(&calls[1], Call::Exec(request) if request.command() == ["cd /workspace/tasks/a && pwd"])
    );
}

// `test_exec_command_tool_does_not_fall_back_for_tty_sessions`
#[tokio::test]
async fn a_terminal_start_does_not_fall_back() {
    let scripted = ScriptedSession::new(
        true,
        vec![Answer::PtyStart(Err(transport_error(Some(true), true)))],
    );
    let tool = ExecCommandTool::new(session(&scripted)).expect("tool");

    let error = invoke(
        &tool,
        &exec_args(&ExecCommandArgs::new("pwd").with_tty(true)),
    )
    .await
    .expect_err("no fallback");

    assert_transport_failure(&error);
    assert_eq!(scripted.calls().len(), 1);
}

// `test_exec_command_tool_does_not_fall_back_for_non_retry_safe_transport_errors`
#[tokio::test]
async fn a_transport_failure_not_marked_retry_safe_does_not_fall_back() {
    let scripted = ScriptedSession::new(
        true,
        vec![Answer::PtyStart(Err(transport_error(None, false)))],
    );
    let tool = ExecCommandTool::new(session(&scripted)).expect("tool");

    let error = invoke(&tool, &exec_args(&ExecCommandArgs::new("pwd")))
        .await
        .expect_err("no fallback");

    assert_transport_failure(&error);
    assert_eq!(scripted.calls().len(), 1);
}

fn assert_transport_failure(error: &Error) {
    assert!(matches!(
        error,
        Error::Tool {
            kind: ToolErrorKind::ExecutionFailed,
            ..
        }
    ));
    let source = std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<SandboxError>())
        .expect("the session's failure is the source");
    assert_eq!(source.error_code(), ErrorCode::ExecTransportError);
}

// `test_exec_command_tool_uses_stdout_only_when_stderr_is_empty`
#[tokio::test]
async fn only_stdout_is_shown_when_stderr_is_empty() {
    let tool = ExecCommandTool::new(session(&exec_session(Ok(ExecResult::new(
        b"stdout only\n".to_vec(),
        Vec::new(),
        7,
    )))))
    .expect("tool");

    let output = invoke(&tool, &exec_args(&ExecCommandArgs::new("pwd")))
        .await
        .expect("output");

    assert_eq!(
        after_stamp(&output),
        "Process exited with code 7\nOutput:\nstdout only\n"
    );
}

// `test_exec_command_tool_uses_stderr_only_when_stdout_is_empty`
#[tokio::test]
async fn only_stderr_is_shown_when_stdout_is_empty() {
    let tool = ExecCommandTool::new(session(&exec_session(Ok(ExecResult::new(
        Vec::new(),
        b"stderr only\n".to_vec(),
        7,
    )))))
    .expect("tool");

    let output = invoke(&tool, &exec_args(&ExecCommandArgs::new("pwd")))
        .await
        .expect("output");

    assert_eq!(
        after_stamp(&output),
        "Process exited with code 7\nOutput:\nstderr only\n"
    );
}

// `test_exec_command_tool_does_not_insert_extra_newline_when_stdout_already_has_one`
#[tokio::test]
async fn no_extra_newline_is_added_after_stdout_that_ends_with_one() {
    let tool = ExecCommandTool::new(session(&exec_session(Ok(ExecResult::new(
        b"stdout line\n".to_vec(),
        b"stderr line\n".to_vec(),
        7,
    )))))
    .expect("tool");

    let output = invoke(&tool, &exec_args(&ExecCommandArgs::new("pwd")))
        .await
        .expect("output");

    assert_eq!(
        after_stamp(&output),
        "Process exited with code 7\nOutput:\nstdout line\nstderr line\n"
    );
}

// `test_write_stdin_tool_writes_and_finishes_session`
#[tokio::test]
async fn write_stdin_reports_a_process_that_finished() {
    let scripted = ScriptedSession::new(
        true,
        vec![Answer::PtyWrite(Ok(PtyExecUpdate::finished(
            b"hello".to_vec(),
            0,
        )))],
    );
    let tool = WriteStdinTool::new(session(&scripted)).expect("tool");

    let output = invoke(
        &tool,
        &stdin_args(&WriteStdinArgs::new(1337).with_chars("hello")),
    )
    .await
    .expect("output");

    assert_eq!(
        after_stamp(&output),
        "Process exited with code 0\nOutput:\nhello"
    );
    let calls = scripted.calls();
    let Call::PtyWrite(request) = &calls[0] else {
        panic!("expected a write");
    };
    assert_eq!(request.process_id(), PtyProcessId::new(1337));
    assert_eq!(request.chars(), "hello");
    assert_eq!(request.yield_time_s(), Some(0.25));
}

// `test_write_stdin_tool_rejects_non_pty_sessions`
#[tokio::test]
async fn write_stdin_fails_on_a_session_without_terminals() {
    let tool = WriteStdinTool::new(session(&shell_session())).expect("tool");

    let error = invoke(&tool, &stdin_args(&WriteStdinArgs::new(1337)))
        .await
        .expect_err("no terminals");

    assert!(
        error
            .to_string()
            .contains("write_stdin is not available for non-PTY sandboxes"),
        "{error}"
    );
}

// `test_write_stdin_tool_formats_unknown_session_error`
#[tokio::test]
async fn an_unknown_process_is_a_response_with_exit_code_one() {
    let scripted = ScriptedSession::new(
        true,
        vec![Answer::PtyWrite(Err(SandboxError::pty_session_not_found(
            9999,
        )))],
    );
    let tool = WriteStdinTool::new(session(&scripted)).expect("tool");

    let output = invoke(&tool, &stdin_args(&WriteStdinArgs::new(9999)))
        .await
        .expect("output");

    assert_eq!(
        after_stamp(&output),
        "Process exited with code 1\nOutput:\nwrite_stdin failed: PTY session not found: 9999"
    );
}

// `test_write_stdin_tool_formats_missing_stdin_error`
#[tokio::test]
async fn input_to_a_process_without_a_terminal_is_a_response_with_exit_code_one() {
    let scripted = ScriptedSession::new(
        true,
        vec![Answer::PtyWrite(Err(SandboxError::pty_stdin_unavailable(
            1337,
        )))],
    );
    let tool = WriteStdinTool::new(session(&scripted)).expect("tool");

    let output = invoke(&tool, &stdin_args(&WriteStdinArgs::new(1337)))
        .await
        .expect("output");

    assert_eq!(
        after_stamp(&output),
        "Process exited with code 1\nOutput:\n\
         stdin is not available for this process. Start the command with `tty=true` in \
         `exec_command` before using `write_stdin`."
    );
}

// `test_write_stdin_tool_reraises_unexpected_runtime_error`
#[tokio::test]
async fn any_other_write_failure_is_an_error() {
    let unexpected = SandboxError::exec_transport(Vec::new(), Some("unexpected stdin failure"));
    let scripted = ScriptedSession::new(true, vec![Answer::PtyWrite(Err(unexpected))]);
    let tool = WriteStdinTool::new(session(&scripted)).expect("tool");

    let error = invoke(&tool, &stdin_args(&WriteStdinArgs::new(1337)))
        .await
        .expect_err("error");

    assert!(
        error.to_string().contains("unexpected stdin failure"),
        "{error}"
    );
}

// ---- beyond the upstream file --------------------------------------------------------------

/// Binding to a sandbox is what gives the capability its tools; the family is the reference's.
#[test]
fn binding_to_a_sandbox_session_yields_a_bound_shell() {
    let scripted = ScriptedSession::new(true, Vec::new());
    let binding = SandboxBinding::new(
        session(&scripted),
        Some(User::new("agent")),
        scope("tasks/a"),
        Manifest::new().with_root("/workspace"),
    );

    let shell = Shell::new();
    assert_eq!(shell.kind(), CapabilityFamily::SHELL);
    let bound = shell
        .bind_sandbox(&binding)
        .expect("bind")
        .expect("a bound copy");

    assert_eq!(names(&bound.tools()), ["exec_command", "write_stdin"]);
    assert!(!shell.is_bound());
}

/// On the run-configuration route an unbound shell is refused with the reference's error, rather
/// than contributing nothing.
#[test]
fn installing_an_unbound_shell_on_a_run_is_refused() {
    let error = Shell::new().bind(&run_context()).err().expect("unbound");
    assert!(error.to_string().contains("not bound to a SandboxSession"));
}

/// Both tools run concurrently, as the reference's function tools do, and ask no approval by
/// default.
#[test]
fn the_tools_are_parallel_and_need_no_approval_by_default() {
    let scripted = ScriptedSession::new(true, Vec::new());
    for tool in bound(&Shell::new(), &scripted).try_tools().expect("tools") {
        let options = tool.options();
        assert_eq!(options.approval(), ToolApprovalPolicy::Never);
        assert_eq!(options.concurrency(), ToolConcurrency::Parallel);
    }
}

/// The advertised schemas carry the reference's fields, defaults, bounds and required set, and are
/// not strict — the reference passes `strict_json_schema=False`.
#[test]
fn the_schemas_carry_the_references_defaults_and_bounds() {
    let scripted = ScriptedSession::new(true, Vec::new());
    let tools = bound(&Shell::new(), &scripted).try_tools().expect("tools");

    let exec = tools[0].schema();
    assert!(!exec.strict_json_schema());
    assert_eq!(
        exec.description(),
        Some("Runs a command in a PTY, returning output or a session ID for ongoing interaction.")
    );
    let properties = &exec.input_schema()["properties"];
    assert_eq!(exec.input_schema()["required"], json!(["cmd"]));
    assert_eq!(properties["cmd"]["minLength"], 1);
    assert_eq!(properties["login"]["default"], true);
    assert_eq!(properties["tty"]["default"], false);
    assert_eq!(properties["yield_time_ms"]["default"], 10_000);
    assert_eq!(properties["yield_time_ms"]["minimum"].as_f64(), Some(0.0));
    assert_eq!(
        properties["max_output_tokens"]["minimum"].as_f64(),
        Some(1.0)
    );
    assert_eq!(
        properties["cmd"]["description"],
        "Shell command to execute."
    );

    let stdin = tools[1].schema();
    assert!(!stdin.strict_json_schema());
    assert_eq!(
        stdin.description(),
        Some("Writes characters to an existing unified exec session and returns recent output.")
    );
    let properties = &stdin.input_schema()["properties"];
    assert_eq!(stdin.input_schema()["required"], json!(["session_id"]));
    assert_eq!(properties["session_id"]["type"], "integer");
    assert_eq!(properties["chars"]["default"], "");
    assert_eq!(properties["yield_time_ms"]["default"], 250);
    assert_eq!(
        properties["max_output_tokens"]["minimum"].as_f64(),
        Some(1.0)
    );
}

/// Arguments the model leaves out take the reference's defaults, and unknown ones are ignored.
#[tokio::test]
async fn omitted_arguments_take_the_references_defaults() {
    let scripted = ScriptedSession::new(true, vec![Answer::PtyStart(Ok(running(4242)))]);
    let tool = ExecCommandTool::new(session(&scripted)).expect("tool");

    invoke(&tool, &json!({"cmd": "top", "unexpected": 1}))
        .await
        .expect("output");

    let start = &pty_starts(&scripted)[0];
    assert_eq!(start.shell(), &ShellInvocation::Login);
    assert!(!start.tty());
    assert_eq!(start.yield_time_s(), Some(10.0));
    assert_eq!(start.max_output_tokens(), None);
}

/// The two bounds the schema states are enforced when the call runs.
#[tokio::test]
async fn arguments_outside_their_bounds_are_refused() {
    let tool = ExecCommandTool::new(session(&shell_session())).expect("tool");
    for arguments in [
        json!({"cmd": ""}),
        json!({"cmd": "pwd", "max_output_tokens": 0}),
    ] {
        let error = invoke(&tool, &arguments).await.expect_err("refused");
        assert!(
            matches!(
                error,
                Error::Tool {
                    kind: ToolErrorKind::InvalidInput,
                    ..
                }
            ),
            "{error}"
        );
    }
}

/// A timeout on the interactive path is a response too, as the reference catches it around both.
#[tokio::test]
async fn an_interactive_timeout_is_a_response() {
    let scripted = ScriptedSession::new(
        true,
        vec![Answer::PtyStart(Err(SandboxError::exec_timeout(
            vec!["pwd".to_owned()],
            Some(1.5),
        )))],
    );
    let tool = ExecCommandTool::new(session(&scripted)).expect("tool");

    let output = invoke(
        &tool,
        &exec_args(&ExecCommandArgs::new("pwd").with_yield_time_ms(1500)),
    )
    .await
    .expect("output");

    assert_eq!(
        after_stamp(&output),
        "Output:\nCommand timed out after 1.500 seconds."
    );
}

/// A predicate over the context is an approval check without a type of its own.
#[test]
fn a_closure_is_an_approval_check() {
    fn is_check(_: &dyn ApprovalCheck) {}
    is_check(&|_: &ToolContext<'_>| Ok(true));
}
