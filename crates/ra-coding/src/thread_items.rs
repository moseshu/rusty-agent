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
//! A patch's item is driven by the patch tool's begin and end events, as Codex's file change is
//! by `PatchApplyBegin` and `PatchApplyEnd`: a patch that fails verification, or that its policy
//! refuses, never begins and shows no item, and one that begins shows it before it is asked
//! about. A patch whose call ends in a way the tool does not report — a timeout, or a refusal
//! after it began and before it ran — fails with the call's result.
//!
//! - A patch's changes are the files its text names. A created file's diff is its content, and a
//!   changed file's is the patch's own hunks rather than a unified diff against the file; a deleted
//!   file's content is not read, and its diff is empty.
//! - A call whose approval was not given ends as the answer the thread recorded says, as Codex's
//!   handlers end it, rather than by the refusal the model is given.

use std::path::PathBuf;

use ra_core::{
    event::{
        ApprovalEvent, ApprovalResolution, FileEvent, HostEvent, HostEventBody,
        exec::ExecEvent,
        file::{
            FileChangeKind, PatchApplyBeginEvent, PatchApplyEndEvent, PatchApplyStatus as Ended,
        },
    },
    item::{ToolCall, ToolCallOutput},
    tool::ToolOutput,
};
use ra_protocol::server::{
    ThreadToolItemEvent, ThreadToolItems,
    protocol::{
        CommandAction, CommandExecutionItem, CommandExecutionStatus, FileUpdateChange,
        PatchApplyStatus, PatchChangeKind, ThreadItem,
    },
};
use serde_json::Value;

const EXEC_COMMAND: &str = "exec_command";

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
            _ => None,
        }
    }

    fn event(&self, event: &HostEvent, items: &[ThreadItem]) -> Option<ThreadToolItemEvent> {
        if let HostEventBody::Approval(approval) = event.body() {
            return refused_item(approval, items).map(ThreadToolItemEvent::Completed);
        }
        if let HostEventBody::File(event) = event.body() {
            return match event {
                FileEvent::PatchApplyBegin(begin) => {
                    Some(ThreadToolItemEvent::Started(patch_began(begin)))
                }
                FileEvent::PatchApplyEnd(end) => {
                    patch_ended(end, items).map(ThreadToolItemEvent::Completed)
                }
                _ => None,
            };
        }
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
            _ => None,
        }
    }

    /// A patch that began and whose call's result came in without its end: the call failed in a
    /// way the tool did not report. A command left running stays in progress.
    fn unfinished(&self, item: &ThreadItem, _output: &ToolCallOutput) -> Option<ThreadItem> {
        let ThreadItem::FileChange { .. } = item else {
            return None;
        };
        let mut item = item.clone();
        if let ThreadItem::FileChange { status, .. } = &mut item {
            *status = PatchApplyStatus::Failed;
        }
        Some(item)
    }
}

/// The item of a call whose approval was not given, ended as Codex's handlers end it: a command
/// declined or cancelled is `declined` and one whose answer could not be obtained `failed`, with
/// nothing it ran; a patch not approved is `declined` whatever the reason, as Codex's patch tool
/// reports every refusal. An approved call's item goes on as the call runs.
fn refused_item(approval: &ApprovalEvent, items: &[ThreadItem]) -> Option<ThreadItem> {
    let call_id = approval.call_id().as_str();
    let resolution = approval.resolution();
    if resolution == ApprovalResolution::Approved {
        return None;
    }
    let mut item = items.iter().find(|item| item.id() == call_id)?.clone();
    match &mut item {
        ThreadItem::CommandExecution {
            status,
            process_id,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } => {
            *status = if resolution == ApprovalResolution::Failed {
                CommandExecutionStatus::Failed
            } else {
                CommandExecutionStatus::Declined
            };
            *process_id = None;
            *aggregated_output = None;
            *exit_code = None;
            *duration_ms = None;
        }
        ThreadItem::FileChange { status, .. } => *status = PatchApplyStatus::Declined,
        _ => return None,
    }
    Some(item)
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

/// A patch's item as its begin announces it: Codex's `PatchApplyBegin` turned into a file change.
fn patch_began(begin: &PatchApplyBeginEvent) -> ThreadItem {
    let changes = begin
        .changes()
        .iter()
        .map(|change| {
            let kind = match change.change() {
                FileChangeKind::Added => PatchChangeKind::Add,
                FileChangeKind::Deleted => PatchChangeKind::Delete,
                _ => PatchChangeKind::update(change.moved_to().map(PathBuf::from)),
            };
            FileUpdateChange::new(change.path(), kind, change.diff())
        })
        .collect();
    ThreadItem::file_change(
        begin.call_id().as_str(),
        changes,
        PatchApplyStatus::InProgress,
    )
}

/// The item a patch's end leaves: the begun item, in the status it ended with.
fn patch_ended(end: &PatchApplyEndEvent, items: &[ThreadItem]) -> Option<ThreadItem> {
    let mut item = items
        .iter()
        .find(|item| item.id() == end.call_id().as_str())?
        .clone();
    let ThreadItem::FileChange { status, .. } = &mut item else {
        return None;
    };
    *status = match end.status() {
        Ended::Completed => PatchApplyStatus::Completed,
        Ended::Declined => PatchApplyStatus::Declined,
        _ => PatchApplyStatus::Failed,
    };
    Some(item)
}
