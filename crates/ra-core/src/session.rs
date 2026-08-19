//! Authoritative conversation session port and session identity.
//!
//! A [`Session`] is the provider-neutral, storage-layout-independent contract for reading and
//! appending authoritative session records ([`RunItem`](crate::item::RunItem)).
//!
//! It is identified by an opaque [`SessionId`].

pub mod id;
pub mod port;

pub use id::SessionId;
pub use port::Session;
