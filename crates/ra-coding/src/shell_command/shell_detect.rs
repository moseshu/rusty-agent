//! Recognizing a shell by its path: the detection half of Codex's `shell-command/src/shell_detect.rs`.
//!
//! Codex's lookup of the user's login shell is not ported; the coding host runs commands with the
//! shell a call names or its default.

/// A shell this project recognizes by name.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ShellType {
    /// Zsh.
    Zsh,
    /// Bash.
    Bash,
    /// PowerShell.
    PowerShell,
    /// The POSIX shell.
    Sh,
    /// Windows `cmd`.
    Cmd,
}

impl ShellType {
    /// Its canonical name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::PowerShell => "powershell",
            Self::Sh => "sh",
            Self::Cmd => "cmd",
        }
    }
}

/// The shell a path names, by its file name with any extension removed.
#[must_use]
pub fn detect_shell_type(shell_path: impl AsRef<std::path::Path>) -> Option<ShellType> {
    let shell_path = shell_path.as_ref();
    match shell_path.as_os_str().to_str() {
        Some("zsh") => Some(ShellType::Zsh),
        Some("sh") => Some(ShellType::Sh),
        Some("cmd") => Some(ShellType::Cmd),
        Some("bash") => Some(ShellType::Bash),
        Some("pwsh" | "powershell") => Some(ShellType::PowerShell),
        _ => {
            let shell_name = shell_path.file_stem();
            if let Some(shell_name) = shell_name {
                let shell_name_path = std::path::Path::new(shell_name);
                if shell_name_path != shell_path {
                    return detect_shell_type(shell_name_path);
                }
            }
            None
        }
    }
}
