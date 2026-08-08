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
