//! Reading shell commands the way Codex's `shell-command` crate does: splitting a `bash -lc`
//! script into the commands it runs, recognizing shells by path, and flagging dangerous commands.
//! The command decisions in [`crate::exec_policy`] and the approval cache build on these.

pub mod bash;
pub mod dangerous;
pub mod shell_detect;

pub use bash::{
    extract_bash_command, parse_shell_lc_literal_commands, parse_shell_lc_plain_commands,
    parse_shell_script_into_commands,
};
pub use dangerous::{DangerousCommandMatch, dangerous_command_match};
pub use shell_detect::{ShellType, detect_shell_type};
