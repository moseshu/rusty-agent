//! Shared effect classifications for the built-in tool adapters and coding presets.
//!
//! These are optional adapter conventions; provider-neutral tools may declare other classes
//! or remain unclassified.

/// Observes information without executing an arbitrary operation.
pub const READ: &str = "read";
/// Changes files or other editable content.
pub const EDIT: &str = "edit";
/// Executes an operation whose effects cannot be restricted to observation or editing.
pub const EXECUTE: &str = "execute";
