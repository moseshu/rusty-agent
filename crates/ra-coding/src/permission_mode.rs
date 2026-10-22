//! Coding-product permission modes.

use std::fmt;

use ra_runtime::permission::ActionClassPolicy;
use serde::{Deserialize, Serialize};

/// Permission modes provided by the coding product.
///
/// These modes are product presets over the runtime's optional action-class policy; they are not
/// part of the provider-neutral framework contract.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodingPermissionMode {
    /// Use each tool's ordinary approval behavior.
    #[default]
    Default,
    /// Automatically approve actions classified as edits.
    AcceptEdits,
    /// Automatically approve every tool action at the framework approval layer.
    BypassPermissions,
    /// Permit only actions classified as reads.
    Plan,
    /// Disable framework approval prompts while preserving other policy settings.
    DontAsk,
}

impl CodingPermissionMode {
    /// All supported coding modes in stable declaration order.
    pub const ALL: [Self; 5] = [
        Self::Default,
        Self::AcceptEdits,
        Self::BypassPermissions,
        Self::Plan,
        Self::DontAsk,
    ];

    /// Stable identifier shared with the serialized representation.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
        }
    }

    /// Runtime policy represented by this coding-product preset.
    #[must_use]
    pub fn policy(self) -> ActionClassPolicy {
        match self {
            Self::Default => ActionClassPolicy::new(),
            Self::AcceptEdits => {
                ActionClassPolicy::new().with_auto_allow([ra_tools::action_classes::EDIT])
            }
            Self::BypassPermissions => ActionClassPolicy::new().with_auto_allow_all(true),
            Self::Plan => {
                ActionClassPolicy::new().with_allow_only([ra_tools::action_classes::READ])
            }
            Self::DontAsk => ActionClassPolicy::new().with_prompts(false),
        }
    }
}

impl fmt::Display for CodingPermissionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}
