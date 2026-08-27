//! Structured detection of potentially destructive coding actions.
//!
//! This is product policy, not a provider-neutral permission concept: it knows that this product
//! runs shell snippets and applies V4A patches. It produces facts for an approval policy to use;
//! it does not infer user intent, grant permission, or enforce a sandbox boundary. In particular,
//! every shell judgement begins with a Bash syntax tree, so a dangerous-looking word in a quoted
//! argument is data, not a command.
//!
//! Three properties decide whether the facts are worth showing to a human:
//!
//! - **Silence on safe input.** A detector that fires on `> /dev/null` or on an appending `>>`
//!   trains the reader to approve without looking, which is worse than not detecting at all.
//! - **No invented paths.** A syntax tree is not an execution trace. A `cd` the shell may skip
//!   keeps both the old and the new directory in play, and a path that escapes under either is
//!   reported; a change that names no directory at all says so with
//!   [`DangerousAction::UnknownWorkingDirectory`] rather than resolving later paths against a base
//!   the detector already knows to be wrong.
//! - **Arguments read as arguments.** Options that consume a following value, clustered short
//!   flags, and git's pre-subcommand options all change which word is the program or the target,
//!   so they are modelled rather than approximated by a prefix test.

use std::{
    ffi::OsStr,
    io,
    num::NonZeroUsize,
    path::{Component, Path, PathBuf},
};

use ra_patch::{PatchAction, PatchPlan};
use tree_sitter::{Node, Parser};

/// The default number of independently targeted deletions that is considered broad.
pub const DEFAULT_LARGE_DELETE_THRESHOLD: NonZeroUsize = match NonZeroUsize::new(3) {
    Some(threshold) => threshold,
    None => unreachable!(),
};

/// How many nested `sh -c` scripts are inspected before the detector stops descending.
const MAX_NESTED_SCRIPT_DEPTH: usize = 3;

/// A byte range in the original shell source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceRange {
    start: usize,
    end: usize,
}

impl SourceRange {
    /// Creates a range from source byte offsets.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Inclusive start byte offset.
    #[must_use]
    pub const fn start(&self) -> usize {
        self.start
    }

    /// Exclusive end byte offset.
    #[must_use]
    pub const fn end(&self) -> usize {
        self.end
    }
}

/// Why a shell command requires careful review.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DangerousShellCommand {
    /// The command requests elevated operating-system privileges.
    PrivilegeEscalation,
    /// The command can erase, format, or otherwise replace a filesystem.
    FilesystemFormat,
    /// The command can write directly to a device.
    RawDeviceWrite,
    /// The command can terminate processes beyond the current tool invocation.
    ProcessTermination,
    /// The command can suspend, reboot, or power off the machine.
    SystemPowerControl,
    /// The command can remove files or directories.
    FileRemoval,
    /// The command can discard version-control state or untracked workspace files.
    VersionControlDestruction,
}

/// How a broad deletion was expressed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BroadDeletionKind {
    /// An `rm`, `rmdir`, or `unlink` command named a broad or many-target scope.
    ShellRemoval,
    /// A `find` command used its `-delete` action.
    FindDelete,
    /// `git clean` was asked to remove workspace content recursively.
    GitClean,
    /// A patch contains many `DeleteFile` actions.
    PatchDelete,
}

/// One structured fact found while examining a proposed action.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DangerousAction {
    /// The shell source did not parse completely, so structural inspection cannot claim it is safe.
    UnparseableShell,
    /// A directory change left the working directory of every later relative path unknown.
    ///
    /// Relative paths after this point are not resolved at all. Reporting them against a base the
    /// detector knows to be wrong would name a path that is not the one at risk.
    UnknownWorkingDirectory {
        /// Location of the directory-changing command.
        source: SourceRange,
    },
    /// A recognized AST command has a potentially destructive effect.
    DangerousShellCommand {
        /// The command program after syntactic wrappers such as `sudo` are considered.
        program: String,
        /// The command's effect class.
        kind: DangerousShellCommand,
        /// Location of the command in the shell source.
        source: SourceRange,
    },
    /// A shell redirection or a known output operand would replace an existing file.
    OverwriteExistingFile {
        /// Canonical-or-lexically-resolved target path.
        path: PathBuf,
        /// Location of the producing shell expression, when it came from a shell command.
        source: Option<SourceRange>,
    },
    /// A write or deletion target resolves outside the configured workspace.
    WriteOutsideWorkspace {
        /// Canonical-or-lexically-resolved target path.
        path: PathBuf,
        /// Location of the producing shell expression, when it came from a shell command.
        source: Option<SourceRange>,
    },
    /// A deletion can affect an unbounded or policy-sized set of paths.
    BroadDeletion {
        /// Structural form that makes the deletion broad.
        kind: BroadDeletionKind,
        /// The statically known paths, if the shell expression had any.
        targets: Vec<PathBuf>,
        /// Location of the deleting shell command, if applicable.
        source: Option<SourceRange>,
    },
}

/// The ordered findings from one proposed shell command or patch plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DangerousActionReport {
    findings: Vec<DangerousAction>,
}

impl DangerousActionReport {
    /// All structured facts found during inspection, in source order for shell input.
    #[must_use]
    pub fn findings(&self) -> &[DangerousAction] {
        &self.findings
    }

    /// Whether inspection found no action that needs special review.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }

    fn push(&mut self, finding: DangerousAction) {
        self.findings.push(finding);
    }
}

/// The working directory a shell expression runs in, as far as static inspection can tell.
///
/// A syntax tree is not an execution trace: `false && cd subdir` reads as a directory change but
/// runs as nothing at all. Rather than pick one reading, a change that may or may not have happened
/// keeps both directories alive, and a path that escapes the workspace under **any** of them is
/// reported. Over-reporting a reachable path is recoverable; going quiet about one is not.
#[derive(Debug, Clone)]
enum ShellCwd {
    /// Relative paths resolve against one of these directories. Never empty.
    Candidates(Vec<PathBuf>),
    /// No directory can be named at all, so relative paths are not resolved.
    Unknown,
}

impl ShellCwd {
    fn one(directory: PathBuf) -> Self {
        Self::Candidates(vec![directory])
    }

    /// Every directory a relative path could resolve against, or `None` when none is known.
    fn resolve(&self, raw: &str) -> Option<Vec<PathBuf>> {
        let requested = Path::new(raw);
        if requested.is_absolute() {
            return Some(vec![resolve_path(Path::new(""), requested)]);
        }
        match self {
            Self::Candidates(bases) => {
                let mut resolved: Vec<PathBuf> = bases
                    .iter()
                    .map(|base| resolve_path(base, requested))
                    .collect();
                resolved.dedup();
                Some(resolved)
            }
            Self::Unknown => None,
        }
    }

    fn candidates(&self) -> &[PathBuf] {
        match self {
            Self::Candidates(bases) => bases,
            Self::Unknown => &[],
        }
    }
}

/// Deterministic dangerous-action detector bound to one workspace root.
///
/// The root is canonicalized once at construction. A path is resolved component by component and
/// each existing component is canonicalized as it is appended, so a symbolic link that leaves the
/// workspace is caught even when the final file does not exist yet, and a `..` after such a link
/// climbs from the link's target rather than from its lexical parent. This is evidence for
/// approval, not an enforcement mechanism: a sandbox remains responsible for the check-to-use
/// boundary.
#[derive(Debug, Clone)]
pub struct DangerousActionDetector {
    workspace_root: PathBuf,
    large_delete_threshold: NonZeroUsize,
}

impl DangerousActionDetector {
    /// Opens a detector rooted at an existing workspace directory.
    pub fn new(workspace_root: impl AsRef<Path>) -> io::Result<Self> {
        let workspace_root = std::fs::canonicalize(workspace_root)?;
        Ok(Self {
            workspace_root,
            large_delete_threshold: DEFAULT_LARGE_DELETE_THRESHOLD,
        })
    }

    /// Changes how many independent deletion targets constitute a broad deletion.
    #[must_use]
    pub const fn with_large_delete_threshold(mut self, threshold: NonZeroUsize) -> Self {
        self.large_delete_threshold = threshold;
        self
    }

    /// Canonical workspace root used for all path comparisons.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Number of independent deletion targets that constitutes a broad deletion.
    #[must_use]
    pub const fn large_delete_threshold(&self) -> NonZeroUsize {
        self.large_delete_threshold
    }

    /// Inspects shell source using the Bash syntax tree.
    ///
    /// `working_directory` is interpreted relative to the workspace root when it is relative.
    /// A caller that already resolved an absolute working directory may pass it directly.
    ///
    /// A `cd` that runs on every pass and names an existing directory is followed. One that runs
    /// only on some passes — the right operand of `&&`, a branch body, a subshell — leaves both
    /// directories in play, so a later relative path is checked against each. One that names no
    /// directory at all, including a loop body, reports
    /// [`DangerousAction::UnknownWorkingDirectory`] and stops resolving relative paths. Dynamic
    /// expansions are not guessed as paths, but a deletion whose target is an expansion is treated
    /// as broad rather than dropped.
    #[must_use]
    pub fn inspect_shell(
        &self,
        source: &str,
        working_directory: Option<&Path>,
    ) -> DangerousActionReport {
        let cwd = self.resolve_working_directory(working_directory);
        let mut report = DangerousActionReport::default();
        self.inspect_shell_source(source, &cwd, MAX_NESTED_SCRIPT_DEPTH, &mut report);
        report
    }

    /// Inspects a parsed V4A plan before it reaches the filesystem.
    #[must_use]
    pub fn inspect_patch(&self, plan: &PatchPlan) -> DangerousActionReport {
        let mut report = DangerousActionReport::default();
        let mut deleted = Vec::new();

        for action in plan.actions() {
            match action {
                // An update rewrites a file that is already there — that is the whole meaning of
                // the action — so its destination is checked exactly like any other patch output.
                PatchAction::AddFile { path, .. } | PatchAction::UpdateFile { path, .. } => {
                    self.inspect_patch_output(path, &mut report);
                }
                PatchAction::MoveFile { from, to } => {
                    self.inspect_patch_path(from, &mut report);
                    self.inspect_patch_output(to, &mut report);
                }
                PatchAction::DeleteFile { path } => {
                    let resolved = resolve_path(&self.workspace_root, path);
                    self.record_outside_workspace(
                        std::slice::from_ref(&resolved),
                        None,
                        &mut report,
                    );
                    deleted.push(resolved);
                }
                _ => {}
            }
        }

        if deleted.len() >= self.large_delete_threshold.get() {
            report.push(DangerousAction::BroadDeletion {
                kind: BroadDeletionKind::PatchDelete,
                targets: deleted,
                source: None,
            });
        }
        report
    }

    fn inspect_shell_source(
        &self,
        source: &str,
        cwd: &ShellCwd,
        depth: usize,
        report: &mut DangerousActionReport,
    ) {
        let mut parser = Parser::new();
        let language = tree_sitter_bash::LANGUAGE.into();
        if parser.set_language(&language).is_err() {
            report.push(DangerousAction::UnparseableShell);
            return;
        }
        let Some(tree) = parser.parse(source, None) else {
            report.push(DangerousAction::UnparseableShell);
            return;
        };
        if tree.root_node().has_error() {
            report.push(DangerousAction::UnparseableShell);
        }
        self.walk_shell_tree(tree.root_node(), source, cwd, depth, report);
    }

    /// Walks the tree in source order without recursing.
    ///
    /// The traversal is iterative on purpose: tree depth is bounded only by input length, and a
    /// recursive walk turns a deeply nested snippet into a stack overflow, which aborts the process
    /// rather than raising a catchable error.
    fn walk_shell_tree(
        &self,
        root: Node<'_>,
        source: &str,
        cwd: &ShellCwd,
        depth: usize,
        report: &mut DangerousActionReport,
    ) {
        let mut cwd = cwd.clone();
        let mut cursor = root.walk();
        let mut level = 0_usize;
        loop {
            let node = cursor.node();
            match node.kind() {
                "command" => self.inspect_shell_command(node, source, &mut cwd, depth, report),
                "file_redirect" => self.inspect_file_redirect(node, source, &cwd, report),
                _ => {}
            }

            if cursor.goto_first_child() {
                level += 1;
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if level == 0 || !cursor.goto_parent() {
                    return;
                }
                level -= 1;
            }
        }
    }

    fn inspect_shell_command(
        &self,
        node: Node<'_>,
        source: &str,
        cwd: &mut ShellCwd,
        depth: usize,
        report: &mut DangerousActionReport,
    ) {
        let Some(name) = node
            .child_by_field_name("name")
            .and_then(|name| static_shell_word(name, source))
        else {
            return;
        };
        let arguments = command_arguments(node, source);
        let location = source_range(node);
        let program = base_program_name(&name);

        if matches!(program, "cd" | "pushd" | "popd") {
            *cwd = directory_change(program, &arguments, node, cwd);
            if matches!(cwd, ShellCwd::Unknown) {
                report.push(DangerousAction::UnknownWorkingDirectory { source: location });
            }
            return;
        }

        // A wrapper such as `sudo` or `xargs` hides the real program inside its own arguments, and
        // wrappers nest (`sudo timeout 5 rm -rf /`), so each unwrapped layer is inspected in turn.
        let mut program = program;
        let mut arguments = arguments.as_slice();
        loop {
            self.inspect_program(program, arguments, location, cwd, report);
            self.inspect_nested_script(program, arguments, location, cwd, depth, report);
            let Some((wrapped, rest)) = unwrap_program(program, arguments) else {
                return;
            };
            program = base_program_name(wrapped);
            arguments = rest;
        }
    }

    fn inspect_program(
        &self,
        program: &str,
        arguments: &[ShellArgument],
        location: SourceRange,
        cwd: &ShellCwd,
        report: &mut DangerousActionReport,
    ) {
        let values = argument_values(arguments);

        if let Some(kind) = dangerous_command_kind(program, &values) {
            report.push(DangerousAction::DangerousShellCommand {
                program: program.to_owned(),
                kind,
                source: location,
            });
        }

        for writable in writable_path_arguments(program, arguments) {
            if is_stream_device(Path::new(writable.path)) {
                continue;
            }
            let Some(resolved) = cwd.resolve(writable.path) else {
                continue;
            };
            self.record_outside_workspace(&resolved, Some(writable.argument.source), report);
            if writable.replaces_existing
                && let Some(existing) = resolved.into_iter().find(|path| path.exists())
            {
                report.push(DangerousAction::OverwriteExistingFile {
                    path: existing,
                    source: Some(writable.argument.source),
                });
            }
        }

        self.inspect_deletion(program, arguments, location, cwd, report);

        if program == "find" && values.contains(&"-delete") {
            report.push(DangerousAction::BroadDeletion {
                kind: BroadDeletionKind::FindDelete,
                targets: Vec::new(),
                source: Some(location),
            });
        }
        if program == "git" && is_broad_git_clean(&values) {
            report.push(DangerousAction::BroadDeletion {
                kind: BroadDeletionKind::GitClean,
                targets: cwd.candidates().to_vec(),
                source: Some(location),
            });
        }
    }

    fn inspect_deletion(
        &self,
        program: &str,
        arguments: &[ShellArgument],
        location: SourceRange,
        cwd: &ShellCwd,
        report: &mut DangerousActionReport,
    ) {
        let targets = deletion_targets(program, arguments);
        if targets.is_empty() {
            return;
        }

        let mut broad = targets.len() >= self.large_delete_threshold.get();
        let mut resolved_targets = Vec::with_capacity(targets.len());
        for target in &targets {
            let raw = target.value.as_deref();
            let resolved = raw.and_then(|raw| cwd.resolve(raw));
            if let Some(resolved) = &resolved {
                self.record_outside_workspace(resolved, Some(target.source), report);
            }
            if self.is_broad_deletion_target(raw, resolved.as_deref()) {
                broad = true;
            }
            if let Some(resolved) = resolved {
                resolved_targets.extend(resolved);
            }
        }

        if broad {
            report.push(DangerousAction::BroadDeletion {
                kind: BroadDeletionKind::ShellRemoval,
                targets: resolved_targets,
                source: Some(location),
            });
        }
    }

    /// Inspects the script operand of `sh -c` so an indirect command is not invisible.
    fn inspect_nested_script(
        &self,
        program: &str,
        arguments: &[ShellArgument],
        location: SourceRange,
        cwd: &ShellCwd,
        depth: usize,
        report: &mut DangerousActionReport,
    ) {
        if !matches!(program, "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash") {
            return;
        }
        let Some(index) = arguments.iter().position(|argument| {
            argument
                .value
                .as_deref()
                .is_some_and(|value| is_short_flag_cluster(value) && value.contains('c'))
        }) else {
            return;
        };
        let Some(operand) = arguments.get(index + 1) else {
            return;
        };

        // Either the script is not statically readable or the nesting is deeper than the detector
        // follows. Both mean the same thing: structural inspection cannot claim this is safe.
        let (Some(script), true) = (operand.value.as_deref(), depth > 0) else {
            report.push(DangerousAction::UnparseableShell);
            return;
        };

        let mut nested = DangerousActionReport::default();
        self.inspect_shell_source(script, cwd, depth - 1, &mut nested);
        for finding in nested.findings {
            // Offsets inside the operand do not survive quote removal, so the findings are anchored
            // to the command that carries the script.
            report.push(relocate(finding, location));
        }
    }

    fn inspect_file_redirect(
        &self,
        node: Node<'_>,
        source: &str,
        cwd: &ShellCwd,
        report: &mut DangerousActionReport,
    ) {
        let Some(destination) = node.child_by_field_name("destination") else {
            return;
        };
        let Some(raw_path) = static_shell_word(destination, source) else {
            return;
        };
        let Some(prefix) = source.get(node.start_byte()..destination.start_byte()) else {
            return;
        };
        if !is_replacing_file_redirect(prefix) || is_stream_device(Path::new(&raw_path)) {
            return;
        }

        let location = source_range(node);
        let Some(resolved) = cwd.resolve(&raw_path) else {
            return;
        };
        self.record_outside_workspace(&resolved, Some(location), report);
        if let Some(existing) = resolved.into_iter().find(|path| path.exists()) {
            report.push(DangerousAction::OverwriteExistingFile {
                path: existing,
                source: Some(location),
            });
        }
    }

    fn inspect_patch_path(&self, path: &Path, report: &mut DangerousActionReport) {
        let resolved = resolve_path(&self.workspace_root, path);
        self.record_outside_workspace(std::slice::from_ref(&resolved), None, report);
    }

    fn inspect_patch_output(&self, path: &Path, report: &mut DangerousActionReport) {
        let resolved = resolve_path(&self.workspace_root, path);
        self.record_outside_workspace(std::slice::from_ref(&resolved), None, report);
        if resolved.exists() {
            report.push(DangerousAction::OverwriteExistingFile {
                path: resolved,
                source: None,
            });
        }
    }

    /// Reports the first candidate that leaves the workspace, if any does.
    ///
    /// One finding per argument, not one per candidate: the reader needs to know that this argument
    /// can escape, and a named path it can actually reach is the evidence for that.
    fn record_outside_workspace(
        &self,
        paths: &[PathBuf],
        source: Option<SourceRange>,
        report: &mut DangerousActionReport,
    ) {
        if let Some(escaping) = paths
            .iter()
            .find(|path| !path.starts_with(&self.workspace_root))
        {
            report.push(DangerousAction::WriteOutsideWorkspace {
                path: escaping.clone(),
                source,
            });
        }
    }

    fn is_broad_deletion_target(&self, raw: Option<&str>, resolved: Option<&[PathBuf]>) -> bool {
        // An expansion the detector cannot read is the case where the impact is least bounded, so
        // it counts as broad rather than being dropped for lack of a path.
        let Some(raw) = raw else {
            return true;
        };
        if raw.contains(['*', '?', '[', '{', '$', '`']) {
            return true;
        }
        let Some(resolved) = resolved else {
            return true;
        };
        resolved
            .iter()
            .any(|path| *path == self.workspace_root || path.parent().is_none())
    }

    fn resolve_working_directory(&self, requested: Option<&Path>) -> ShellCwd {
        ShellCwd::one(requested.map_or_else(
            || self.workspace_root.clone(),
            |requested| resolve_path(&self.workspace_root, requested),
        ))
    }
}

#[derive(Debug, Clone)]
struct ShellArgument {
    value: Option<String>,
    source: SourceRange,
}

struct WritablePath<'a> {
    argument: &'a ShellArgument,
    path: &'a str,
    replaces_existing: bool,
}

/// A program that runs another program named in its own arguments.
///
/// `value_options` is the contract that makes the unwrap correct: an option missing from it is read
/// as taking no value, and the wrapper's next word — `0` in `stdbuf -o 0 rm -rf /` — is then
/// mistaken for the program, which hides everything the real command does. Adding a wrapper means
/// listing every one of its value-taking options.
struct CommandWrapper {
    /// Options that consume the following argument as their value.
    value_options: &'static [&'static str],
    /// Operands that precede the wrapped program, such as `timeout`'s duration.
    leading_operands: usize,
}

const SUDO_VALUE_OPTIONS: &[&str] = &[
    "-a",
    "-C",
    "-D",
    "-g",
    "-h",
    "-p",
    "-R",
    "-r",
    "-t",
    "-T",
    "-U",
    "-u",
    "--auth-type",
    "--close-from",
    "--chdir",
    "--chroot",
    "--group",
    "--host",
    "--prompt",
    "--role",
    "--type",
    "--command-timeout",
    "--other-user",
    "--user",
];
const XARGS_VALUE_OPTIONS: &[&str] = &[
    "-a",
    "-d",
    "-E",
    "-I",
    "-i",
    "-L",
    "-l",
    "-n",
    "-P",
    "-s",
    "--arg-file",
    "--delimiter",
    "--eof",
    "--replace",
    "--max-lines",
    "--max-args",
    "--max-procs",
    "--max-chars",
];

fn command_wrapper(program: &str) -> Option<CommandWrapper> {
    let wrapper = match program {
        "sudo" => CommandWrapper {
            value_options: SUDO_VALUE_OPTIONS,
            leading_operands: 0,
        },
        "doas" => CommandWrapper {
            value_options: &["-a", "-C", "-u"],
            leading_operands: 0,
        },
        "env" => CommandWrapper {
            value_options: &["-C", "-S", "-u", "--chdir", "--split-string", "--unset"],
            leading_operands: 0,
        },
        "timeout" => CommandWrapper {
            value_options: &["-k", "-s", "--kill-after", "--signal"],
            leading_operands: 1,
        },
        "nice" => CommandWrapper {
            value_options: &["-n", "--adjustment"],
            leading_operands: 0,
        },
        "xargs" => CommandWrapper {
            value_options: XARGS_VALUE_OPTIONS,
            leading_operands: 0,
        },
        "stdbuf" => CommandWrapper {
            value_options: &["-e", "-i", "-o", "--error", "--input", "--output"],
            leading_operands: 0,
        },
        "command" | "nohup" => CommandWrapper {
            value_options: &[],
            leading_operands: 0,
        },
        _ => return None,
    };
    Some(wrapper)
}

fn command_arguments(node: Node<'_>, source: &str) -> Vec<ShellArgument> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| {
            !matches!(
                child.kind(),
                "command_name" | "file_redirect" | "herestring_redirect" | "variable_assignment"
            )
        })
        .map(|child| ShellArgument {
            value: static_shell_word(child, source),
            source: source_range(child),
        })
        .collect()
}

fn static_shell_word(node: Node<'_>, source: &str) -> Option<String> {
    if contains_dynamic_shell_syntax(node) {
        return None;
    }
    let text = node.utf8_text(source.as_bytes()).ok()?.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .or_else(|| {
            text.strip_prefix('\'')
                .and_then(|text| text.strip_suffix('\''))
        })
        .unwrap_or(text);
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// Whether the subtree contains syntax whose value is only known at run time.
///
/// Iterative for the same reason as [`DangerousActionDetector::walk_shell_tree`]: the subtree is as
/// deep as the input allows, and a stack overflow here would abort the process.
fn contains_dynamic_shell_syntax(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let mut level = 0_usize;
    loop {
        if matches!(
            cursor.node().kind(),
            "command_substitution"
                | "process_substitution"
                | "expansion"
                | "simple_expansion"
                | "brace_expression"
                | "glob"
                | "extglob_pattern"
        ) {
            return true;
        }
        if cursor.goto_first_child() {
            level += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if level == 0 || !cursor.goto_parent() {
                return false;
            }
            level -= 1;
        }
    }
}

/// How often a command in the syntax tree actually runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionReach {
    /// Reached on every run, so it replaces the working directory outright.
    Always,
    /// Reached on some runs only, or in a scope of its own; both directories stay possible.
    Sometimes,
    /// Reached an unknown number of times, so no directory can be named afterwards.
    Repeated,
}

/// The number of directories kept alive before the detector gives up naming any of them.
const MAX_CWD_CANDIDATES: usize = 8;

/// Classifies a command by how the shell reaches it, using the tree rather than a prefix test.
///
/// The rule is an allow-list, so an unfamiliar construct is conditional rather than certain: only a
/// top-level statement, the first operand of a `&&`/`||` list, and a redirected statement's body run
/// on every pass. A loop body runs an unknown number of times, which no single directory describes.
fn execution_reach(node: Node<'_>) -> ExecutionReach {
    let mut current = node;
    let mut conditional = false;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "program" => break,
            "while_statement" | "until_statement" | "for_statement" | "c_style_for_statement" => {
                return ExecutionReach::Repeated;
            }
            // The first operand of a list runs unconditionally; a redirect does not gate its body.
            "list" | "redirected_statement" => {
                if parent.named_child(0).map(|first| first.id()) != Some(current.id()) {
                    conditional = true;
                }
            }
            // A branch body, a pipeline stage, a subshell, a function body, or anything unfamiliar.
            _ => conditional = true,
        }
        current = parent;
    }
    if conditional {
        ExecutionReach::Sometimes
    } else {
        ExecutionReach::Always
    }
}

/// Decides the working directory after a `cd`, `pushd`, or `popd`.
///
/// **A change that fails is not a change that is unknown.** `cd nowhere` and `cd some-file` both
/// leave the shell exactly where it was, so the old directory has to stay in play — dropping it
/// would stop resolving later relative paths and lose the escape in `cd nowhere; touch ../out`,
/// which is the case the shell makes most certain rather than least. So a destination that exists
/// and is not a directory can never be entered and changes nothing; a destination that does not
/// exist yet may be created before the command runs, so both readings are kept.
///
/// The shell moves for certain only when the command always runs and every destination it could
/// name is already a directory. `popd` and `cd -` need a stack this does not keep, and an operand
/// it cannot read names nothing at all; those are the cases where no directory can be named.
fn directory_change(
    program: &str,
    arguments: &[ShellArgument],
    node: Node<'_>,
    cwd: &ShellCwd,
) -> ShellCwd {
    let reach = execution_reach(node);
    if program == "popd" || reach == ExecutionReach::Repeated {
        return ShellCwd::Unknown;
    }
    let positional = positional_arguments(arguments);
    let [target] = positional.as_slice() else {
        return ShellCwd::Unknown;
    };
    let Some(raw) = target.value.as_deref() else {
        return ShellCwd::Unknown;
    };
    if raw == "-" {
        return ShellCwd::Unknown;
    }
    let Some(targets) = cwd.resolve(raw) else {
        return ShellCwd::Unknown;
    };
    let reachable: Vec<PathBuf> = targets
        .into_iter()
        .filter(|path| path.is_dir() || !path.exists())
        .collect();
    if reachable.is_empty() {
        return cwd.clone();
    }

    if reach == ExecutionReach::Always && reachable.iter().all(|path| path.is_dir()) {
        return ShellCwd::Candidates(reachable);
    }
    let mut candidates = cwd.candidates().to_vec();
    for path in reachable {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    if candidates.len() > MAX_CWD_CANDIDATES {
        ShellCwd::Unknown
    } else {
        ShellCwd::Candidates(candidates)
    }
}

fn source_range(node: Node<'_>) -> SourceRange {
    SourceRange::new(node.start_byte(), node.end_byte())
}

fn base_program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(program)
}

/// Anchors a finding from a nested script to the command that carries it.
fn relocate(finding: DangerousAction, location: SourceRange) -> DangerousAction {
    match finding {
        DangerousAction::UnparseableShell => DangerousAction::UnparseableShell,
        DangerousAction::UnknownWorkingDirectory { .. } => {
            DangerousAction::UnknownWorkingDirectory { source: location }
        }
        DangerousAction::DangerousShellCommand { program, kind, .. } => {
            DangerousAction::DangerousShellCommand {
                program,
                kind,
                source: location,
            }
        }
        DangerousAction::OverwriteExistingFile { path, .. } => {
            DangerousAction::OverwriteExistingFile {
                path,
                source: Some(location),
            }
        }
        DangerousAction::WriteOutsideWorkspace { path, .. } => {
            DangerousAction::WriteOutsideWorkspace {
                path,
                source: Some(location),
            }
        }
        DangerousAction::BroadDeletion { kind, targets, .. } => DangerousAction::BroadDeletion {
            kind,
            targets,
            source: Some(location),
        },
    }
}

/// Finds the program a wrapper runs, skipping the wrapper's own options and their values.
///
/// A dynamic argument ends the search: it could be an option, an option's value, or the program
/// itself, and picking one of those readings would be a guess presented as a fact.
fn unwrap_program<'a>(
    program: &str,
    arguments: &'a [ShellArgument],
) -> Option<(&'a str, &'a [ShellArgument])> {
    let wrapper = command_wrapper(program)?;
    let mut operands = 0_usize;
    let mut index = 0_usize;
    while index < arguments.len() {
        let value = arguments[index].value.as_deref()?;
        if value == "--" {
            index += 1;
        } else if value.starts_with('-') {
            index += usize::from(wrapper.value_options.contains(&value)) + 1;
        } else if value.contains('=') {
            // An `env`-style assignment, never the program.
            index += 1;
        } else if operands < wrapper.leading_operands {
            operands += 1;
            index += 1;
        } else {
            return Some((value, arguments.get(index + 1..)?));
        }
    }
    None
}

fn dangerous_command_kind(program: &str, arguments: &[&str]) -> Option<DangerousShellCommand> {
    match program {
        "sudo" | "doas" | "su" => Some(DangerousShellCommand::PrivilegeEscalation),
        command if command.starts_with("mkfs") => Some(DangerousShellCommand::FilesystemFormat),
        "dd" if arguments
            .iter()
            .any(|argument| argument.starts_with("of=/dev/")) =>
        {
            Some(DangerousShellCommand::RawDeviceWrite)
        }
        "kill" | "pkill" | "killall" => Some(DangerousShellCommand::ProcessTermination),
        "shutdown" | "reboot" | "halt" | "poweroff" => {
            Some(DangerousShellCommand::SystemPowerControl)
        }
        "rm" | "rmdir" | "unlink" => Some(DangerousShellCommand::FileRemoval),
        "git" if is_destructive_git(arguments) => {
            Some(DangerousShellCommand::VersionControlDestruction)
        }
        _ => None,
    }
}

fn writable(argument: &ShellArgument, replaces_existing: bool) -> Option<WritablePath<'_>> {
    Some(WritablePath {
        argument,
        path: argument.value.as_deref()?,
        replaces_existing,
    })
}

fn writable_path_arguments<'a>(
    program: &str,
    arguments: &'a [ShellArgument],
) -> Vec<WritablePath<'a>> {
    let values = argument_values(arguments);
    let positional = positional_arguments(arguments);
    let last_positional = |replaces_existing: bool| {
        positional
            .last()
            .copied()
            .and_then(|argument| writable(argument, replaces_existing))
            .into_iter()
            .collect()
    };

    match program {
        "cp" | "mv" => {
            let clobbers = !(has_short_flag(&values, 'n') || values.contains(&"--no-clobber"));
            last_positional(clobbers)
        }
        "install" => last_positional(true),
        // `ln` refuses an existing target unless it is forced.
        "ln" => last_positional(has_short_flag(&values, 'f') || values.contains(&"--force")),
        "tee" => {
            let appends = has_short_flag(&values, 'a') || values.contains(&"--append");
            positional
                .iter()
                .filter_map(|argument| writable(argument, !appends))
                .collect()
        }
        "touch" | "mkdir" => positional
            .iter()
            .filter_map(|argument| writable(argument, false))
            .collect(),
        "dd" => arguments
            .iter()
            .filter_map(|argument| {
                let path = argument.value.as_deref()?.strip_prefix("of=")?;
                Some(WritablePath {
                    argument,
                    path,
                    replaces_existing: true,
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn deletion_targets<'a>(program: &str, arguments: &'a [ShellArgument]) -> Vec<&'a ShellArgument> {
    if matches!(program, "rm" | "rmdir" | "unlink") {
        positional_arguments(arguments)
    } else {
        Vec::new()
    }
}

/// The operands of a command, with an unreadable argument counted as one.
///
/// An expansion could be an option or a path. Treating it as an operand keeps it visible to the
/// deletion check, which reports it as broad rather than assuming it away.
fn positional_arguments(arguments: &[ShellArgument]) -> Vec<&ShellArgument> {
    let mut after_separator = false;
    arguments
        .iter()
        .filter(|argument| match argument.value.as_deref() {
            Some("--") => {
                after_separator = true;
                false
            }
            Some(value) => after_separator || !value.starts_with('-'),
            None => true,
        })
        .collect()
}

fn argument_values(arguments: &[ShellArgument]) -> Vec<&str> {
    arguments
        .iter()
        .filter_map(|argument| argument.value.as_deref())
        .collect()
}

fn is_short_flag_cluster(argument: &str) -> bool {
    argument.starts_with('-') && !argument.starts_with("--") && argument.len() > 1
}

/// Whether any short-flag cluster carries `flag`, so `-fdx` answers for `-d` and `-x` alike.
fn has_short_flag(arguments: &[&str], flag: char) -> bool {
    arguments.iter().any(|argument| {
        is_short_flag_cluster(argument) && argument.chars().skip(1).any(|letter| letter == flag)
    })
}

/// Splits git's pre-subcommand options from the subcommand and its own arguments.
fn git_subcommand<'a>(arguments: &'a [&'a str]) -> Option<(&'a str, &'a [&'a str])> {
    const VALUE_OPTIONS: &[&str] = &[
        "-C",
        "-c",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--exec-path",
        "--config-env",
    ];
    let mut index = 0_usize;
    while index < arguments.len() {
        let argument = arguments[index];
        if argument.starts_with('-') {
            index += usize::from(VALUE_OPTIONS.contains(&argument)) + 1;
            continue;
        }
        return Some((argument, arguments.get(index + 1..)?));
    }
    None
}

fn is_destructive_git(arguments: &[&str]) -> bool {
    let Some((subcommand, options)) = git_subcommand(arguments) else {
        return false;
    };
    match subcommand {
        "clean" => !is_dry_run(options),
        "reset" => options.contains(&"--hard"),
        _ => false,
    }
}

fn is_dry_run(options: &[&str]) -> bool {
    has_short_flag(options, 'n') || options.contains(&"--dry-run")
}

fn is_broad_git_clean(arguments: &[&str]) -> bool {
    let Some((subcommand, options)) = git_subcommand(arguments) else {
        return false;
    };
    if subcommand != "clean" || is_dry_run(options) {
        return false;
    }
    ['d', 'x', 'X']
        .into_iter()
        .any(|flag| has_short_flag(options, flag))
}

/// Devices that are streams rather than files: writing to one replaces nothing.
///
/// A block device is deliberately absent — `> /dev/sda` destroys a disk and must stay visible.
const STREAM_DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/stdin",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/tty",
    "/dev/console",
];

fn is_stream_device(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|path| STREAM_DEVICES.contains(&path))
        || path.starts_with("/dev/fd")
}

/// Whether a redirect operator truncates its destination.
///
/// `>>` appends and `>&` duplicates a descriptor; neither destroys the destination's contents, and
/// reporting them as an overwrite is the kind of noise that makes an approval prompt worthless.
fn is_replacing_file_redirect(prefix: &str) -> bool {
    let operator = prefix.trim_end();
    let operator = operator.strip_suffix('|').unwrap_or(operator);
    if operator.ends_with(">>") || operator.ends_with(">&") || operator.ends_with("<&") {
        return false;
    }
    operator.ends_with('>')
}

/// Resolves `requested` against `base`, following symbolic links component by component.
///
/// Canonicalizing each existing component as it is appended is what makes a later `..` climb from a
/// link's target instead of its lexical parent, so `linked-outside/../x` cannot re-enter the
/// workspace on paper while leaving it in fact. Components that do not exist yet are kept as
/// written, which is what lets a not-yet-created file still be placed inside or outside the root.
fn resolve_path(base: &Path, requested: &Path) -> PathBuf {
    let mut resolved = if requested.is_absolute() {
        PathBuf::new()
    } else {
        base.to_path_buf()
    };
    for component in requested.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                if let Ok(canonical) = std::fs::canonicalize(&resolved) {
                    resolved = canonical;
                }
            }
        }
    }
    resolved
}
