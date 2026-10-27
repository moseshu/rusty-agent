// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Identifies the sandbox implementation and overrides selected for a command.

#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use serde::Deserialize;
use serde::Serialize;

/// Controller decision that overrides normal sandbox selection for a command.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SandboxOverride {
    /// Use normal sandbox selection.
    #[default]
    NoOverride,
    /// Broaden approved filesystem access while retaining denied-read enforcement.
    EscalatedSandboxWithRestrictions,
    /// Bypass the sandbox on the first attempt.
    BypassSandboxFirstAttempt,
}

impl SandboxOverride {
    #[must_use]
    pub fn is_no_override(&self) -> bool {
        *self == Self::NoOverride
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SandboxType {
    None,
    MacosSeatbelt,
    LinuxSeccomp,
    WindowsRestrictedToken,
    WindowsMxc,
}

impl SandboxType {
    #[must_use]
    pub fn as_metric_tag(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::MacosSeatbelt => "seatbelt",
            Self::LinuxSeccomp => "seccomp",
            Self::WindowsRestrictedToken => "windows_sandbox",
            Self::WindowsMxc => "windows_mxc",
        }
    }
}

/// Preserves explicit MXC selection while honoring legacy runtime level updates.
#[must_use]
pub fn effective_windows_sandbox_type(
    sandbox_type: SandboxType,
    sandbox_level: WindowsSandboxLevel,
) -> SandboxType {
    match (sandbox_type, sandbox_level) {
        (SandboxType::WindowsMxc, _) => SandboxType::WindowsMxc,
        (_, WindowsSandboxLevel::Disabled) => SandboxType::None,
        (_, WindowsSandboxLevel::RestrictedToken | WindowsSandboxLevel::Elevated) => {
            SandboxType::WindowsRestrictedToken
        }
    }
}

/// The legacy Windows restricted-token sandbox selection.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowsSandboxLevel {
    #[default]
    Disabled,
    RestrictedToken,
    Elevated,
}
/// Whether a Windows sandbox launch reconciles persistent proxy settings.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowsSandboxProxySettingsMode {
    #[default]
    Reconcile,
    Preserve,
}
