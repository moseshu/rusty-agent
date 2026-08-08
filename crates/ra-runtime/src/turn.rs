//! Single-turn settlement.

pub(crate) mod batch;
pub(crate) mod interrupt;
#[doc(hidden)]
pub mod prepare;
pub(crate) mod process;
pub(crate) mod resolve;
