//! The coding host's tool calls as the items of a turn, as Codex's own tools announce them: a
//! command `exec_command` runs is a command execution, a patch `apply_patch` applies is a file
//! change, and the other tools show no item, as a Codex function tool with no item of its own
//! shows none.
//!
//! # Differences from Codex
//!
//! Command boundaries use the executor's typed start and exit events, as Codex does. Exit
//! events include a bounded output snapshot, exit code and duration; a yielded command stays in
//! progress until its exit watcher reports completion, even after its turn ends. Old rollouts
//! without these events fall back to the model-visible result, which cannot recover every field.
//! Codex parses command lines into reads, listings and searches; this adapter retains one
//! unparsed command.
//!
//! - A patch's changes are the files its text names. A created file's diff is its content, and a
//!   changed file's is the patch's own hunks rather than a unified diff against the file; a deleted
//!   file's content is not known, and its diff is empty.
//! - A call that failed is a failed item. Codex tells a call the user declined from one that
//!   failed; the result here does not say which.

use std::path::{Path, PathBuf};

use ra_core::{
    event::{HostEvent, HostEventBody, exec::ExecEvent},
    item::{ToolCall, ToolCallOutput},
    tool::ToolOutput,
};
use ra_patch::{PatchAction, PatchHunk, parse_patch};
use ra_protocol::server::{
    ThreadToolItemEvent, ThreadToolItems,
    protocol::{
        CommandAction, CommandExecutionItem, CommandExecutionStatus, FileUpdateChange,
        PatchApplyStatus, PatchChangeKind, ThreadItem,
    },
};
use serde_json::Value;

const EXEC_COMMAND: &str = "exec_command";
const APPLY_PATCH: &str = "apply_patch";

/// The items the coding host's tools show, for the commands it runs in `workspace_root`.
#[derive(Debug, Clone)]
pub struct CodingThreadToolItems {
    workspace_root: PathBuf,
}

impl CodingThreadToolItems {
    /// The items of the tools of a host whose commands start in `workspace_root`.
    #[must_use]
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
        }
    }

    /// The command line `call` runs and the directory it runs in.
    fn command(&self, call: &ToolCall) -> Option<(String, String)> {
        let command = call.arguments().get("cmd")?.as_str()?.to_owned();
        let cwd = match call.arguments().get("workdir").and_then(Value::as_str) {
            Some(workdir) => self.workspace_root.join(workdir),
            None => self.workspace_root.clone(),
        };
        Some((command, cwd.to_string_lossy().into_owned()))
    }

    fn command_started(&self, call: &ToolCall) -> Option<ThreadItem> {
        let (command, cwd) = self.command(call)?;
        Some(
            CommandExecutionItem::new(
                call.call_id().as_str(),
                command.clone(),
                cwd,
                CommandExecutionStatus::InProgress,
            )
            .with_command_actions(vec![CommandAction::unknown(command)])
            .into(),
        )
    }

    fn command_completed(&self, call: &ToolCall, output: &ToolCallOutput) -> Option<ThreadItem> {
        let (command, cwd) = self.command(call)?;
        let (text, guidance) = output_text(output);
        let result = CommandResult::read(&text, &guidance);
        let status = if output.is_error() {
            CommandExecutionStatus::Failed
        } else {
            result.status
        };
        let mut item =
            CommandExecutionItem::new(call.call_id().as_str(), command.clone(), cwd, status)
                .with_command_actions(vec![CommandAction::unknown(command)])
                .with_aggregated_output(result.output);
        if let Some(session_id) = result.running_session {
            item = item.with_process_id(session_id);
        }
        if let Some(exit_code) = result.exit_code {
            item = item.with_exit_code(exit_code);
        }
        Some(item.into())
    }
}

impl ThreadToolItems for CodingThreadToolItems {
    fn started(&self, call: &ToolCall) -> Option<ThreadItem> {
        match call.name() {
            EXEC_COMMAND => self.command_started(call),
            APPLY_PATCH => patch_item(call, PatchApplyStatus::InProgress),
            _ => None,
        }
    }

    fn event(&self, event: &HostEvent, items: &[ThreadItem]) -> Option<ThreadToolItemEvent> {
        let HostEventBody::Exec(event) = event.body() else {
            return None;
        };
        match event {
            ExecEvent::Started(start) => {
                let call_id = start.call_id()?;
                let mut item = items
                    .iter()
                    .find(|item| item.id() == call_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        CommandExecutionItem::new(
                            call_id,
                            start.command(),
                            start.cwd().unwrap_or_default(),
                            CommandExecutionStatus::InProgress,
                        )
                        .with_command_actions(vec![CommandAction::unknown(start.command())])
                        .into()
                    });
                let ThreadItem::CommandExecution { process_id, .. } = &mut item else {
                    return None;
                };
                *process_id = Some(start.session_id().as_str().to_owned());
                Some(ThreadToolItemEvent::Started(item))
            }
            ExecEvent::Exited(end) => {
                let call_id = end.call_id()?;
                let mut item = items.iter().find(|item| item.id() == call_id)?.clone();
                let ThreadItem::CommandExecution {
                    status,
                    exit_code,
                    duration_ms,
                    aggregated_output,
                    ..
                } = &mut item
                else {
                    return None;
                };
                *status = if end.exit_code() == Some(0) {
                    CommandExecutionStatus::Completed
                } else {
                    CommandExecutionStatus::Failed
                };
                *exit_code = end.exit_code();
                *duration_ms = Some(i64::try_from(end.duration_ms()).unwrap_or(i64::MAX));
                *aggregated_output = end.aggregated_output().map(str::to_owned);
                Some(ThreadToolItemEvent::Completed(item))
            }
            _ => None,
        }
    }

    fn completed(&self, call: &ToolCall, output: &ToolCallOutput) -> Option<ThreadItem> {
        match call.name() {
            EXEC_COMMAND => self.command_completed(call, output).filter(|item| {
                !matches!(
                    item,
                    ThreadItem::CommandExecution {
                        status: CommandExecutionStatus::InProgress,
                        ..
                    }
                )
            }),
            APPLY_PATCH => patch_item(
                call,
                if output.is_error() {
                    PatchApplyStatus::Failed
                } else {
                    PatchApplyStatus::Completed
                },
            ),
            _ => None,
        }
    }
}

/// The text of a tool's result as the model read it, and the guidance that came with it.
fn output_text(output: &ToolCallOutput) -> (String, Vec<String>) {
    match ToolOutput::from_stored(output.output()) {
        Ok(Some(stored)) => (
            stored.as_text().map(str::to_owned).unwrap_or_default(),
            stored.metadata().guidance().to_vec(),
        ),
        _ => match output.output() {
            Value::String(text) => (text.clone(), Vec::new()),
            other => (other.to_string(), Vec::new()),
        },
    }
}

/// What an `exec_command` result says about its command.
#[derive(Debug, PartialEq, Eq)]
struct CommandResult {
    status: CommandExecutionStatus,
    exit_code: Option<i32>,
    running_session: Option<String>,
    output: String,
}

impl CommandResult {
    /// Reads the result `exec_command` renders.
    ///
    /// A command that finished within its wait is answered with what it printed, or with
    /// `Command exited with status <code> and produced no output.` (`Command finished …` when it
    /// left no status); a status other than 0 is also given as the guidance
    /// `Command exited with status <code>.`. A command still running when the wait ran out is
    /// answered with a first line ending in `` Session `<id>` is still running. `` and, after a
    /// blank line, what it printed so far.
    fn read(text: &str, guidance: &[String]) -> Self {
        let (headline, rest) = text.split_once("\n\n").unwrap_or((text, ""));
        if let Some(session_id) = running_session(headline) {
            return Self {
                status: CommandExecutionStatus::InProgress,
                exit_code: None,
                running_session: Some(session_id.to_owned()),
                output: rest.to_owned(),
            };
        }
        let silent = text
            .strip_prefix("Command exited with status ")
            .and_then(|rest| rest.strip_suffix(" and produced no output."))
            .and_then(|code| code.parse().ok());
        let stated = guidance.iter().find_map(|guidance| {
            guidance
                .strip_prefix("Command exited with status ")
                .and_then(|rest| rest.strip_suffix('.'))
                .and_then(|code| code.parse().ok())
        });
        let exit_code = silent.or(stated);
        let output = if silent.is_some() || text == "Command finished and produced no output." {
            String::new()
        } else {
            text.to_owned()
        };
        Self {
            status: if exit_code.is_some_and(|code| code != 0) {
                CommandExecutionStatus::Failed
            } else {
                CommandExecutionStatus::Completed
            },
            exit_code,
            running_session: None,
            output,
        }
    }
}

/// The session a result's first line says is still running, if it says so.
fn running_session(headline: &str) -> Option<&str> {
    let standing = &headline[headline.rfind("Session `")?..];
    let (session_id, phrase) = standing.strip_prefix("Session `")?.split_once('`')?;
    phrase
        .trim_start()
        .starts_with("is still running")
        .then_some(session_id)
}

/// The patch `call` applies, as a file change in `status`, or `None` if its text cannot be read.
fn patch_item(call: &ToolCall, status: PatchApplyStatus) -> Option<ThreadItem> {
    let patch = match call.arguments() {
        Value::String(patch) => patch.as_str(),
        arguments => arguments.get("patch")?.as_str()?,
    };
    let plan = parse_patch(patch).ok()?;
    let mut changes: Vec<FileUpdateChange> = Vec::new();
    for action in plan.actions() {
        match action {
            PatchAction::AddFile { path, content } => {
                changes.push(FileUpdateChange::new(
                    display(path),
                    PatchChangeKind::Add,
                    content.clone(),
                ));
            }
            PatchAction::DeleteFile { path } => {
                changes.push(FileUpdateChange::new(
                    display(path),
                    PatchChangeKind::Delete,
                    "",
                ));
            }
            PatchAction::UpdateFile { path, hunks } => {
                changes.push(FileUpdateChange::new(
                    display(path),
                    PatchChangeKind::update(None),
                    hunks_diff(hunks),
                ));
            }
            PatchAction::MoveFile { from, to } => {
                // The parser gives a moved file's changes first and its move after them.
                let from = display(from);
                let diff = match changes.last() {
                    Some(last)
                        if last.path() == from
                            && matches!(last.kind(), PatchChangeKind::Update { .. }) =>
                    {
                        changes
                            .pop()
                            .map(|last| last.diff().to_owned())
                            .unwrap_or_default()
                    }
                    _ => String::new(),
                };
                changes.push(FileUpdateChange::new(
                    from,
                    PatchChangeKind::update(Some(to.clone())),
                    diff,
                ));
            }
            _ => {}
        }
    }
    Some(ThreadItem::file_change(
        call.call_id().as_str(),
        changes,
        status,
    ))
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A changed file's hunks as the patch gives them: context lines, then the lines it removes and
/// the lines it adds, each under the anchor it is found by.
fn hunks_diff(hunks: &[PatchHunk]) -> String {
    let mut diff = String::new();
    for hunk in hunks {
        let anchors = hunk.context_anchors().join(" ");
        diff.push_str("@@");
        if !anchors.is_empty() {
            diff.push(' ');
            diff.push_str(&anchors);
        }
        diff.push('\n');
        let lines = hunk
            .context_before()
            .iter()
            .map(|line| (' ', line))
            .chain(hunk.removed_lines().iter().map(|line| ('-', line)))
            .chain(hunk.added_lines().iter().map(|line| ('+', line)))
            .chain(hunk.context_after().iter().map(|line| (' ', line)));
        for (prefix, line) in lines {
            diff.push(prefix);
            diff.push_str(line);
            diff.push('\n');
        }
    }
    diff
}
