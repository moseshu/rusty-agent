//! Permission-policy contracts shared by hosts, runtime, and persisted state.
//!
//! This module deliberately defines policy selection rather than tool-specific safety rules.
//! Rule matching, approval collection, sandbox enforcement, and UI wording have different
//! owners and arrive in their respective layers. Keeping the selected mode as a core value lets
//! those layers agree on one policy without making the core depend on any of them.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The host-selected policy for permission evaluation.
///
/// A mode is not itself an authorization decision. A matching rule, an explicit user decision,
/// or an enforcement boundary can still allow or deny a particular action. In particular,
/// [`Self::BypassPermissions`] does not bypass sandbox or host-enforced restrictions.
///
/// The variants align with the established permission-policy vocabulary while keeping their
/// concrete evaluation in the runtime: `Plan` and `AcceptEdits`, for example, require knowledge
/// of the requested action that this provider-neutral value type does not have.
///
/// **Deliberately not ordered.** A derived `Ord` would rank the variants by declaration order,
/// which is not a permissiveness order — [`Self::BypassPermissions`], the loosest mode, would sit
/// between two stricter ones. `mode >= BypassPermissions` and "take the stricter of the two" via
/// `max` would then compile and be wrong on a security policy, and inserting a variant under
/// `#[non_exhaustive]` would silently change every such comparison. Modes are matched, not
/// compared.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Evaluate actions under the host's normal approval policy.
    #[default]
    Default,
    /// Automatically accept edits while retaining approval checks for other actions.
    AcceptEdits,
    /// Automatically approve actions unless an explicit deny rule applies.
    BypassPermissions,
    /// Restrict actions to read-only ones: planning may inspect the workspace but neither modify
    /// it nor execute commands.
    Plan,
    /// Never prompt; actions that require approval are denied.
    DontAsk,
}

impl PermissionMode {
    /// Every mode currently defined by this version of the framework.
    ///
    /// This is not an exhaustive list for downstream matching because the enum may grow.
    pub const ALL: &'static [Self] = &[
        Self::Default,
        Self::AcceptEdits,
        Self::BypassPermissions,
        Self::Plan,
        Self::DontAsk,
    ];

    /// Stable machine-readable identifier used by configuration and control protocols.
    ///
    /// The label matches this type's serde representation and the established public
    /// permission-mode vocabulary.
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
}

impl fmt::Display for PermissionMode {
    /// Renders [`Self::label`], so a mode reads the same in a log line, a trace field, and on the
    /// wire. A derived `Debug` spelling would give the same value a second name.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}
