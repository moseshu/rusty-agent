//! Single-turn settlement.

pub(crate) mod batch;
pub(crate) mod interrupt;
#[doc(hidden)]
pub mod prepare;
#[doc(hidden)]
pub mod process;
pub(crate) mod resolve;
