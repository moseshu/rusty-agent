//! # `ra-patch`
//!
//! Parsing and application of the V4A patch format.
//!
//! **Boundary**: it handles patch syntax, matching, application, and diff rendering only — not
//! file permissions, approval, sandboxing, or the coding agent's editing discipline. The fuzzy
//! matcher is an implementation detail and stays out of the public API.
//!
//! **Stability**: `Evolving`. The V4A format itself is defined upstream; this parser's API may
//! grow but not shrink.

pub mod apply;
pub(crate) mod fuzz;
pub mod parse;
pub mod render;

pub use apply::CommittedPatchDelta;
pub use parse::{PatchAction, PatchConflict, PatchHunk, PatchMatchLevel, PatchPlan};

/// Default schema version for patch structures.
///
/// The version number is this crate's own — a patch record evolves on its own schedule. The
/// [`SchemaVersion`](ra_core::compat::SchemaVersion) and
/// [`Unknown`](ra_core::compat::Unknown) types carrying it are deliberately **not**: a second
/// pair of types with these names would make "is this record newer than my code, and does it need
/// migrating" a question a caller has to answer once per crate, with `ra_core::compat::Compatibility`
/// reachable for some records and not others. Downgrade reads are one mechanism, so they are one
/// set of types.
pub const PATCH_SCHEMA_VERSION: ra_core::compat::SchemaVersion =
    ra_core::compat::SchemaVersion::new(1);
