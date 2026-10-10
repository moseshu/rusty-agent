//! Commands dangerous enough to ask about even inside a sandbox: the POSIX half of Codex's
//! `shell-command/src/command_safety/is_dangerous_command.rs`.
//!
//! On POSIX Codex flags one thing: an `rm` given its force option, including behind `sudo`, `env`,
//! `trap` and nested shell scripts, read from the literal words of any valid script. Codex's
//! Windows and PowerShell rules are not ported: the executor runs commands on Unix only.

use super::bash::parse_shell_lc_literal_commands;

/// Identifies the dangerous-command rule matched by a command invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DangerousCommandMatch {
    /// An `rm` invocation includes the force option.
    ForcedRm,
    /// Another dangerous-command rule matched.
    Other,
}

const MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH: usize = 8;

/// Returns the dangerous-command rule matched by an already-tokenized command.
#[must_use]
pub fn dangerous_command_match(command: &[String]) -> Option<DangerousCommandMatch> {
    dangerous_command_match_with_depth(command, /*wrapper_depth*/ 0)
}

fn dangerous_command_match_with_depth(
    command: &[String],
    wrapper_depth: usize,
) -> Option<DangerousCommandMatch> {
    if wrapper_depth > MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH {
        return Some(DangerousCommandMatch::Other);
    }

    if let Some(dangerous_match) = dangerous_command_match_for_exec(command, wrapper_depth) {
        return Some(dangerous_match);
    }

    // Support shell scripts where any literal command might be dangerous,
    // including commands nested in control flow or substitutions.
    if let Some(dangerous_match) = parse_shell_lc_literal_commands(command).and_then(|commands| {
        commands
            .iter()
            .find_map(|command| dangerous_command_match_with_depth(command, wrapper_depth + 1))
    }) {
        return Some(dangerous_match);
    }

    None
}

fn executable_name_lookup_key(raw: &str) -> Option<String> {
    raw.rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn dangerous_command_match_for_exec(
    command: &[String],
    wrapper_depth: usize,
) -> Option<DangerousCommandMatch> {
    let cmd0 = command
        .first()
        .and_then(|command| executable_name_lookup_key(command));

    match cmd0.as_deref() {
        Some("rm") if rm_args_include_force_option(&command[1..]) => {
            Some(DangerousCommandMatch::ForcedRm)
        }

        // For sudo <cmd>, simply check <cmd>.
        Some("sudo") => dangerous_command_match_with_depth(&command[1..], wrapper_depth + 1),

        // Skip environment assignments before checking the command run by env.
        Some("env") => dangerous_command_match_for_env(command, wrapper_depth),

        // A trap action is shell source stored in the first operand.
        Some("trap") => dangerous_command_match_for_trap(command, wrapper_depth),

        // ── anything else ─────────────────────────────────────────────────
        _ => None,
    }
}

fn dangerous_command_match_for_env(
    command: &[String],
    wrapper_depth: usize,
) -> Option<DangerousCommandMatch> {
    let mut command_index = 1;
    while let Some(argument) = command.get(command_index) {
        if argument == "--" {
            command_index += 1;
            break;
        }
        if matches!(argument.as_str(), "-i" | "--ignore-environment")
            || argument
                .split_once('=')
                .is_some_and(|(name, _)| !name.is_empty() && !name.starts_with('-'))
        {
            command_index += 1;
            continue;
        }
        break;
    }
    dangerous_command_match_with_depth(&command[command_index..], wrapper_depth + 1)
}

fn dangerous_command_match_for_trap(
    command: &[String],
    wrapper_depth: usize,
) -> Option<DangerousCommandMatch> {
    let mut action_index = 1;
    if command
        .get(action_index)
        .is_some_and(|argument| argument == "--")
    {
        action_index += 1;
    }
    let action = command
        .get(action_index)
        .filter(|action| !action.starts_with('-'))?;
    let shell_command = vec!["sh".to_string(), "-c".to_string(), action.clone()];
    dangerous_command_match_with_depth(&shell_command, wrapper_depth + 1)
}

fn rm_args_include_force_option(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            arg == "--force"
                || arg
                    .strip_prefix('-')
                    .is_some_and(|flags| !flags.starts_with('-') && flags.contains('f'))
        })
}
