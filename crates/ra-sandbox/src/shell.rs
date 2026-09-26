//! Quoting an argument so a shell reads it back as one word.
//!
//! The port lives in [`ra_core::sandbox::shell`], where the shell tool can reach it too; this path
//! is kept for the backends that already use it.

pub use ra_core::sandbox::shell::{join, quote};
