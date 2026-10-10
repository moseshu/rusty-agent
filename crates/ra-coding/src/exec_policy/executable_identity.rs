//! Judging an unfamiliar shell executable on its own: Codex's
//! `core/src/exec_policy/executable_identity.rs`.
//!
//! A script's words describe what the shell is asked to run, and an executable that is not the
//! configured shell or a system shell may ignore them. So such an executable is judged as a command
//! of its own beside the commands its script appears to run: the script can add restrictions, but
//! cannot grant the executable trust. Codex's Windows and zsh-fork cases are not ported.

use std::path::Path;

use super::{
    ExecApprovalRequest, ExecApprovalRequirement, ExecPolicyManager, commands_for_exec_policy,
};

impl ExecPolicyManager {
    /// Decides a command the coding shell tool runs with `configured_shell` as its default shell.
    #[must_use]
    pub fn create_exec_approval_requirement_for_shell(
        &self,
        mut request: ExecApprovalRequest<'_>,
        configured_shell: &Path,
    ) -> ExecApprovalRequirement {
        let command = request.command;
        let executable = shell_approval_command(command, configured_shell);
        if executable.len() == command.len() {
            return self.create_exec_approval_requirement_for_command(request);
        }

        let mut policy_commands = commands_for_exec_policy(command);

        // Evaluate the executable alongside its apparent commands. Inner
        // commands can add restrictions, but cannot grant the executable trust.
        policy_commands.insert(0, executable.to_vec());
        request.command = executable;
        self.create_exec_approval_requirement_for_parsed_commands(request, &policy_commands)
    }
}

fn shell_approval_command<'a>(command: &'a [String], configured_shell: &Path) -> &'a [String] {
    let Some(executable) = command.first() else {
        return command;
    };
    let executable_path = Path::new(executable);

    let is_system_shell = executable_path
        .parent()
        .is_some_and(|parent| parent == Path::new("/bin") || parent == Path::new("/usr/bin"));
    let is_configured_shell = executable_path == configured_shell;

    if is_configured_shell || is_system_shell {
        command
    } else {
        // An unfamiliar executable can ignore its arguments, so evaluate the
        // executable separately from any restrictions on its apparent command.
        std::slice::from_ref(executable)
    }
}
