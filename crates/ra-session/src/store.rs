//! Runtime probing of the required and optional `SessionStore` methods.

#[cfg(feature = "sqlite")]
pub mod local;
pub mod mirror;
pub mod summary;
